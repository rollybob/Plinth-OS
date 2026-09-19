//! NVMe controller discovery and bring-up (reset + admin queue standup).
//!
//! This is the storage driver-gap milestone. The first slice was discovery only
//! (locate the controller by PCI class, map BAR0, read CAP + VS). This slice adds
//! controller bring-up: disable the controller and wait for it to quiesce, stand
//! up the admin submission/completion queues in freshly allocated DMA memory,
//! program AQA/ASQ/ACQ, then enable the controller and wait for it to report
//! ready. It stops there deliberately -- it does NOT ring a doorbell, submit an
//! IDENTIFY command, create I/O queues, or touch a namespace. Issuing admin
//! commands (starting with IDENTIFY) is the next slice, and the admin queues
//! stood up here are exactly what that step submits into.
//!
//! On a machine with no NVMe controller -- every default lane, and QEMU q35
//! unless `-device nvme` is added -- the class scan finds nothing and this prints
//! nothing, so it is inert outside the `smoke-nvme` lane and leaves the default
//! boot output byte-identical.

use core::fmt::Write;

use crate::frame_alloc::{FRAME_ALLOC, FRAME_SIZE};
use crate::{memory, pci, timer};

/// PCI class triple for an NVMe I/O controller (NVMe 1.4 spec, 2.1.1): mass
/// storage (class 0x01), Non-Volatile Memory subclass (0x08), NVMe programming
/// interface (0x02). QEMU's `-device nvme` presents exactly this, as does a
/// physical NVMe SSD.
const CLASS_MASS_STORAGE: u8 = 0x01;
const SUBCLASS_NVM: u8 = 0x08;
const PROG_IF_NVME: u8 = 0x02;

/// Controller registers (NVMe 1.4 spec, 3.1), byte offsets into BAR0.
const NVME_REG_CAP: u64 = 0x00; // Controller Capabilities (64-bit RO)
const NVME_REG_VS: u64 = 0x08; // Version (32-bit RO)
const NVME_REG_CC: u64 = 0x14; // Controller Configuration (32-bit RW)
const NVME_REG_CSTS: u64 = 0x1C; // Controller Status (32-bit RO)
const NVME_REG_AQA: u64 = 0x24; // Admin Queue Attributes (32-bit RW)
const NVME_REG_ASQ: u64 = 0x28; // Admin Submission Queue base (64-bit RW)
const NVME_REG_ACQ: u64 = 0x30; // Admin Completion Queue base (64-bit RW)

/// One page covers every register touched here (CAP..ACQ all sit below 0x60).
/// The submission/completion doorbells begin at 0x1000 and are NOT mapped or
/// touched -- they belong to the admin-command (IDENTIFY) slice, not bring-up.
const NVME_REG_SPAN: u64 = 0x1000;

/// CC (Controller Configuration) fields (NVMe 1.4, 3.1.5).
const CC_EN: u32 = 1 << 0; // Enable
const CC_CSS_NVM: u32 = 0 << 4; // Command Set Selected: NVM command set
const CC_MPS_4K: u32 = 0 << 7; // Memory Page Size: 2^(12+0) = 4 KiB
const CC_AMS_RR: u32 = 0 << 11; // Arbitration: round robin
const CC_IOSQES: u32 = 6 << 16; // I/O SQ entry size: 2^6 = 64 bytes
const CC_IOCQES: u32 = 4 << 20; // I/O CQ entry size: 2^4 = 16 bytes

/// CSTS (Controller Status) fields (NVMe 1.4, 3.1.6).
const CSTS_RDY: u32 = 1 << 0; // Ready
const CSTS_CFS: u32 = 1 << 1; // Controller Fatal Status

/// Admin queue depth in entries. An admin SQ entry is 64 bytes and an admin CQ
/// entry is 16 bytes (both fixed by the spec), so 64 entries fit a single 4-KiB
/// page for either queue -- the SQ exactly (64 * 64 = 4096), the CQ with room to
/// spare (64 * 16 = 1024). Clamped down to CAP.MQES if the controller supports
/// fewer. This is only the admin queue; I/O queue sizing is a later slice.
const ADMIN_QUEUE_ENTRIES: u32 = 64;

/// Read a 32-bit MMIO register.
///
/// # Safety
/// `addr` must be a mapped, dword-aligned NVMe register address inside the
/// region returned by `map_kernel_mmio`.
unsafe fn r32(addr: u64) -> u32 {
    core::ptr::read_volatile(addr as *const u32)
}

/// Write a 32-bit MMIO register.
///
/// # Safety
/// `addr` must be a mapped, dword-aligned NVMe register address inside the
/// region returned by `map_kernel_mmio`.
unsafe fn w32(addr: u64, val: u32) {
    core::ptr::write_volatile(addr as *mut u32, val)
}

