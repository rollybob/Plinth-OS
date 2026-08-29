//! The IOMMU remapping-unit seam -- protected-DMA discovery (slice 1).
//!
//! An IOMMU (Intel calls it VT-d, AMD calls it AMD-Vi) sits between a device's
//! DMA engine and physical memory and translates the addresses a device issues
//! through a per-device page table (a "domain"), so a device can physically
//! reach only the frames the kernel put in its domain. That hardware boundary is
//! what will make a library-OS-written virtqueue descriptor safe: the worst a
//! bad descriptor can name is an address outside the domain, which the hardware
//! refuses.
//!
//! This module is the seam that hides the VT-d-vs-AMD-Vi difference from the rest
//! of the kernel, exactly as `irq` hides PIC-vs-APIC behind mask/unmask/eoi. Per
//! the design ruling it is introduced *with* its first backend (VT-d), not
//! speculatively: today it does discovery only. The VT-d backend reads the DMAR
//! table (`acpi::find_dmar`); an AMD-Vi backend would read the IVRS table and
//! fill the same `RemappingUnit`s.
//!
//! Slice 1 is discovery only. Translation stays OFF and DMA is unchanged -- still
//! kernel-bridged, where the kernel is the only writer of physical descriptor
//! addresses, so isolation already holds without an IOMMU. Building the
//! per-device domains, enabling translation, binding the block device's DMA, and
//! forcing an out-of-domain fault are the later slices; this only reports the
//! ground they stand on.
//!
//! Clean-room: built from the public VT-d / DMAR table layout and generic OSdev
//! references, not from any other kernel's IOMMU code.

use core::fmt::Write;
use core::ptr::{read_volatile, write_volatile};

use spin::Mutex;

use crate::acpi;
use crate::frame_alloc::{FrameAlloc, FRAME_ALLOC};
use crate::memory;
use crate::pci;

/// The largest number of remapping units we retain. Each DRHD in the DMAR
/// becomes one unit, so this matches `acpi::MAX_DRHD`.
pub const MAX_UNITS: usize = acpi::MAX_DRHD;

/// Which IOMMU vendor a remapping unit is, chosen at discovery from the ACPI table
/// present (VT-d's DMAR or AMD-Vi's IVRS). Selects which `Backend` bring-up and
/// `PteFmt` a unit uses; a device with neither table stays kernel-bridged.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Vtd,
    AmdVi,
}

/// One remapping unit, platform-agnostic: the MMIO register base a backend
/// programs, the PCI segment it covers, and its vendor. The VT-d backend fills this
/// from a DRHD; the AMD-Vi backend fills it from an IVHD.
///
/// `allow(dead_code)`: some fields are populated by `discover` and first read by the
/// backend bring-up, kept as a forward API rather than re-plumbed later, mirroring
/// how `acpi::Topology` was defined ahead of its consumer.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub struct RemappingUnit {
    /// MMIO base of this unit's register set.
    pub register_base: u64,
    /// PCI segment (domain) number this unit covers.
    pub segment: u16,
    /// This unit covers every device in its segment not claimed by another unit
    /// (VT-d INCLUDE_PCI_ALL). When false, it covers only the devices its DMAR
    /// device-scope names -- which is what QEMU's `intel-iommu` reports.
    pub covers_all: bool,
    /// Host DMA address width in bits, the page-table depth is derived from it when
    /// a domain is built for this unit.
    pub addr_width: u8,
    /// VT-d or AMD-Vi -- which backend and page-table format this unit uses.
    pub vendor: Vendor,
}

impl RemappingUnit {
    const EMPTY: RemappingUnit = RemappingUnit {
        register_base: 0,
        segment: 0,
        covers_all: false,
        addr_width: 0,
        vendor: Vendor::Vtd,
    };
}

/// The remapping units discovered at boot. Filled once by `discover`; read by the
/// later slices that build domains and enable translation. A zero count means no
/// IOMMU was found (no VT-d DMAR, and AMD-Vi is not yet a backend) -- DMA stays
/// kernel-bridged with no hardware protection, which is the current safe default.
static UNITS: Mutex<[RemappingUnit; MAX_UNITS]> =
    Mutex::new([RemappingUnit::EMPTY; MAX_UNITS]);
static UNIT_COUNT: Mutex<usize> = Mutex::new(0);

/// Discover the platform's DMA-remapping units from ACPI, report them, and store
/// them for the later slices. Returns the number of units found.
///
/// Pure discovery: no translation is enabled and DMA is unchanged. Call once at
/// boot, after `acpi::init` -- it re-reads the firmware tables through the same
/// phys-offset window, keyed off the same `BootInfo` RSDP.
///
/// Only the stable count is asserted by the smoke test; the per-unit register
/// base and the address width ride unasserted detail lines (they can shift across
/// QEMU versions, exactly like the MADT LAPIC base and the PCI BARs).
pub fn discover<W: Write>(out: &mut W, rsdp: Option<u64>, phys_offset: u64) -> usize {
    // VT-d first (DMAR), then AMD-Vi (IVRS); a platform has one or the other (D7).
    if let Some(dmar) = acpi::find_dmar(rsdp, phys_offset) {
        let mut units = UNITS.lock();
        let mut count = 0usize;
        for i in 0..dmar.drhd_count.min(MAX_UNITS) {
            let d = dmar.drhds[i];
            units[count] = RemappingUnit {
                register_base: d.register_base,
                segment: d.segment,
                covers_all: d.include_pci_all,
                addr_width: dmar.host_addr_width,
                vendor: Vendor::Vtd,
            };
            count += 1;
            // Detail line -- the register base is not asserted (allow-listed).
            let _ = writeln!(
                out,
                "plinth:   iommu unit: base 0x{:x} segment {} covers_all {}",
                d.register_base, d.segment, d.include_pci_all as u8
            );
        }
        *UNIT_COUNT.lock() = count;
        drop(units);
        // The one asserted line is the stable count PREFIX; the address-width tail is
        // not asserted (aw-bits varies by QEMU version). "translation off" is the
        // whole claim: the unit is found, not yet driving anything.
        let _ = writeln!(
            out,
            "plinth: iommu: {} dma remapping unit(s), {}-bit DMA addressing (translation off)",
            count, dmar.host_addr_width
        );
        return count;
    }

    if let Some(ivrs) = acpi::find_ivrs(rsdp, phys_offset) {
        *UNITS.lock() = {
            let mut u = [RemappingUnit::EMPTY; MAX_UNITS];
            u[0] = RemappingUnit {
                register_base: ivrs.mmio_base,
                segment: ivrs.segment,
                // The IVHD's device scope covers its whole segment for our purposes.
                covers_all: true,
                addr_width: ivrs.host_addr_width,
                vendor: Vendor::AmdVi,
            };
            u
        };
        *UNIT_COUNT.lock() = 1;
        // Detail line (allow-listed): the IOMMU BDF is what pci.rs skips (D9).
        let _ = writeln!(
            out,
            "plinth:   iommu unit: base 0x{:x} segment {} vendor amd-vi bdf 0x{:04x}",
            ivrs.mmio_base, ivrs.segment, ivrs.iommu_bdf
        );
        // Same asserted prefix as the VT-d path, so the count line is vendor-neutral.
        let _ = writeln!(
            out,
            "plinth: iommu: 1 dma remapping unit(s), {}-bit DMA addressing (translation off)",
            ivrs.host_addr_width
        );
        return 1;
    }

    // Neither table: a plain q35 with no vIOMMU. Not an error -- DMA stays
    // kernel-bridged, which needs no IOMMU to be safe.
    let _ = writeln!(out, "plinth: iommu: no remapping unit (no VT-d DMAR or AMD-Vi IVRS)");
    0
}

/// The remapping units discovered at boot, copied out with their count. Empty
/// until `discover` runs, and empty on a machine with no IOMMU. Slice 2 (domain
/// build) is the first real consumer; exposed now so that consumer has an API to
/// read rather than reaching into the statics.
#[allow(dead_code)]
pub fn units() -> ([RemappingUnit; MAX_UNITS], usize) {
    (*UNITS.lock(), *UNIT_COUNT.lock())
}

// ---------------------------------------------------------------------------
// Domain: the second-level (device) DMA page table -- slice 2.
//
// A domain is the per-device address space an IOMMU enforces: "which physical
// frames may this device touch." It is a page-table tree over the existing
// frame allocator, structurally like the CPU's page tables but in the VT-d
// **second-level** format, which is deliberately NOT `x86_64::PageTable`:
//   - an entry is "present" iff Read or Write is set (bits 0/1); there is no
//     separate present bit, and no NX/user/global bits,
//   - the next-level / page physical address sits in bits [51:12].
// Slice 2 builds and unit-tests this as a pure structure over the real frame
// allocator (the in-kernel harness, the `WaitQueue` pattern): create a domain,
// map/unmap a 4-KiB frame, translate, tear down. No VT-d register is touched and
// translation is not enabled -- that is slice 3, which points a device's context
// entry at `Domain::root` and turns the unit on. Everything below is test-only
// until then, so it is dead in the shipping build (the `frame_alloc` precedent).
// ---------------------------------------------------------------------------

/// Second-level PTE: readable. Present == (READ | WRITE) != 0.
#[cfg_attr(not(feature = "tests"), allow(dead_code))]
pub const IOMMU_READ: u64 = 1 << 0;
/// Second-level PTE: writable.
#[cfg_attr(not(feature = "tests"), allow(dead_code))]
pub const IOMMU_WRITE: u64 = 1 << 1;

