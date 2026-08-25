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

use spin::Mutex;

use crate::acpi;
use crate::frame_alloc::FrameAlloc;
use crate::memory;

/// The largest number of remapping units we retain. Each DRHD in the DMAR
/// becomes one unit, so this matches `acpi::MAX_DRHD`.
pub const MAX_UNITS: usize = acpi::MAX_DRHD;

/// One remapping unit, platform-agnostic: the MMIO register base a backend
/// programs and the PCI segment it covers. The VT-d backend fills this from a
/// DRHD; an AMD-Vi backend would fill it from an IVHD. Slice 1 stores it and
/// stops -- slice 2 hangs a domain (a per-device page table over the frame
/// allocator) off each unit.
///
/// `allow(dead_code)`: the fields are populated by `discover` (slice 1) and first
/// read by the domain build (slice 2, via `units`). Kept as a forward API rather
/// than re-plumbed later, mirroring how `acpi::Topology` was defined ahead of its
/// consumer.
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
}

impl RemappingUnit {
    const EMPTY: RemappingUnit =
        RemappingUnit { register_base: 0, segment: 0, covers_all: false };
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
    let Some(dmar) = acpi::find_dmar(rsdp, phys_offset) else {
        // No VT-d DMAR: a plain q35 with no `-device intel-iommu`, or an AMD-Vi
        // platform (IVRS, not yet a backend). Not an error -- DMA stays
        // kernel-bridged, which needs no IOMMU to be safe.
        let _ = writeln!(out, "plinth: iommu: no remapping unit (no VT-d DMAR)");
        return 0;
    };

    let mut units = UNITS.lock();
    let mut count = 0usize;
    for i in 0..dmar.drhd_count.min(MAX_UNITS) {
        let d = dmar.drhds[i];
        units[count] = RemappingUnit {
            register_base: d.register_base,
            segment: d.segment,
            covers_all: d.include_pci_all,
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
    // not asserted (VT-d aw-bits varies by QEMU version). "translation off" is
    // stated because that is the whole claim of slice 1: the unit is found, not
    // yet driving anything.
    let _ = writeln!(
        out,
        "plinth: iommu: {} dma remapping unit(s), {}-bit DMA addressing (translation off)",
        count, dmar.host_addr_width
    );
    count
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
    pub fn new(frames: &mut FrameAlloc, addr_width_bits: u8) -> Result<Domain, DomainError> {
        // VT-d AGAWs are 39/48/57-bit == 3/4/5 levels; each level adds 9 bits
        // above the 12-bit page offset.
        let span = (addr_width_bits as usize).checked_sub(PAGE_SHIFT).ok_or(DomainError::UnsupportedWidth)?;
        if span == 0 || span % INDEX_BITS != 0 {
            return Err(DomainError::UnsupportedWidth);
        }
        let levels = span / INDEX_BITS;
        if !(3..=5).contains(&levels) {
            return Err(DomainError::UnsupportedWidth);
        }
        let root = alloc_table(frames)?;
        Ok(Domain { root, levels: levels as u8 })
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
        let mut table_phys = self.root;
        let last = self.levels as usize - 1;
        for depth in 0..self.levels as usize {
            let idx = self.index(iova, depth);
            // SAFETY: table_phys is the root or an intermediate frame this domain
            // allocated; we hold `&mut self`, so no other walk aliases it.
            let table = unsafe { &mut *table_at(table_phys) };
            if depth == last {
                if table[idx] & SL_PRESENT != 0 {
                    return Err(DomainError::AlreadyMapped);
                }
                table[idx] = (phys & SL_ADDR_MASK) | (perms & SL_PRESENT);
                return Ok(());
            }
            if table[idx] & SL_PRESENT == 0 {
                let child = alloc_table(frames)?;
                // Intermediate links carry R+W; leaf perms gate the actual access.
                table[idx] = (child & SL_ADDR_MASK) | SL_PRESENT;
                table_phys = child;
            } else {
                table_phys = table[idx] & SL_ADDR_MASK;
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
        let mut table_phys = self.root;
        let last = self.levels as usize - 1;
        for depth in 0..self.levels as usize {
            let idx = self.index(iova, depth);
            // SAFETY: as in `map`.
            let table = unsafe { &mut *table_at(table_phys) };
            if table[idx] & SL_PRESENT == 0 {
                return Err(DomainError::NotMapped);
            }
            if depth == last {
                table[idx] = 0;
                return Ok(());
            }
            table_phys = table[idx] & SL_ADDR_MASK;
        }
        unreachable!("the leaf level returns inside the loop")
    }

    /// Resolve `iova` to a physical address the way the hardware would, or `None`
    /// if unmapped. The unit-test oracle for `map`/`unmap`; also the shape a
    /// fault check reasons about in slice 4.
    pub fn translate(&self, iova: u64) -> Option<u64> {
        let mut table_phys = self.root;
        let last = self.levels as usize - 1;
        for depth in 0..self.levels as usize {
            let idx = self.index(iova, depth);
            // SAFETY: read-only walk over this domain's own table frames.
            let table = unsafe { &*table_at(table_phys) };
            let entry = table[idx];
            if entry & SL_PRESENT == 0 {
                return None;
            }
            if depth == last {
                return Some((entry & SL_ADDR_MASK) | (iova & ((1 << PAGE_SHIFT) - 1)));
            }
            table_phys = entry & SL_ADDR_MASK;
        }
        None
    }

    /// Free every table frame this domain owns (root + all intermediate tables),
    /// leaving it unusable. Mapped data frames are the caller's and are NOT freed.
    /// After this the domain's `root` is 0.
    pub fn teardown(&mut self, frames: &mut FrameAlloc) {
        if self.root != 0 {
            free_subtree(frames, self.root, self.levels);
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
fn free_subtree(frames: &mut FrameAlloc, table_phys: u64, level: u8) {
    if level > 1 {
        // SAFETY: `table_phys` is a table frame this domain allocated; the walk
        // is read-only and dealloc only flips allocator bitmap bits, not table
        // memory, so the reference stays valid across the recursion.
        let table = unsafe { &*table_at(table_phys) };
        for &entry in table.iter() {
            if entry & SL_PRESENT != 0 {
                free_subtree(frames, entry & SL_ADDR_MASK, level - 1);
            }
        }
    }
    let _ = frames.dealloc(table_phys);
}