/// A discovered, BAR-mapped NVMe controller: where it sits, the kernel VA of its
/// register set, and its capabilities dword. Produced by `discover`, consumed by
/// `bring_up`.
pub struct Nvme {
    loc: pci::Location,
    /// Kernel VA of BAR0 (the controller register set), uncached.
    base: u64,
    /// Raw CAP register (decoded fields read out as needed).
    cap: u64,
}

impl Nvme {
    /// CAP.MQES [15:0] is (max queue entries - 1); the real maximum is +1.
    fn max_queue_entries(&self) -> u32 {
        (self.cap & 0xFFFF) as u32 + 1
    }
    /// CAP.TO [31:24]: worst-case time for CSTS.RDY to change after CC.EN is
    /// written, in 500 ms units.
    fn ready_timeout_500ms(&self) -> u32 {
        ((self.cap >> 24) & 0xFF) as u32
    }
}

/// Allocate one zeroed frame for a DMA queue, returning (physical, kernel VA).
/// The physical base is what the controller is programmed with (ASQ/ACQ take
/// page-aligned bases); the VA is how the kernel would read/write entries. A
/// frame is page-aligned by construction, so the low 12 bits are zero as the
/// admin queue base registers require.
fn alloc_queue() -> Result<(u64, u64), &'static str> {
    let phys = {
        let mut g = FRAME_ALLOC.lock();
        let fa = g.as_mut().ok_or("frame allocator not initialised")?;
        fa.alloc().map_err(|_| "out of frames for nvme admin queue")?
    };
    let va = memory::phys_offset() + phys;
    // SAFETY: freshly allocated frame, reachable through the phys window; nothing
    // else aliases it. Queue memory must start zeroed (a CQ entry's phase bit
    // must read 0 until the controller posts a completion).
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, FRAME_SIZE as usize) };
    Ok((phys, va))
}

/// Poll CSTS until RDY equals `want`, or the controller's CAP.TO-derived timeout
/// elapses. Returns false on timeout or if CSTS.CFS (fatal) is set. Polls in
/// 1 ms steps against the PIT-backed busy wait, so the bound is real wall-clock
/// time, not an iteration count that means nothing across hosts.
fn wait_ready<W: Write>(ctrl: &Nvme, want: bool, out: &mut W) -> bool {
    // CAP.TO is worst-case in 500 ms units; give it that plus a 500 ms margin,
    // and never wait less than 1 s so a controller reporting TO=0 still gets a
    // sane grace period.
    let budget_ms = ctrl
        .ready_timeout_500ms()
        .saturating_mul(500)
        .saturating_add(500)
        .max(1000);
    for _ in 0..budget_ms {
        // SAFETY: CSTS is a read-only status register within the mapped BAR page.
        let csts = unsafe { r32(ctrl.base + NVME_REG_CSTS) };
        if csts & CSTS_CFS != 0 {
            let _ = writeln!(out, "plinth: nvme fatal (CSTS.CFS set)");
            return false;
        }
        if (csts & CSTS_RDY != 0) == want {
            return true;
        }
        timer::busy_wait_us(1000);
    }
    let _ = writeln!(
        out,
        "plinth: nvme timeout waiting for RDY={} ({} ms)",
        want as u32, budget_ms
    );
    false
}