/// The next-level-table / page physical address field of a second-level entry,
/// bits [51:12]. Both the intermediate links and the leaf mapping use it.
const SL_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
/// Present iff either permission bit is set (VT-d has no separate present bit).
const SL_PRESENT: u64 = IOMMU_READ | IOMMU_WRITE;
/// 512 u64 entries per 4-KiB table; 9 IOVA bits index each level.
const ENTRIES: usize = 512;
const INDEX_BITS: usize = 9;
const PAGE_SHIFT: usize = 12;

/// The DMA page-table entry format a `Domain` walks. Both Intel VT-d second-level
/// tables and AMD-Vi I/O page tables are 4-KiB / 9-bit-index / 512-entry radix
/// trees with the physical address in bits [51:12], so the walk is shared; only the
/// per-entry present/permission/next-level encoding differs, and that difference is
/// isolated to the three primitives here. Vendor selection sets a domain's format at
/// creation; the walk in `map`/`unmap`/`translate`/`free_subtree` is format-agnostic.
/// AMD-Vi I/O page-table entry bits (AMD IOMMU spec). Present is an explicit bit 0;
/// the Next Level field in bits [11:9] tells the walker whether an entry is a page
/// directory (NL 1-6, points to a table at that level) or a page (NL 0, the leaf);
/// read/write permissions are IR (bit 61) / IW (bit 62), ANDed down the walk, so
/// intermediate links carry both and the leaf gates the actual access. The physical
/// address field is bits [51:12], the same `SL_ADDR_MASK` as VT-d.
const AMD_PR: u64 = 1 << 0;
const AMD_NL_SHIFT: u64 = 9;
const AMD_IR: u64 = 1 << 61;
const AMD_IW: u64 = 1 << 62;

#[cfg_attr(not(feature = "tests"), allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PteFmt {
    /// Intel VT-d second-level: present iff Read|Write set; no separate present bit,
    /// no next-level field. Intermediate links carry Read|Write.
    Vtd,
    /// AMD-Vi I/O page table: explicit present bit, a Next Level field, IR/IW
    /// permission bits ANDed down the walk.
    AmdVi,
}

impl PteFmt {
    /// Decode a raw entry to `(present, next-table-or-page physical address)`.
    #[inline]
    fn decode(self, entry: u64) -> (bool, u64) {
        match self {
            PteFmt::Vtd => (entry & SL_PRESENT != 0, entry & SL_ADDR_MASK),
            PteFmt::AmdVi => (entry & AMD_PR != 0, entry & SL_ADDR_MASK),
        }
    }

    /// Encode a leaf entry mapping `phys` with `perms` (`IOMMU_READ`/`IOMMU_WRITE`).
    #[inline]
    fn encode_leaf(self, phys: u64, perms: u64) -> u64 {
        match self {
            PteFmt::Vtd => (phys & SL_ADDR_MASK) | (perms & SL_PRESENT),
            PteFmt::AmdVi => {
                // Next Level 0 == a 4-KiB page (leaf); IR/IW from perms.
                let mut e = AMD_PR | (phys & SL_ADDR_MASK);
                if perms & IOMMU_READ != 0 {
                    e |= AMD_IR;
                }
                if perms & IOMMU_WRITE != 0 {
                    e |= AMD_IW;
                }
                e
            }
        }
    }

    /// Encode an intermediate link to a child table at `child`. `next_level` is the
    /// number of page-table levels below the child (AMD-Vi's Next Level field); VT-d
    /// has no such field and ignores it.
    #[inline]
    fn encode_link(self, child: u64, next_level: u8) -> u64 {
        match self {
            PteFmt::Vtd => (child & SL_ADDR_MASK) | SL_PRESENT,
            PteFmt::AmdVi => {
                // A page directory entry: present, read+write (ANDed down the walk),
                // the child table's level in the Next Level field.
                AMD_PR | AMD_IR
                    | AMD_IW
                    | (child & SL_ADDR_MASK)
                    | ((next_level as u64 & 0x7) << AMD_NL_SHIFT)
            }
        }
    }
}

#[cfg_attr(not(feature = "tests"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainError {
    /// The frame allocator is out of frames for a table.
    Exhausted,
    /// An IOVA or physical address was not 4-KiB aligned.
    Misaligned,
    /// `map` found the leaf entry already present (a conflicting mapping).
    AlreadyMapped,
    /// `unmap` found no mapping at the IOVA.
    NotMapped,
    /// The remapping unit's address width is not a VT-d page-table depth
    /// (39/48/57-bit -> 3/4/5 levels).
    UnsupportedWidth,
    /// The per-domain IOVA allocator's window is fully allocated.
    IovaExhausted,
}

/// A device DMA domain: a second-level page-table tree rooted at `root`. The
/// mapped data frames belong to the caller (a library OS's held capabilities,
/// per D2); the domain owns only the table frames, and `teardown` frees exactly
/// those -- never a mapped page.
#[cfg_attr(not(feature = "tests"), allow(dead_code))]
pub struct Domain {
    /// Physical address of the top-level table (the SL page-table root a context
    /// entry will point at in slice 3).
    root: u64,
    /// Walk depth: 3, 4, or 5, derived from the unit's DMA address width.
    levels: u8,
    /// The per-entry encoding this domain's tables use (VT-d or AMD-Vi). The walk is
    /// shared; this selects how each entry is read and written.
    fmt: PteFmt,
}

/// The 512-entry table at physical address `phys`, via the phys-offset window.
///
/// # Safety
/// `phys` must be a 4-KiB table frame this domain allocated (so it is mapped at
/// `phys_offset` and no other reference aliases it for the call's duration).
unsafe fn table_at(phys: u64) -> *mut [u64; ENTRIES] {
    (memory::phys_offset() + phys) as *mut [u64; ENTRIES]
}

/// Allocate a frame and zero it (every entry not-present) for use as a table.
fn alloc_table(frames: &mut FrameAlloc) -> Result<u64, DomainError> {
    let phys = frames.alloc().map_err(|_| DomainError::Exhausted)?;
    // SAFETY: `alloc` handed us a fresh frame we exclusively own, mapped at
    // phys_offset; zeroing it makes every entry not-present.
    unsafe { (*table_at(phys)).fill(0) };
    Ok(phys)
}

#[cfg_attr(not(feature = "tests"), allow(dead_code))]
impl Domain {
    /// Create an empty domain sized for a remapping unit of `addr_width_bits`
    /// (the DMAR host address width). QEMU's unit is 48-bit -> a 4-level table.
    pub fn new(
        frames: &mut FrameAlloc,
        addr_width_bits: u8,
        fmt: PteFmt,
    ) -> Result<Domain, DomainError> {
        // Both VT-d AGAWs and AMD-Vi modes are 39/48/57-bit == 3/4/5 levels; each
        // level adds 9 bits above the 12-bit page offset.
        let span = (addr_width_bits as usize).checked_sub(PAGE_SHIFT).ok_or(DomainError::UnsupportedWidth)?;
        if span == 0 || span % INDEX_BITS != 0 {
            return Err(DomainError::UnsupportedWidth);
        }
        let levels = span / INDEX_BITS;
        if !(3..=5).contains(&levels) {
            return Err(DomainError::UnsupportedWidth);
        }
        let root = alloc_table(frames)?;
        Ok(Domain { root, levels: levels as u8, fmt })
    }

    /// The table index for `iova` at walk depth `depth` (0 = top level).
    fn index(&self, iova: u64, depth: usize) -> usize {
        let shift = PAGE_SHIFT + (self.levels as usize - 1 - depth) * INDEX_BITS;
        ((iova >> shift) as usize) & (ENTRIES - 1)
    }

    /// Map one 4-KiB page: device address `iova` -> physical frame `phys`, with
    /// `perms` (`IOMMU_READ`/`IOMMU_WRITE`). Allocates intermediate tables as
    /// needed. `AlreadyMapped` if the leaf is already present, so a double-map is
    /// a caught error rather than a silent overwrite.
    pub fn map(
        &mut self,
        frames: &mut FrameAlloc,
        iova: u64,
        phys: u64,
        perms: u64,
    ) -> Result<(), DomainError> {
        if iova % (1 << PAGE_SHIFT) != 0 || phys % (1 << PAGE_SHIFT) != 0 {
            return Err(DomainError::Misaligned);
        }
        let fmt = self.fmt;
        let mut table_phys = self.root;
        let last = self.levels as usize - 1;
        for depth in 0..self.levels as usize {
            let idx = self.index(iova, depth);
            // SAFETY: table_phys is the root or an intermediate frame this domain
            // allocated; we hold `&mut self`, so no other walk aliases it.
            let table = unsafe { &mut *table_at(table_phys) };
            if depth == last {
                let (present, _) = fmt.decode(table[idx]);
                if present {
                    return Err(DomainError::AlreadyMapped);
                }
                table[idx] = fmt.encode_leaf(phys, perms);
                return Ok(());
            }
            let (present, next) = fmt.decode(table[idx]);
            if !present {
                let child = alloc_table(frames)?;
                // Intermediate links carry R+W; leaf perms gate the actual access.
                // next_level = levels below the child (leaf tables are level 1).
                let next_level = (self.levels as usize - 1 - depth) as u8;
                table[idx] = fmt.encode_link(child, next_level);
                table_phys = child;
            } else {
                table_phys = next;
            }
        }
        unreachable!("the leaf level returns inside the loop")
    }

