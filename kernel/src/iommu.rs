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