/// Find an NVMe controller, map its register BAR, and report CAP + Version.
/// Returns the controller handle if one is present, None otherwise.
///
/// Pure discovery: a config-space class scan, a single BAR mapping, and two
/// register reads. No reset, no queues, no doorbell, no namespace I/O.
pub fn discover<W: Write>(out: &mut W) -> Option<Nvme> {
    let loc = pci::find_class(CLASS_MASS_STORAGE, SUBCLASS_NVM, PROG_IF_NVME)?;

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

    // Memory-space decode must be on before the registers respond. Discovery
    // does no DMA, so only Memory Space Enable is set here; `bring_up` sets Bus
    // Master Enable once the controller is about to DMA the admin queues.
    pci::enable_memory_space(loc);

    let base = match memory::map_kernel_mmio(bar_phys, NVME_REG_SPAN) {
        Ok(va) => va,
        Err(e) => {
            let _ = writeln!(out, "plinth: nvme bar map failed: {e}");
            return None;
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
    // parameters the admin-queue and I/O steps need to lay out queues and
    // doorbells.
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
    Some(Nvme { loc, base, cap })
}

/// Reset the controller, stand up the admin queues, and enable it.
///
/// Follows the NVMe 1.4 initialization sequence (3.5.1) up to "controller ready":
/// clear CC.EN and wait for CSTS.RDY=0; allocate the admin submission/completion
/// queues in fresh DMA memory; program AQA (queue sizes) and ASQ/ACQ (their
/// physical bases); write CC (page size, command set, admin/IO entry sizes) with
/// EN=1; wait for CSTS.RDY=1. Enables PCI bus mastering first, since the
/// controller DMAs the admin queues once enabled -- this is where
/// `enable_bus_master` starts being used (contrast discovery, which only enabled
/// memory-space decode).
///
/// It stops at "ready": it does NOT ring a doorbell, submit an IDENTIFY command,
/// or create I/O queues. Returns true on success. On any failure it prints the
/// reason and returns false, leaving the boot to continue (an absent or wedged
/// NVMe controller must not hang the boot -- first_metal_boot.md D3).
pub fn bring_up<W: Write>(ctrl: &Nvme, out: &mut W) -> bool {
    // The controller will DMA-fetch admin submission entries and DMA-write admin
    // completions once enabled, so bus mastering must be on before EN=1. (It also
    // sets Memory Space Enable, already set by discovery -- harmless.)
    pci::enable_bus_master(ctrl.loc);

    // Reset: clear CC.EN and wait for the controller to quiesce (CSTS.RDY=0).
    // Admin queue registers may only be programmed while the controller is
    // disabled (NVMe 1.4, 3.5.1). QEMU boots disabled, but a real controller (or
    // a warm reboot) may already be enabled, so this is unconditional.
    // SAFETY: CC is the mapped 32-bit config register within BAR0.
    let cc = unsafe { r32(ctrl.base + NVME_REG_CC) };
    unsafe { w32(ctrl.base + NVME_REG_CC, cc & !CC_EN) };
    if !wait_ready(ctrl, false, out) {
        let _ = writeln!(out, "plinth: nvme reset failed (RDY did not clear)");
        return false;
    }
    let _ = writeln!(out, "plinth: nvme reset ok (RDY=0 after CC.EN cleared)");

    // Admin queue depth: our fixed choice, clamped to what the controller
    // supports (CAP.MQES). AQA encodes each size as (entries - 1).
    let entries = ADMIN_QUEUE_ENTRIES.min(ctrl.max_queue_entries()).max(2);

    let (asq_phys, _asq_va) = match alloc_queue() {
        Ok(q) => q,
        Err(e) => {
            let _ = writeln!(out, "plinth: nvme admin SQ alloc failed: {e}");
            return false;
        }
    };
    let (acq_phys, _acq_va) = match alloc_queue() {
        Ok(q) => q,
        Err(e) => {
            let _ = writeln!(out, "plinth: nvme admin CQ alloc failed: {e}");
            return false;
        }
    };

    // Program admin queue attributes and bases while disabled. AQA: ASQS [11:0],
    // ACQS [27:16], each (entries - 1). ASQ/ACQ are 64-bit; write as two 32-bit
    // halves (the register set is defined for 32-bit accesses).
    let aqa = ((entries - 1) & 0xFFF) | (((entries - 1) & 0xFFF) << 16);
    // SAFETY: AQA/ASQ/ACQ are mapped RW registers within BAR0, written only while
    // the controller is disabled (RDY=0), exactly as the spec requires.
    unsafe {
        w32(ctrl.base + NVME_REG_AQA, aqa);
        w32(ctrl.base + NVME_REG_ASQ, asq_phys as u32);
        w32(ctrl.base + NVME_REG_ASQ + 4, (asq_phys >> 32) as u32);
        w32(ctrl.base + NVME_REG_ACQ, acq_phys as u32);
        w32(ctrl.base + NVME_REG_ACQ + 4, (acq_phys >> 32) as u32);
    }
    let _ = writeln!(
        out,
        "plinth: nvme admin queues ready (asq 0x{asq_phys:x} acq 0x{acq_phys:x}, {entries} entries each)"
    );

    // Enable: 4-KiB pages, NVM command set, round-robin arbitration, the fixed
    // I/O queue entry sizes, EN=1. The admin queue entry sizes are fixed by the
    // spec (64 B SQ, 16 B CQ) and are not configured here; IOSQES/IOCQES describe
    // the I/O queues a later slice will create.
    let cc = CC_EN | CC_CSS_NVM | CC_MPS_4K | CC_AMS_RR | CC_IOSQES | CC_IOCQES;
    // SAFETY: CC is the mapped RW config register; every field is a defined
    // controller-configuration value, written with the admin queues already in
    // place, as the enable step requires.
    unsafe { w32(ctrl.base + NVME_REG_CC, cc) };
    if !wait_ready(ctrl, true, out) {
        let _ = writeln!(out, "plinth: nvme enable failed (RDY did not set)");
        return false;
    }
    let _ = writeln!(out, "plinth: nvme enabled ok (RDY=1)");
    let _ = writeln!(
        out,
        "plinth: nvme bring-up ok (admin queues stood up, no doorbell, no io)"
    );
    true
}

/// Storage driver-gap entry point: discover an NVMe controller and, if present,
/// bring it up (reset + admin queues + enable). Silent and inert when no
/// controller is present. Call once at boot, before any process is created, so
/// the BAR's kernel-half MMIO mapping propagates into every process address
/// space (as the virtio-blk BARs do).
pub fn init<W: Write>(out: &mut W) {
    if let Some(ctrl) = discover(out) {
        bring_up(&ctrl, out);
    }
}