    /// Remove the mapping for `iova`. `NotMapped` if none exists. Intermediate
    /// tables are left in place (freed wholesale by `teardown`); the mapped data
    /// frame is the caller's and is never touched here.
    pub fn unmap(&mut self, iova: u64) -> Result<(), DomainError> {
        if iova % (1 << PAGE_SHIFT) != 0 {
            return Err(DomainError::Misaligned);
        }
        let fmt = self.fmt;
        let mut table_phys = self.root;
        let last = self.levels as usize - 1;
        for depth in 0..self.levels as usize {
            let idx = self.index(iova, depth);
            // SAFETY: as in `map`.
            let table = unsafe { &mut *table_at(table_phys) };
            let (present, next) = fmt.decode(table[idx]);
            if !present {
                return Err(DomainError::NotMapped);
            }
            if depth == last {
                table[idx] = 0;
                return Ok(());
            }
            table_phys = next;
        }
        unreachable!("the leaf level returns inside the loop")
    }

    /// Resolve `iova` to a physical address the way the hardware would, or `None`
    /// if unmapped. The unit-test oracle for `map`/`unmap`; also the shape a
    /// fault check reasons about in slice 4.
    pub fn translate(&self, iova: u64) -> Option<u64> {
        let fmt = self.fmt;
        let mut table_phys = self.root;
        let last = self.levels as usize - 1;
        for depth in 0..self.levels as usize {
            let idx = self.index(iova, depth);
            // SAFETY: read-only walk over this domain's own table frames.
            let table = unsafe { &*table_at(table_phys) };
            let (present, next) = fmt.decode(table[idx]);
            if !present {
                return None;
            }
            if depth == last {
                return Some(next | (iova & ((1 << PAGE_SHIFT) - 1)));
            }
            table_phys = next;
        }
        None
    }

    /// Free every table frame this domain owns (root + all intermediate tables),
    /// leaving it unusable. Mapped data frames are the caller's and are NOT freed.
    /// After this the domain's `root` is 0.
    pub fn teardown(&mut self, frames: &mut FrameAlloc) {
        if self.root != 0 {
            free_subtree(frames, self.root, self.levels, self.fmt);
            self.root = 0;
        }
    }

    /// The SL page-table root physical address, for slice 3 to program into a
    /// device's context entry.
    pub fn root(&self) -> u64 {
        self.root
    }
}

/// Recursively free a table at `table_phys` and, if it is not a leaf table, the
/// subtree beneath it. `level` is the number of levels from this table down to
/// the leaf inclusive (root == `Domain::levels`, leaf table == 1). Leaf-table
/// entries point at caller-owned data frames, so they are never freed -- only the
/// table frames are.
fn free_subtree(frames: &mut FrameAlloc, table_phys: u64, level: u8, fmt: PteFmt) {
    if level > 1 {
        // SAFETY: `table_phys` is a table frame this domain allocated; the walk
        // is read-only and dealloc only flips allocator bitmap bits, not table
        // memory, so the reference stays valid across the recursion.
        let table = unsafe { &*table_at(table_phys) };
        for &entry in table.iter() {
            let (present, child) = fmt.decode(entry);
            if present {
                free_subtree(frames, child, level - 1, fmt);
            }
        }
    }
    let _ = frames.dealloc(table_phys);
}

// ---------------------------------------------------------------------------
// TranslationTables: the per-unit root/context tables -- slice 3a.
//
// VT-d resolves a device to its second-level page table through two more tables,
// indexed by the device's PCI source-id (bus:dev.func):
//
//   root[bus] --present--> context table
//   context[(dev<<3)|func] --present--> SL page table (a `Domain`), + AW + DID
//
// Each table is one 4-KiB frame of 256 128-bit entries (two u64 per entry). This
// builds and unit-tests those two tables as a pure structure -- the fiddly VT-d
// entry bit-packing, isolated and checked before any register is touched. The
// register block that points the unit's RTADDR at `root_phys()` and flips
// translation on is slice 3b, so this is test-only in the shipping build. Single
// bus (bus 0) for now, which is all QEMU's q35 root complex needs; a second bus
// is another context frame hung off another root entry.
// ---------------------------------------------------------------------------

/// Root/context entry present bit (bit 0 of the low u64).
const VT_PRESENT: u64 = 1 << 0;
/// Context-entry translation type: legacy / second-level (TT = 00, bits 3:2).
/// Named to document the choice even though the value is zero.
const CTX_TT_SECOND_LEVEL: u64 = 0b00 << 2;

/// The per-unit root table plus one bus's context table. Owns exactly those two
/// frames; `teardown` frees them (never a `Domain`'s page-table frames -- those
/// belong to the domain).
#[cfg_attr(not(feature = "tests"), allow(dead_code))]
pub struct TranslationTables {
    /// Physical address of the root table (the value RTADDR takes in slice 3b).
    root_phys: u64,
    /// Physical address of the bus-0 context table (root[0] points here).
    ctx_phys: u64,
}

#[cfg_attr(not(feature = "tests"), allow(dead_code))]
impl TranslationTables {
    /// Allocate a zeroed root table and one context table (bus 0), and link
    /// root[0] -> the context table. No device is present until `set_device`.
    pub fn new(frames: &mut FrameAlloc) -> Result<TranslationTables, DomainError> {
        let root_phys = alloc_table(frames)?;
        let ctx_phys = match alloc_table(frames) {
            Ok(p) => p,
            Err(e) => {
                let _ = frames.dealloc(root_phys);
                return Err(e);
            }
        };
        // root[0].low = present | context-table pointer; high stays 0.
        // SAFETY: root_phys is our freshly allocated, zeroed table frame.
        unsafe {
            let root = &mut *table_at(root_phys);
            root[0] = (ctx_phys & SL_ADDR_MASK) | VT_PRESENT;
        }
        Ok(TranslationTables { root_phys, ctx_phys })
    }

    /// Point the device `dev:func` (on bus 0) at the second-level page table
    /// rooted at `slptptr`, with a page-table depth of `levels` (3/4/5) and
    /// domain id `did`. Overwrites any prior entry for that source-id.
    ///
    /// The context entry's Address Width field encodes the AGAW as `levels - 2`
    /// (3-level=1/39-bit, 4-level=2/48-bit, 5-level=3/57-bit).
    pub fn set_device(&mut self, dev: u8, func: u8, slptptr: u64, levels: u8, did: u16) {
        // PCI device is 5 bits, function 3 bits -> an 8-bit source-id, so the
        // entry index is always < 256. Masking keeps a stray high bit from
        // running the write off the end of the context frame.
        let devfn = ((dev as usize & 0x1f) << 3) | (func as usize & 0x7);
        let aw = (levels as u64).saturating_sub(2) & 0x7;
        // SAFETY: ctx_phys is our zeroed context table frame; devfn < 256 so the
        // two-u64 entry at [2*devfn, 2*devfn+1] is in the 512-u64 frame.
        unsafe {
            let ctx = &mut *table_at(self.ctx_phys);
            ctx[2 * devfn] = (slptptr & SL_ADDR_MASK) | CTX_TT_SECOND_LEVEL | VT_PRESENT;
            ctx[2 * devfn + 1] = aw | ((did as u64) << 8);
        }
    }

    /// Clear the context entry for `dev:func`, making the source-id absent. Used
    /// by slice-5 teardown to stop the unit routing a bound device once its domain
    /// is freed; an access from a now-absent source-id faults rather than walking a
    /// freed page table.
    pub fn clear_device(&mut self, dev: u8, func: u8) {
        let devfn = ((dev as usize & 0x1f) << 3) | (func as usize & 0x7);
        // SAFETY: ctx_phys is our context table frame; devfn < 256, so the two-u64
        // entry is in the 512-u64 frame.
        unsafe {
            let ctx = &mut *table_at(self.ctx_phys);
            ctx[2 * devfn] = 0;
            ctx[2 * devfn + 1] = 0;
        }
    }

    /// The root-table physical address, for slice 3b to program into RTADDR.
    pub fn root_phys(&self) -> u64 {
        self.root_phys
    }

    /// The raw (low, high) u64 pair of the root entry for `bus`. Test-only
    /// introspection: the tables are consumed by hardware, so reading back the
    /// encoded bits is the only way to check the packing before slice 3b.
    #[cfg(feature = "tests")]
    pub fn root_entry(&self, bus: u8) -> (u64, u64) {
        // SAFETY: root_phys is our table frame; bus < 256 so the entry is in it.
        let root = unsafe { &*table_at(self.root_phys) };
        (root[2 * bus as usize], root[2 * bus as usize + 1])
    }

    /// The raw (low, high) u64 pair of the context entry for `dev:func` on bus 0.
    #[cfg(feature = "tests")]
    pub fn context_entry(&self, dev: u8, func: u8) -> (u64, u64) {
        let devfn = ((dev as usize & 0x1f) << 3) | (func as usize & 0x7);
        // SAFETY: ctx_phys is our table frame; devfn < 256 so the entry is in it.
        let ctx = unsafe { &*table_at(self.ctx_phys) };
        (ctx[2 * devfn], ctx[2 * devfn + 1])
    }

