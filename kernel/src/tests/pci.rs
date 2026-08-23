//! PCI storage-controller classification (real_hardware.md S6/F4).
//!
//! The boot-time enumerate-report path is proven end to end by the smoke, but
//! only for the one controller QEMU q35 exposes -- a SATA AHCI at 00:1f.2. The
//! arms that matter on other real machines (NVMe above all) are never hit under
//! QEMU, so they are pinned here: the subclass -> name mapping, and the prog_if
//! distinction that separates an AHCI SATA controller from a plain one.

use super::TestCtx;
use crate::pci::mass_storage_name;
use crate::test_assert;

pub fn classifies_storage_subclasses(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    test_assert!(mass_storage_name(0x08, 0x02) == "NVMe", "subclass 0x08 should be NVMe");
    test_assert!(mass_storage_name(0x01, 0x00) == "IDE", "subclass 0x01 should be IDE");
    test_assert!(mass_storage_name(0x00, 0x00) == "SCSI", "subclass 0x00 should be SCSI");
    test_assert!(mass_storage_name(0x05, 0x00) == "ATA", "subclass 0x05 should be ATA");
    // An unknown subclass still names it as storage rather than misreporting.
    test_assert!(
        mass_storage_name(0x80, 0x00) == "mass storage",
        "unknown subclass should fall back to a generic storage name"
    );
    Ok(())
}

/// The prog_if byte is what separates an AHCI SATA controller (1) -- the one
/// Plinth would actually meet on a modern SATA machine -- from a plain one.
pub fn ahci_prog_if_distinguished(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    test_assert!(
        mass_storage_name(0x06, 0x01) == "SATA (AHCI)",
        "SATA with prog_if 1 should be named AHCI"
    );
    test_assert!(
        mass_storage_name(0x06, 0x00) == "SATA",
        "SATA with prog_if 0 should not claim AHCI"
    );
    Ok(())
}
