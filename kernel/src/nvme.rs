//! NVMe controller discovery: PCI class scan + BAR-mapped register read.
//!
//! This is the first slice of the NVMe storage driver-gap milestone, and it is
//! deliberately discovery ONLY. It locates an NVMe controller by its PCI class
//! triple, maps the controller register BAR (BAR0, a 64-bit memory BAR), and
//! reads the two identifying registers -- Controller Capabilities (CAP) and
//! Version (VS). It does NOT reset the controller, create admin/IO queues, ring
//! a doorbell, or touch a namespace; driving I/O is a later milestone, and the
//! register map read here is exactly what that step consumes.
//!
//! On a machine with no NVMe controller -- every default lane, and QEMU q35
//! unless `-device nvme` is added -- the class scan finds nothing and this prints
//! nothing, so it is inert outside the `smoke-nvme` lane and leaves the default
//! boot output byte-identical.

use core::fmt::Write;

use crate::{memory, pci};

/// PCI class triple for an NVMe I/O controller (NVMe 1.4 spec, 2.1.1): mass
/// storage (class 0x01), Non-Volatile Memory subclass (0x08), NVMe programming
/// interface (0x02). QEMU's `-device nvme` presents exactly this, as does a
/// physical NVMe SSD.
const CLASS_MASS_STORAGE: u8 = 0x01;
const SUBCLASS_NVM: u8 = 0x08;
const PROG_IF_NVME: u8 = 0x02;

/// The controller register set (NVMe 1.4 spec, 3.1) lives at the start of BAR0.
/// CAP is the 64-bit register at offset 0x00; VS is the 32-bit register at 0x08.
/// Both sit within the first page.
const NVME_REG_CAP: u64 = 0x00;
const NVME_REG_VS: u64 = 0x08;

/// One page covers every identifying register (CAP..CMBSZ all sit below 0x60).
/// The submission/completion doorbells begin at 0x1000 and are untouched here --
/// they belong to the I/O milestone, not discovery.
const NVME_REG_SPAN: u64 = 0x1000;

/// Read a 32-bit MMIO register.
///
/// # Safety
/// `addr` must be a mapped, dword-aligned NVMe register address inside the
/// region returned by `map_kernel_mmio`.
unsafe fn r32(addr: u64) -> u32 {
    core::ptr::read_volatile(addr as *const u32)
}

/// Find an NVMe controller, map its register BAR, and report CAP + Version.
/// Returns true if a controller was found and read, false if none is present.
///
/// Pure discovery: a config-space class scan, a single BAR mapping, and two
/// register reads. No reset, no queues, no doorbell, no namespace I/O.
pub fn discover<W: Write>(out: &mut W) -> bool {
    let loc = match pci::find_class(CLASS_MASS_STORAGE, SUBCLASS_NVM, PROG_IF_NVME) {
        Some(l) => l,
        // No NVMe controller: stay silent so default lanes are unchanged.
        None => return false,
    };

    // BAR0 is the controller register BAR (NVMe 1.4, 3.1.1) and is a 64-bit
    // memory BAR, so `read_bar` consumes BAR0+BAR1 as one address. Size it with
    // the write-all-ones probe before enabling decode, so the transient probe
    // base is never live.
    let bar_phys = pci::read_bar(loc, 0);
    let bar_size = pci::bar_size(loc, 0);
    let _ = writeln!(
        out,
        "plinth: nvme controller at {:02x}:{:02x}.{} bar0 0x{:x} size 0x{:x}",
        loc.bus, loc.slot, loc.func, bar_phys, bar_size
    );

    // Memory-space decode must be on before the registers respond. Enumeration
    // does no DMA, so only Memory Space Enable is set -- Bus Master stays clear
    // until a driver posts a request (unlike virtio-blk's `enable_bus_master`).
    pci::enable_memory_space(loc);

    let base = match memory::map_kernel_mmio(bar_phys, NVME_REG_SPAN) {
        Ok(va) => va,
        Err(e) => {
            let _ = writeln!(out, "plinth: nvme bar map failed: {e}");
            return false;
        }
    };

    // SAFETY: `base` is the freshly mapped BAR0; CAP (0x00, 64-bit) and VS (0x08,
    // 32-bit) are read-only identifying registers within the first page. Reading
    // them is side-effect free -- no reset, no queue setup, no doorbell.
    let (cap_lo, cap_hi, vs) = unsafe {
        (
            r32(base + NVME_REG_CAP),
            r32(base + NVME_REG_CAP + 4),
            r32(base + NVME_REG_VS),
        )
    };
    let cap = ((cap_hi as u64) << 32) | cap_lo as u64;

    // CAP fields (NVMe 1.4, 3.1.1): MQES [15:0] is (max queue entries - 1);
    // DSTRD [35:32] is the doorbell stride, 2^(2+DSTRD) bytes; MPSMIN [51:48] and
    // MPSMAX [55:52] bound the host memory page size at 2^(12+x). These are the
    // parameters the I/O milestone needs to lay out queues and doorbells.
    let mqes = (cap & 0xFFFF) as u32;
    let dstrd = ((cap >> 32) & 0xF) as u32;
    let mpsmin = ((cap >> 48) & 0xF) as u32;
    let mpsmax = ((cap >> 52) & 0xF) as u32;

    // VS fields (NVMe 1.4, 3.1.2): major [31:16], minor [15:8], tertiary [7:0].
    let vs_mjr = (vs >> 16) & 0xFFFF;
    let vs_mnr = (vs >> 8) & 0xFF;
    let vs_ter = vs & 0xFF;

    let _ = writeln!(
        out,
        "plinth: nvme version {vs_mjr}.{vs_mnr}.{vs_ter} cap mqes {} dstrd {} mpsmin {} mpsmax {}",
        mqes + 1,
        dstrd,
        mpsmin,
        mpsmax
    );
    let _ = writeln!(out, "plinth: nvme discovery ok (no reset, no queues, no io)");
    true
}