    /// Free the root and context table frames.
    pub fn teardown(&mut self, frames: &mut FrameAlloc) {
        if self.ctx_phys != 0 {
            let _ = frames.dealloc(self.ctx_phys);
            self.ctx_phys = 0;
        }
        if self.root_phys != 0 {
            let _ = frames.dealloc(self.root_phys);
            self.root_phys = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// VT-d register interface + block-device translation enable -- slice 3b.
//
// The first code that touches the remapping unit's MMIO registers and the first
// shipping caller of Domain/TranslationTables. It gives the kernel's own block
// DMA a real IOMMU domain and turns translation on, while the block smoke must
// still pass byte-identically (the positive; the forced out-of-domain fault is
// slice 4).
//
// Model: one shared identity domain for block DMA (the driver is trusted and
// kernel-bridged, so per-device isolation between the two disks is not the point
// yet -- protecting the kernel's own DMA is). Both virtio-blk devices get a
// context entry pointing at that domain. The domain maps each device's fixed ring
// frames at prepare time and each request's data frame on demand (add-only: with
// QEMU caching-mode off, a not-present->present change needs no invalidation).
//
// Lock order (boot is single-threaded, but stated so it stays correct under SMP):
// a device lock (held across post_request) -> BLOCK_IOMMU -> FRAME_ALLOC. Nothing
// takes them in reverse. map_kernel_mmio locks FRAME_ALLOC itself, so it is
// always called before FRAME_ALLOC is taken here.
// ---------------------------------------------------------------------------

/// VT-d register offsets (from the unit's register base).
const VTD_CAP: usize = 0x08; // capabilities (u64): SAGAW, MGAW, caching mode
const VTD_ECAP: usize = 0x10; // extended capabilities (u64)
const VTD_GCMD: usize = 0x18; // global command (u32, write to act)
const VTD_GSTS: usize = 0x1c; // global status (u32, read back)
const VTD_RTADDR: usize = 0x20; // root table address (u64)

/// GCMD/GSTS bits.
const GCMD_SRTP: u32 = 1 << 30; // set root table pointer (one-shot)
const GCMD_TE: u32 = 1 << 31; // translation enable (sticky)
const GSTS_RTPS: u32 = 1 << 30; // root table pointer set
const GSTS_TES: u32 = 1 << 31; // translation enable status

/// Domain id for the shared block domain. Any nonzero id distinct from the
/// reserved domain 0 works; both block devices share it.
const BLOCK_DID: u16 = 1;

/// Bound on a status-register poll, so a unit that never sets a status bit fails
/// loudly instead of hanging the boot.
const GSTS_POLL_LIMIT: u32 = 1_000_000;

/// # Safety
/// `va + off` must be inside the mapped, uncached VT-d register window.
unsafe fn reg_r32(va: u64, off: usize) -> u32 {
    read_volatile((va + off as u64) as *const u32)
}
unsafe fn reg_w32(va: u64, off: usize, val: u32) {
    write_volatile((va + off as u64) as *mut u32, val)
}
unsafe fn reg_r64(va: u64, off: usize) -> u64 {
    read_volatile((va + off as u64) as *const u64)
}
unsafe fn reg_w64(va: u64, off: usize, val: u64) {
    write_volatile((va + off as u64) as *mut u64, val)
}

/// A DMA fault reported by a remapping unit: the faulting page address and the
/// vendor fault-reason code. The neutral shape both backends' fault paths return
/// (VT-d reads it from the FRCD register; an AMD-Vi backend reads its event log).
#[derive(Clone, Copy)]
pub struct Fault {
    pub addr: u64,
    /// Vendor fault-reason code. Recorded for diagnostics and forward use (the
    /// AMD-Vi event log carries one too); no caller reads it yet, so it is a
    /// forward-API field like `RemappingUnit`'s.
    #[allow(dead_code)]
    pub reason: u32,
}

/// The vendor-specific half of a remapping unit -- the register window, the
/// device->domain tables, and the fault/invalidation machinery that differ between
/// VT-d and AMD-Vi. Chosen once at bring-up and dispatched by match, exactly as
/// `irq` selects PIC-vs-APIC (no `dyn`, no allocator). The neutral orchestration
/// (`BlockIommu`, the shared `Domain`, `IovaAllocator`) sits outside this enum.
enum Backend {
    Vtd(VtdUnit),
    AmdVi(AmdViUnit),
}

/// Intel VT-d's per-unit state: the mapped register window, the root/context
/// tables, and the FRCD / IOTLB register offsets derived from CAP/ECAP.
struct VtdUnit {
    regs_va: u64,
    tables: TranslationTables,
    /// Byte offset of the first fault-recording register (FRCD) from the register
    /// base, derived from CAP.FRO -- read to confirm a forced fault.
    fault_off: usize,
    /// Byte offset of the IOTLB invalidate register (IOTLB_REG) from the register
    /// base, derived from ECAP.IRO. Under caching-mode a mapping change is only
    /// seen after invalidating here.
    iotlb_off: usize,
}

impl Backend {
    /// Point the device `loc` at the domain rooted at `slptptr` (depth `levels`,
    /// domain id `did`).
    fn attach_device(&mut self, loc: pci::Location, slptptr: u64, levels: u8, did: u16) {
        match self {
            Backend::Vtd(v) => v.tables.set_device(loc.slot, loc.func, slptptr, levels, did),
            Backend::AmdVi(v) => v.set_device(loc, slptptr, levels, did),
        }
    }

    /// Make `loc`'s source-id absent so it stops routing (teardown).
    fn detach_device(&mut self, loc: pci::Location) {
        match self {
            Backend::Vtd(v) => v.tables.clear_device(loc.slot, loc.func),
            Backend::AmdVi(v) => v.clear_device(loc),
        }
    }

    /// Drop the unit's cached translations so a mapping change is seen.
    fn invalidate_all(&mut self) {
        match self {
            // SAFETY: regs_va/iotlb_off are this unit's mapped registers.
            Backend::Vtd(v) => unsafe { invalidate_all(v.regs_va, v.iotlb_off) },
            Backend::AmdVi(v) => v.invalidate_all(),
        }
    }

    /// Read and clear the first recorded DMA fault, if any.
    fn take_fault(&mut self) -> Option<Fault> {
        match self {
            Backend::Vtd(v) => v.take_fault(),
            Backend::AmdVi(v) => v.take_fault(),
        }
    }

    /// Point the unit at its device tables and enable DMA translation, reporting
    /// the unit's capabilities on `out`.
    fn enable<W: Write>(&mut self, out: &mut W) -> Result<(), &'static str> {
        match self {
            Backend::Vtd(v) => v.enable(out),
            Backend::AmdVi(v) => v.enable(out),
        }
    }

    /// The device-table root a `set_device`/context entry lives in (VT-d: RTADDR;
    /// AMD-Vi: the Device Table base). Only used internally, kept for symmetry.
    #[allow(dead_code)]
    fn tables_root(&self) -> u64 {
        match self {
            Backend::Vtd(v) => v.tables.root_phys(),
            Backend::AmdVi(v) => v.devtab_phys,
        }
    }

    /// The page-table entry format this backend's domains use, so a domain built for
    /// the bound device (which shares the unit) matches the unit's vendor.
    fn pte_fmt(&self) -> PteFmt {
        match self {
            Backend::Vtd(_) => PteFmt::Vtd,
            Backend::AmdVi(_) => PteFmt::AmdVi,
        }
    }
}

/// The shared block-DMA IOMMU state: a vendor `Backend`, plus the neutral
/// orchestration -- the one identity domain both block devices use, its depth, and
/// the optional directly-bound device's private domain.
struct BlockIommu {
    /// The vendor-specific unit (registers, device tables, fault/invalidation).
    backend: Backend,
    /// The shared identity domain both kernel-bridged block devices use.
    domain: Domain,
    levels: u8,
    /// The unit's DMA address width in bits (48 under QEMU), kept so the bound
    /// device's domain can be sized for the same unit without re-reading CAP.
    addr_width: u8,
    prepared: usize,
    enabled: bool,
    /// The one directly-bound device's private non-identity domain, if any
    /// (direct-binding). It shares this unit's registers and device tables but has
    /// its own domain + opaque IOVA allocator and a distinct domain id, so it is
    /// confined separately from the shared block domain (D9).
    bound: Option<BoundDomain>,
}

impl VtdUnit {
    /// Bring up the VT-d unit `unit`: map its register window, validate it supports
    /// the required address width (SAGAW), derive the FRCD/IOTLB offsets, and build
    /// the shared identity `Domain` + the root/context tables. Returns the unit, the
    /// shared domain, and the page-table depth. Maps the registers BEFORE taking
    /// FRAME_ALLOC (map_kernel_mmio locks it internally), preserving the lock order.
    fn bring_up(unit: RemappingUnit) -> Result<(VtdUnit, Domain, u8), &'static str> {
        let regs_va = memory::map_kernel_mmio(unit.register_base, 0x1000)?;
        // SAFETY: regs_va is the freshly mapped, uncached VT-d register window.
        let cap = unsafe { reg_r64(regs_va, VTD_CAP) };
        let sagaw = ((cap >> 8) & 0x1f) as u32;
        let levels = levels_for(unit.addr_width)?;
        // SAGAW bit index is the AGAW value (levels - 2): bit0=30/2lvl, bit1=39/3lvl,
        // bit2=48/4lvl, bit3=57/5lvl -- the same encoding the context AW field uses.
        if sagaw & (1 << (levels - 2)) == 0 {
            return Err("unit does not support the required address width");
        }
        // CAP.FRO (bits [33:24]) is the fault-recording register offset in 16-byte
        // units; the fault probe reads FRCD there.
        let fault_off = (((cap >> 24) & 0x3ff) as usize) * 16;
        // ECAP.IRO (bits [17:8]) is the IOTLB register block offset in 16-byte units;
        // IOTLB_REG (what we write to invalidate) is 8 bytes past it.
        let ecap = unsafe { reg_r64(regs_va, VTD_ECAP) };
        let iotlb_off = (((ecap >> 8) & 0x3ff) as usize) * 16 + 8;
        let (domain, tables) = {
            let mut fg = FRAME_ALLOC.lock();
            let fa = fg.as_mut().ok_or("frame allocator not initialised")?;
            let domain = Domain::new(fa, unit.addr_width, PteFmt::Vtd)
                .map_err(|_| "iommu domain alloc failed")?;
            let tables = TranslationTables::new(fa).map_err(|_| "iommu tables alloc failed")?;
            (domain, tables)
        };
        Ok((VtdUnit { regs_va, tables, fault_off, iotlb_off }, domain, levels))
    }

    /// Point RTADDR at the root table and enable translation, reporting CAP/ECAP.
    fn enable<W: Write>(&mut self, out: &mut W) -> Result<(), &'static str> {
        let regs = self.regs_va;
        let root = self.tables.root_phys();
        // SAFETY: `regs` is the mapped, uncached VT-d register window; the writes
        // below are the spec's root-table-pointer + translation-enable sequence, and
        // each is confirmed by polling its status bit before proceeding.
        unsafe {
            let cap = reg_r64(regs, VTD_CAP);
            let ecap = reg_r64(regs, VTD_ECAP);
            let caching_mode = (cap >> 7) & 1;
            let _ = writeln!(
                out,
                "plinth:   iommu cap {cap:#018x} ecap {ecap:#018x} caching_mode {caching_mode}"
            );

            // Root table pointer, TTM=00 (legacy) since root is 4-KiB aligned.
            reg_w64(regs, VTD_RTADDR, root);
            reg_w32(regs, VTD_GCMD, GCMD_SRTP);
            let mut spun = 0;
            while reg_r32(regs, VTD_GSTS) & GSTS_RTPS == 0 {
                spun += 1;
                if spun >= GSTS_POLL_LIMIT {
                    return Err("iommu: root table pointer never acknowledged");
                }
                core::hint::spin_loop();
            }

            reg_w32(regs, VTD_GCMD, GCMD_TE);
            let mut spun = 0;
            while reg_r32(regs, VTD_GSTS) & GSTS_TES == 0 {
                spun += 1;
                if spun >= GSTS_POLL_LIMIT {
                    return Err("iommu: translation enable never acknowledged");
                }
                core::hint::spin_loop();
            }

            // Flush any stale context/IOTLB state so the device sees the context
            // entries and fixed-frame mappings established before enable.
            invalidate_all(regs, self.iotlb_off);
        }
        Ok(())
    }

    /// Read and clear the first fault-recording register. `None` if no fault. The
    /// reason for a not-present second-level mapping is 0x05 under VT-d.
    fn take_fault(&mut self) -> Option<Fault> {
        // SAFETY: regs_va + fault_off is the mapped FRCD register for this unit.
        unsafe {
            let hi = reg_r64(self.regs_va, self.fault_off + 8);
            if hi & FRCD_HI_FAULT == 0 {
                return None;
            }
            let lo = reg_r64(self.regs_va, self.fault_off);
            let addr = lo & SL_ADDR_MASK;
            let reason = ((hi >> 32) & 0xff) as u32;
            // FRCD.F is RW1C: writing the F bit back clears the record.
            reg_w64(self.regs_va, self.fault_off + 8, FRCD_HI_FAULT);
            // FSTS pending bits are RW1C: write back what we read to clear them.
            let fsts = reg_r32(self.regs_va, VTD_FSTS);
            reg_w32(self.regs_va, VTD_FSTS, fsts);
            Some(Fault { addr, reason })
        }
    }
}

// ---------------------------------------------------------------------------
// AMD-Vi backend (Phase B3).
//
// AMD-Vi differs from VT-d in every mechanism below discovery: an explicit
// Device Table (flat, indexed by BDF) instead of root/context tables, an
// in-memory command buffer for invalidation instead of register pokes, and an
// in-memory event log for faults instead of a fault-recording register. The
// IOMMU accesses these structures with PHYSICAL addresses (it does not translate
// its own structures), so they are reachable before translation is enabled. The
// Device Table is one frame -- 128 entries, BDF 0..127 (bus 0, dev 0..15) -- which
// covers every device QEMU's q35 places (D3: bus 0 only).
// ---------------------------------------------------------------------------

/// AMD-Vi MMIO register offsets from the IOMMU base.
const AMD_REG_DEV_TAB_BASE: usize = 0x00;
const AMD_REG_CMD_BUF_BASE: usize = 0x08;
const AMD_REG_EVT_LOG_BASE: usize = 0x10;
const AMD_REG_CONTROL: usize = 0x18;
const AMD_REG_EXT_FEATURE: usize = 0x30;
const AMD_REG_CMD_BUF_HEAD: usize = 0x2000;
const AMD_REG_CMD_BUF_TAIL: usize = 0x2008;
const AMD_REG_EVT_LOG_HEAD: usize = 0x2010;
const AMD_REG_EVT_LOG_TAIL: usize = 0x2018;

/// Control register bits.
const AMD_CTRL_IOMMU_EN: u64 = 1 << 0;
const AMD_CTRL_EVT_LOG_EN: u64 = 1 << 2;
const AMD_CTRL_CMD_BUF_EN: u64 = 1 << 12;

/// Device Table Entry qword-0 bits.
const AMD_DTE_V: u64 = 1 << 0; // entry valid
const AMD_DTE_TV: u64 = 1 << 1; // translation (page-table) valid
const AMD_DTE_IR: u64 = 1 << 61; // device-level read permission
const AMD_DTE_IW: u64 = 1 << 62; // device-level write permission
/// The Mode (page-table level count) field starts at bit 9 of DTE qword 0.
const AMD_DTE_MODE_SHIFT: u64 = 9;

/// Command opcode field, in command dword 1 bits [31:28].
const AMD_CMD_OPCODE_SHIFT: u32 = 28;
const AMD_CMD_COMPLETION_WAIT: u32 = 0x01;
const AMD_CMD_INV_ALL: u32 = 0x08;
/// COMPLETION_WAIT dword-0 store flag (write the completion value to memory).
const AMD_COMPL_STORE: u32 = 1 << 0;

/// One 4-KiB Device Table frame holds 128 32-byte entries (BDF 0..127).
const AMD_DEVTAB_ENTRIES: usize = 128;
/// Command buffer / event log are 4 KiB = 256 16-byte entries; the size field in
/// the base register is log2(entries) = 8.
const AMD_RING_LEN_FIELD: u64 = 8;
const AMD_CMDBUF_BYTES: u32 = 4096;

/// AMD-Vi per-unit state: the mapped register window and the physical structures
/// the IOMMU DMAs into (Device Table, command buffer, event log, completion
/// semaphore), plus the software copy of the command-buffer tail.
struct AmdViUnit {
    regs_va: u64,
    devtab_va: u64,
    devtab_phys: u64,
    cmdbuf_va: u64,
    cmdbuf_tail: u32,
    /// Event log physical base -- programmed into the unit; the log is read by
    /// `take_fault` (Phase B4), stubbed to `None` here.
    #[allow(dead_code)]
    evtlog_phys: u64,
    /// Completion-wait semaphore: the IOMMU stores `sem_val` here when it finishes
    /// a COMPLETION_WAIT; we poll it. Accessed physically by the IOMMU.
    sem_va: u64,
    sem_phys: u64,
    sem_val: u64,
}

impl AmdViUnit {
    /// Bring up the AMD-Vi unit: map its registers, allocate and zero the Device
    /// Table, command buffer, event log, and completion semaphore, build the shared
    /// AMD-format `Domain`, and program the base registers. Does NOT set IOMMUEN --
    /// `enable` does that after every device's DTE is written (like the VT-d flow).
    fn bring_up(unit: RemappingUnit) -> Result<(AmdViUnit, Domain, u8), &'static str> {
        // Map enough to reach the command/event head-tail registers at 0x2000+.
        let regs_va = memory::map_kernel_mmio(unit.register_base, 0x3000)?;
        let levels = levels_for(unit.addr_width)?;
        let (devtab_phys, cmdbuf_phys, evtlog_phys, sem_phys, domain) = {
            let mut fg = FRAME_ALLOC.lock();
            let fa = fg.as_mut().ok_or("frame allocator not initialised")?;
            let devtab = fa.alloc().map_err(|_| "amd devtab alloc failed")?;
            let cmdbuf = fa.alloc().map_err(|_| "amd cmdbuf alloc failed")?;
            let evtlog = fa.alloc().map_err(|_| "amd evtlog alloc failed")?;
            let sem = fa.alloc().map_err(|_| "amd sem alloc failed")?;
            let domain = Domain::new(fa, unit.addr_width, PteFmt::AmdVi)
                .map_err(|_| "amd domain alloc failed")?;
            (devtab, cmdbuf, evtlog, sem, domain)
        };
        let po = memory::phys_offset();
        let devtab_va = po + devtab_phys;
        let cmdbuf_va = po + cmdbuf_phys;
        let sem_va = po + sem_phys;
        // SAFETY: the four frames were just allocated and are mapped at phys_offset;
        // zeroing makes every DTE invalid and both rings empty before enable.
        unsafe {
            core::ptr::write_bytes(devtab_va as *mut u8, 0, 4096);
            core::ptr::write_bytes(cmdbuf_va as *mut u8, 0, 4096);
            core::ptr::write_bytes((po + evtlog_phys) as *mut u8, 0, 4096);
            write_volatile(sem_va as *mut u64, 0);
            // Program the base registers. Device Table size field (bits [8:0]) = 0
            // means one 4-KiB page. Command buffer / event log size = log2(256) = 8
            // in bits [59:56]. Reset both rings' head and tail.
            reg_w64(regs_va, AMD_REG_DEV_TAB_BASE, devtab_phys & SL_ADDR_MASK);
            reg_w64(regs_va, AMD_REG_CMD_BUF_BASE, (cmdbuf_phys & SL_ADDR_MASK) | (AMD_RING_LEN_FIELD << 56));
            reg_w64(regs_va, AMD_REG_EVT_LOG_BASE, (evtlog_phys & SL_ADDR_MASK) | (AMD_RING_LEN_FIELD << 56));
            reg_w64(regs_va, AMD_REG_CMD_BUF_HEAD, 0);
            reg_w64(regs_va, AMD_REG_CMD_BUF_TAIL, 0);
            reg_w64(regs_va, AMD_REG_EVT_LOG_HEAD, 0);
            reg_w64(regs_va, AMD_REG_EVT_LOG_TAIL, 0);
        }
        Ok((
            AmdViUnit {
                regs_va,
                devtab_va,
                devtab_phys,
                cmdbuf_va,
                cmdbuf_tail: 0,
                evtlog_phys,
                sem_va,
                sem_phys,
                sem_val: 0,
            },
            domain,
            levels,
        ))
    }

    /// Write the Device Table Entry for `loc`: valid + translation-valid, the page
    /// table root, the level count in the Mode field, read+write, and the domain id.
    fn set_device(&mut self, loc: pci::Location, slptptr: u64, levels: u8, did: u16) {
        // BDF for a bus-0 device: (dev << 3) | func.
        let bdf = ((loc.slot as usize & 0x1f) << 3) | (loc.func as usize & 0x7);
        if bdf >= AMD_DEVTAB_ENTRIES {
            return; // beyond the single-frame table (bus 0, dev 0..15)
        }
        let mode = (levels as u64) & 0x7;
        let d0 = AMD_DTE_V
            | AMD_DTE_TV
            | (mode << AMD_DTE_MODE_SHIFT)
            | (slptptr & SL_ADDR_MASK)
            | AMD_DTE_IR
            | AMD_DTE_IW;
        let d1 = did as u64; // domain id in the low bits of qword 1
        // SAFETY: bdf < 128, so the 32-byte entry is within the mapped Device Table
        // frame; the IOMMU is not yet enabled (or is flushed after), so a plain
        // write is safe.
        unsafe {
            let e = (self.devtab_va + (bdf as u64) * 32) as *mut u64;
            write_volatile(e, d0);
            write_volatile(e.add(1), d1);
            write_volatile(e.add(2), 0);
            write_volatile(e.add(3), 0);
        }
    }

    /// Clear `loc`'s Device Table Entry so it stops routing (teardown).
    fn clear_device(&mut self, loc: pci::Location) {
        let bdf = ((loc.slot as usize & 0x1f) << 3) | (loc.func as usize & 0x7);
        if bdf >= AMD_DEVTAB_ENTRIES {
            return;
        }
        // SAFETY: bdf < 128, within the mapped Device Table frame.
        unsafe {
            let e = (self.devtab_va + (bdf as u64) * 32) as *mut u64;
            for i in 0..4 {
                write_volatile(e.add(i), 0);
            }
        }
    }

    /// Post one 16-byte command (four dwords) at the tail and advance the tail
    /// register. Requires the command buffer to be enabled (set in `enable`).
    fn post_command(&mut self, cmd: [u32; 4]) {
        // SAFETY: cmdbuf_tail < 4096 and 16-aligned, so the four dwords are within
        // the mapped command-buffer frame.
        unsafe {
            let p = (self.cmdbuf_va + self.cmdbuf_tail as u64) as *mut u32;
            write_volatile(p, cmd[0]);
            write_volatile(p.add(1), cmd[1]);
            write_volatile(p.add(2), cmd[2]);
            write_volatile(p.add(3), cmd[3]);
        }
        self.cmdbuf_tail = (self.cmdbuf_tail + 16) % AMD_CMDBUF_BYTES;
        // SAFETY: the tail register is at a fixed offset in the mapped window.
        unsafe { reg_w64(self.regs_va, AMD_REG_CMD_BUF_TAIL, self.cmdbuf_tail as u64) };
    }

    /// Post a COMPLETION_WAIT with a memory store and spin until the IOMMU writes
    /// the completion value -- the barrier that a prior invalidation has retired.
    fn completion_wait(&mut self) {
        self.sem_val = self.sem_val.wrapping_add(1);
        let target = self.sem_val;
        // SAFETY: sem_va is our mapped semaphore frame.
        unsafe { write_volatile(self.sem_va as *mut u64, 0) };
        let paddr = self.sem_phys;
        let cmd = [
            (paddr as u32 & 0xffff_fff8) | AMD_COMPL_STORE,
            ((paddr >> 32) as u32) | (AMD_CMD_COMPLETION_WAIT << AMD_CMD_OPCODE_SHIFT),
            target as u32,
            (target >> 32) as u32,
        ];
        self.post_command(cmd);
        let mut spun = 0u32;
        // SAFETY: sem_va is our mapped semaphore frame; the IOMMU stores `target`.
        while unsafe { read_volatile(self.sem_va as *const u64) } != target {
            spun += 1;
            if spun >= INVAL_POLL_LIMIT {
                break;
            }
            core::hint::spin_loop();
        }
    }

    /// Flush every cached translation and DTE (INVALIDATE_IOMMU_ALL), then wait for
    /// completion -- the AMD analogue of VT-d's context + IOTLB invalidation.
    fn invalidate_all(&mut self) {
        let cmd = [0u32, AMD_CMD_INV_ALL << AMD_CMD_OPCODE_SHIFT, 0, 0];
        self.post_command(cmd);
        self.completion_wait();
    }

    /// Enable the IOMMU, command buffer, and event log, then flush all caches so the
    /// Device Table entries written before enable take effect.
    fn enable<W: Write>(&mut self, out: &mut W) -> Result<(), &'static str> {
        // SAFETY: regs_va is the mapped AMD-Vi register window.
        unsafe {
            let efr = reg_r64(self.regs_va, AMD_REG_EXT_FEATURE);
            let _ = writeln!(out, "plinth:   iommu amd-vi efr {efr:#018x}");
            let mut ctrl = reg_r64(self.regs_va, AMD_REG_CONTROL);
            ctrl |= AMD_CTRL_IOMMU_EN | AMD_CTRL_CMD_BUF_EN | AMD_CTRL_EVT_LOG_EN;
            reg_w64(self.regs_va, AMD_REG_CONTROL, ctrl);
        }
        self.invalidate_all();
        Ok(())
    }

    /// Read a recorded DMA fault from the event log. Stubbed to `None` in Phase B3;
    /// the event-log ring parse is Phase B4 (D5/D6).
    fn take_fault(&mut self) -> Option<Fault> {
        None
    }
}

/// A directly-bound device's private DMA state (direct-binding slice 2): its own
/// non-identity `Domain` and the opaque IOVA allocator that names frames in it.
/// Lives inside `BlockIommu` because it shares that unit's registers and
/// root/context tables (q35 has a single remapping unit).
struct BoundDomain {
    domain: Domain,
    iova: IovaAllocator,
    /// The bound device's PCI location, so teardown (slice 5) can clear its
    /// context entry in the shared per-unit tables.
    loc: pci::Location,
}

/// Domain id for the bound device, distinct from `BLOCK_DID` so the unit keeps
/// the bound device's translations separate from the shared block domain's.
const BOUND_DID: u16 = 2;
/// The bound device's opaque IOVA window: a 1 GiB base (well clear of any
/// physical frame the allocator hands out, so an IOVA is never mistaken for a
/// physical address) with room for the virtqueue rings, the header/status
/// buffer, and a handful of data buffers.
const BOUND_IOVA_BASE: u64 = 0x4000_0000;
const BOUND_IOVA_PAGES: usize = 256;

static BLOCK_IOMMU: Mutex<Option<BlockIommu>> = Mutex::new(None);

/// When set, `block_map_dma` skips the mapping for one request -- the slice-4
/// fault probe uses it to send the device at a frame the domain does not map, to
/// prove an out-of-domain access faults. Off in all normal operation.
static SKIP_MAP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// VT-d fault register offsets and bits (slice 4).
const VTD_FSTS: usize = 0x34; // fault status (RW1C)
const FRCD_HI_FAULT: u64 = 1 << 63; // FRCD high: F (this record holds a fault)

/// Context Command Register (invalidate the context cache).
const VTD_CCMD: usize = 0x28;
const CCMD_ICC: u64 = 1 << 63; // invalidate context cache (self-clearing)
const CCMD_CIRG_GLOBAL: u64 = 1 << 61; // request granularity = global
/// IOTLB register bits (the register itself is at ECAP.IRO + 8).
const IOTLB_IVT: u64 = 1 << 63; // invalidate IOTLB (self-clearing)
const IOTLB_IIRG_GLOBAL: u64 = 1 << 60; // request granularity = global
/// Bound on an invalidation-completion poll.
const INVAL_POLL_LIMIT: u32 = 1_000_000;

/// Invalidate the whole context cache, then the whole IOTLB, and wait for each to
/// complete. Required under caching-mode after changing a context entry or a
/// page mapping, or the unit keeps using stale (or cached not-present) entries.
///
/// # Safety
/// `regs` is the mapped register window; `iotlb_off` is ECAP.IRO + 8 within it.
unsafe fn invalidate_all(regs: u64, iotlb_off: usize) {
    reg_w64(regs, VTD_CCMD, CCMD_ICC | CCMD_CIRG_GLOBAL);
    let mut spun = 0;
    while reg_r64(regs, VTD_CCMD) & CCMD_ICC != 0 {
        spun += 1;
        if spun >= INVAL_POLL_LIMIT {
            break;
        }
        core::hint::spin_loop();
    }
    reg_w64(regs, iotlb_off, IOTLB_IVT | IOTLB_IIRG_GLOBAL);
    let mut spun = 0;
    while reg_r64(regs, iotlb_off) & IOTLB_IVT != 0 {
        spun += 1;
        if spun >= INVAL_POLL_LIMIT {
            break;
        }
        core::hint::spin_loop();
    }
}

/// Page-table depth for `width` bits (48 -> 4), matching `Domain::new`.
fn levels_for(width: u8) -> Result<u8, &'static str> {
    let span = (width as usize).checked_sub(PAGE_SHIFT).ok_or("bad addr width")?;
    if span == 0 || span % INDEX_BITS != 0 {
        return Err("addr width not a VT-d AGAW");
    }
    let levels = span / INDEX_BITS;
    if !(3..=5).contains(&levels) {
        return Err("addr width not a VT-d AGAW");
    }
    Ok(levels as u8)
}

/// Give a block device a context entry pointing at the shared identity domain,
/// and map its fixed DMA frames (the virtqueue rings + the header/status buffer)
/// into that domain. Lazily builds the shared domain, root/context tables, and
/// maps the unit's register window on the first call. Does NOT enable translation
/// (that is `block_enable`, once every device is prepared). Idempotent per frame.
pub fn block_prepare_device(loc: pci::Location, fixed_frames: &[u64]) -> Result<(), &'static str> {
    // First call: map registers (this locks FRAME_ALLOC internally, so it must
    // happen before we take that lock below) and validate the unit.
    let need_init = BLOCK_IOMMU.lock().is_none();
    if need_init {
        let (units, n) = units();
        if n == 0 {
            return Err("no remapping unit to bind block DMA to");
        }
        let unit = units[0];
        // Select the backend by the unit's vendor. bring_up maps registers,
        // validates, and builds the shared domain + device tables.
        let (backend, domain, levels) = match unit.vendor {
            Vendor::Vtd => {
                let (vtd, domain, levels) = VtdUnit::bring_up(unit)?;
                (Backend::Vtd(vtd), domain, levels)
            }
            Vendor::AmdVi => {
                let (amd, domain, levels) = AmdViUnit::bring_up(unit)?;
                (Backend::AmdVi(amd), domain, levels)
            }
        };
        *BLOCK_IOMMU.lock() = Some(BlockIommu {
            backend,
            domain,
            levels,
            addr_width: unit.addr_width,
            prepared: 0,
            enabled: false,
            bound: None,
        });
    }

    let mut g = BLOCK_IOMMU.lock();
    let bi = g.as_mut().ok_or("block iommu vanished")?;
    {
        let mut fg = FRAME_ALLOC.lock();
        let fa = fg.as_mut().ok_or("frame allocator not initialised")?;
        for &frame in fixed_frames {
            let page = frame & !((1 << PAGE_SHIFT) - 1);
            match bi.domain.map(fa, page, page, IOMMU_READ | IOMMU_WRITE) {
                Ok(()) | Err(DomainError::AlreadyMapped) => {}
                Err(_) => return Err("mapping a fixed block DMA frame failed"),
            }
        }
    }
    let (root, levels) = (bi.domain.root(), bi.levels);
    bi.backend.attach_device(loc, root, levels, BLOCK_DID);
    bi.prepared += 1;
    Ok(())
}

/// Identity-map one request's data frame into the shared block domain before the
/// device DMAs to it. Add-only and idempotent (frames are reused across
/// requests); with caching-mode off a not-present->present change needs no
/// invalidation. A no-op if block translation was never set up. Called from the
/// request path under the device lock.
pub fn block_map_dma(data_phys: u64) -> Result<(), &'static str> {
    let mut g = BLOCK_IOMMU.lock();
    let Some(bi) = g.as_mut() else { return Ok(()) };
    if !bi.enabled {
        return Ok(());
    }
    // Fault probe (slice 4): deliberately leave this request's frame unmapped so
    // the device's access to it faults.
    if SKIP_MAP.load(core::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    let page = data_phys & !((1 << PAGE_SHIFT) - 1);
    let added = {
        let mut fg = FRAME_ALLOC.lock();
        let fa = fg.as_mut().ok_or("frame allocator not initialised")?;
        match bi.domain.map(fa, page, page, IOMMU_READ | IOMMU_WRITE) {
            Ok(()) => true,
            Err(DomainError::AlreadyMapped) => false,
            Err(_) => return Err("mapping a block DMA data frame failed"),
        }
    };
    // Under caching-mode the device only sees a new mapping after invalidation
    // (and a not-present entry it already touched stays cached until then). Only
    // needed when we actually added a mapping.
    if added {
        bi.backend.invalidate_all();
    }
    Ok(())
}

/// Point the unit at the root table and enable DMA translation. Call once, after
/// every block device has been prepared (both context entries must be present
/// before translation is turned on, or a device without one faults). Reports the
/// unit capabilities and the enable. No-op if already enabled.
pub fn block_enable<W: Write>(out: &mut W) -> Result<(), &'static str> {
    let mut g = BLOCK_IOMMU.lock();
    let Some(bi) = g.as_mut() else {
        return Err("block iommu not prepared");
    };
    if bi.enabled {
        return Ok(());
    }
    bi.backend.enable(out)?;
    bi.enabled = true;
    let _ = writeln!(
        out,
        "plinth: iommu: translation enabled ({} block device(s))",
        bi.prepared
    );
    Ok(())
}

/// True once block DMA translation has been turned on. The fault probe (slice 4)
/// only runs when this holds -- on a machine with no remapping unit there is
/// nothing to fault.
pub fn block_translation_enabled() -> bool {
    BLOCK_IOMMU.lock().as_ref().is_some_and(|bi| bi.enabled)
}

/// The active backend's vendor, or `None` if no unit is bound. Used to gate the
/// forced-fault negative proof to VT-d until the AMD-Vi event-log fault path lands
/// (Phase B4 / D6).
pub fn active_vendor() -> Option<Vendor> {
    BLOCK_IOMMU.lock().as_ref().map(|bi| match &bi.backend {
        Backend::Vtd(_) => Vendor::Vtd,
        Backend::AmdVi(_) => Vendor::AmdVi,
    })
}

/// Arm/disarm the fault probe: while armed, the next `block_map_dma` calls do NOT
/// map their frame, so the device is sent at an address outside its domain. Slice
/// 4 only.
pub fn arm_skip_map(on: bool) {
    SKIP_MAP.store(on, core::sync::atomic::Ordering::Relaxed);
}

/// Read and clear the first recorded DMA fault, if any -- used by the fault probe
/// to confirm an out-of-domain access actually faulted. Returns a neutral `Fault`
/// (the backend reads it from VT-d's FRCD register or an AMD-Vi event log).
pub fn take_fault() -> Option<Fault> {
    let mut g = BLOCK_IOMMU.lock();
    let bi = g.as_mut()?;
    bi.backend.take_fault()
}

// ---------------------------------------------------------------------------
// IOVA allocator + non-identity buffer mapping -- direct-binding slice 1.
//
// Every mapping above uses an IDENTITY IOVA (IOVA == physical): the
// kernel-bridged block path maps each frame to itself, because the kernel is the
// sole writer of the device's descriptors, so an identity map is the simplest
// correct choice and the IOVA is never exposed to anyone. Direct binding
// (`direct_binding.md`, D1/D3) inverts that: a library OS will write its OWN
// descriptors, naming IOVAs the device reads directly, so the IOVA must reveal
// nothing about physical layout. The kernel therefore hands each mapped frame an
// OPAQUE IOVA from a per-domain allocator over a fixed window; the IOVA is
// meaningless outside the domain, and the hardware refuses any IOVA the kernel
// did not map -- that hardware refusal is what lets a libOS-written descriptor
// be safe (I3/I5, D1).
//
// This is slice 1: the allocator plus a map-at-assigned-IOVA path over the
// existing `Domain`, pure structure over the frame allocator, unit-tested before
// any device is bound. It deliberately carries NO invalidation -- no remapping
// unit is live at this layer, and `Domain` stays invalidation-agnostic exactly
// as the slice-3 block path leaves invalidation to `block_map_dma` around the
// same `Domain::map`. On the bind path (slice 2) the caller invalidates the live
// unit after a map. Test-only in the shipping build until slice 2 is the first
// caller (the `Domain`/`frame_alloc` precedent).
// ---------------------------------------------------------------------------

/// How many freed IOVAs the allocator remembers for reuse. Paired map/unmap
/// traffic stays within this; if it overflows, a freed IOVA is simply not
/// recycled (the window's own slack absorbs it) rather than being mis-recycled.
const IOVA_FREELIST: usize = 64;

/// A per-domain allocator of opaque device IOVAs over a fixed, page-aligned
/// window `[base, base + pages * 4KiB)`. A bump cursor plus a small free stack:
/// `alloc` reuses a freed IOVA before advancing the cursor, so a steady
/// map/unmap workload does not walk off the window. `base` is required nonzero so
/// IOVA 0 stays an obviously-invalid sentinel and never a live buffer address.
///
/// The window layout is kernel policy the libOS never sees: it receives only the
/// individual IOVAs this hands out, never the base, the bounds, or the physical
/// frames behind them.
#[cfg_attr(not(feature = "tests"), allow(dead_code))]
pub struct IovaAllocator {
    /// The next never-yet-allocated IOVA (the bump cursor).
    next: u64,
    /// One past the end of the window.
    end: u64,
    /// Recently freed IOVAs, handed back out before the cursor advances.
    free: [u64; IOVA_FREELIST],
    free_len: usize,
}

#[cfg_attr(not(feature = "tests"), allow(dead_code))]
impl IovaAllocator {
    /// A window of `pages` 4-KiB IOVAs starting at `base`. `base` must be
    /// page-aligned and nonzero; a zero or unaligned base is a kernel bug (it
    /// would make IOVA 0 or an unaligned address live), so it is asserted rather
    /// than handled.
    pub fn new(base: u64, pages: usize) -> IovaAllocator {
        debug_assert!(
            base != 0 && base % (1 << PAGE_SHIFT) == 0,
            "iova window base must be nonzero and page-aligned"
        );
        IovaAllocator {
            next: base,
            end: base + ((pages as u64) << PAGE_SHIFT),
            free: [0; IOVA_FREELIST],
            free_len: 0,
        }
    }

    /// Hand out an opaque, page-aligned IOVA, or `None` when the window is spent.
    /// Reuses a freed IOVA (LIFO) before advancing the cursor, so churn is bounded
    /// to the live working set rather than the total ever mapped.
    pub fn alloc(&mut self) -> Option<u64> {
        if self.free_len > 0 {
            self.free_len -= 1;
            return Some(self.free[self.free_len]);
        }
        if self.next < self.end {
            let iova = self.next;
            self.next += 1 << PAGE_SHIFT;
            Some(iova)
        } else {
            None
        }
    }

    /// Return an IOVA for reuse. Remembered if the free stack has room; otherwise
    /// dropped (the window's slack covers it) -- never handed out incorrectly.
    pub fn free(&mut self, iova: u64) {
        if self.free_len < self.free.len() {
            self.free[self.free_len] = iova;
            self.free_len += 1;
        }
    }
}

#[cfg_attr(not(feature = "tests"), allow(dead_code))]
impl Domain {
    /// Assign an opaque IOVA from `alloc` and map `phys` at it -- a NON-identity
    /// mapping -- returning the IOVA for a library OS to name in its descriptors.
    /// The inverse of `unmap_buffer`.
    ///
    /// If the map fails, the IOVA is returned to `alloc`, so a failed call leaks
    /// no IOVA; any intermediate tables `map` allocated before failing are
    /// reclaimed by `teardown`, exactly as for a direct `map`. No invalidation is
    /// issued here (see the section comment) -- the bind-path caller invalidates
    /// the live unit after this returns.
    pub fn map_buffer(
        &mut self,
        frames: &mut FrameAlloc,
        alloc: &mut IovaAllocator,
        phys: u64,
        perms: u64,
    ) -> Result<u64, DomainError> {
        let iova = alloc.alloc().ok_or(DomainError::IovaExhausted)?;
        match self.map(frames, iova, phys, perms) {
            Ok(()) => Ok(iova),
            Err(e) => {
                alloc.free(iova);
                Err(e)
            }
        }
    }

    /// Unmap a buffer mapped by `map_buffer` and return its IOVA to `alloc` for
    /// reuse. `NotMapped` if the IOVA is not currently mapped; in that case the
    /// IOVA is not returned to the allocator (it was never a live mapping).
    pub fn unmap_buffer(
        &mut self,
        alloc: &mut IovaAllocator,
        iova: u64,
    ) -> Result<(), DomainError> {
        self.unmap(iova)?;
        alloc.free(iova);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Direct-binding: a device on its own non-identity domain -- slice 2.
//
// The two kernel-bridged block devices share ONE identity domain (IOVA == phys),
// because the kernel writes their descriptors. Direct binding (direct_binding.md
// D2/D9) is the opposite: one device gets its OWN domain, and its virtqueue rings
// and buffers live at OPAQUE, NON-IDENTITY IOVAs from slice 1's allocator. This
// slice sets that up; the kernel still drives the device here (the library OS
// writes its own descriptors only in slices 3-5). A device is either
// kernel-bridged or bound, never both (D9).
//
// The bound device shares the one q35 remapping unit with the block devices, so
// its context entry lives in the same per-unit root/context tables and it uses
// the same registers for invalidation -- it differs only in pointing at a
// distinct domain with a distinct domain id (BOUND_DID). These are shipping-path
// functions (virtio_blk::init calls bind_prepare; the bound selftest calls
// bind_map_dma), so unlike the pure structure above they are not test-gated.
// ---------------------------------------------------------------------------

/// Claim `loc` as a directly-bound device: build it a fresh non-identity domain,
/// map its fixed frames (the virtqueue desc/avail/used rings + the header/status
/// buffer) at opaque IOVAs, add a context entry pointing at that domain, and
/// return the assigned IOVAs in the same order as `fixed_frames`. The caller
/// programs the device's queue registers with the ring IOVAs.
///
/// Requires the unit to be up -- a kernel-bridged block device must be prepared
/// first (the bound device is enumerated after the block devices). v1 binds one
/// device. Does NOT enable translation: `block_enable` does that once, after
/// every device (bridged and bound) has a context entry. The fixed-frame
/// mappings are made pre-enable, so no invalidation is needed here (block_enable
/// flushes the whole unit at enable time).
pub fn bind_prepare(loc: pci::Location, fixed_frames: &[u64]) -> Result<[u64; 4], &'static str> {
    if fixed_frames.len() != 4 {
        return Err("bind_prepare expects the four fixed virtqueue frames");
    }
    let mut g = BLOCK_IOMMU.lock();
    let bi = g
        .as_mut()
        .ok_or("bind needs the remapping unit (prepare a block device first)")?;
    if bi.bound.is_some() {
        return Err("a device is already directly bound (v1 binds one)");
    }
    let addr_width = bi.addr_width;
    let levels = bi.levels;
    let fmt = bi.backend.pte_fmt();
    let mut iovas = [0u64; 4];
    let bound = {
        let mut fg = FRAME_ALLOC.lock();
        let fa = fg.as_mut().ok_or("frame allocator not initialised")?;
        let mut domain = Domain::new(fa, addr_width, fmt).map_err(|_| "bound domain alloc failed")?;
        let mut iova = IovaAllocator::new(BOUND_IOVA_BASE, BOUND_IOVA_PAGES);
        for (i, &frame) in fixed_frames.iter().enumerate() {
            let page = frame & !((1 << PAGE_SHIFT) - 1);
            iovas[i] = domain
                .map_buffer(fa, &mut iova, page, IOMMU_READ | IOMMU_WRITE)
                .map_err(|_| "mapping a bound fixed frame failed")?;
        }
        BoundDomain { domain, iova, loc }
    };
    // Context entry -> the bound domain, with the distinct bound domain id.
    // Present before block_enable turns translation on for the whole unit.
    let root = bound.domain.root();
    bi.backend.attach_device(loc, root, levels, BOUND_DID);
    bi.bound = Some(bound);
    Ok(iovas)
}

/// Map one data frame into the bound device's domain at an opaque IOVA and return
/// it, invalidating the unit so a post-enable mapping is actually seen (the bound
/// analogue of `block_map_dma`). Errs if no device is bound. v1 is not idempotent
/// -- a fresh IOVA per call -- which suits the kernel-driven bound selftest;
/// per-request reuse is a later slice's concern.
pub fn bind_map_dma(data_phys: u64) -> Result<u64, &'static str> {
    let mut g = BLOCK_IOMMU.lock();
    let bi = g.as_mut().ok_or("no remapping unit")?;
    let enabled = bi.enabled;
    let page = data_phys & !((1 << PAGE_SHIFT) - 1);
    let iova = {
        let bound = bi.bound.as_mut().ok_or("no bound device")?;
        let mut fg = FRAME_ALLOC.lock();
        let fa = fg.as_mut().ok_or("frame allocator not initialised")?;
        bound
            .domain
            .map_buffer(fa, &mut bound.iova, page, IOMMU_READ | IOMMU_WRITE)
            .map_err(|_| "mapping a bound data frame failed")?
    };
    // Under caching-mode a not-present -> present change is only seen after
    // invalidation; pre-enable this is skipped (block_enable flushes at enable).
    if enabled {
        bi.backend.invalidate_all();
    }
    Ok(iova)
}

/// Tear down the bound device's binding (direct-binding slice 5, D7): clear its
/// context entry so the source-id stops routing, free its non-identity domain's
/// table frames, and invalidate the unit so no stale translation survives. The
/// caller (`virtio_blk::unbind`) has already reset the device (quiesced its DMA)
/// and freed the mapped ring/data frames, which are the device's, not the domain's.
/// A no-op if nothing is bound. The caller must NOT hold FRAME_ALLOC (this takes
/// it), which is why teardown is driven from cap_release, not process teardown.
pub fn bind_teardown() {
    let mut g = BLOCK_IOMMU.lock();
    let Some(bi) = g.as_mut() else { return };
    let enabled = bi.enabled;
    let Some(mut bound) = bi.bound.take() else { return };
    // Stop routing the source-id before freeing the page tables it points at: an
    // access from a now-absent device faults instead of walking freed memory.
    let loc = bound.loc;
    bi.backend.detach_device(loc);
    {
        let mut fg = FRAME_ALLOC.lock();
        if let Some(fa) = fg.as_mut() {
            bound.domain.teardown(fa);
        }
    }
    // Drop the unit's cached context/IOTLB state so the cleared entry and freed
    // domain leave nothing stale behind.
    if enabled {
        bi.backend.invalidate_all();
    }
}
