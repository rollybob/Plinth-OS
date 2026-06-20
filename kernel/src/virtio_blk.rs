//! virtio-blk modern (virtio-pci) driver -- Stage 1 storage bring-up.
//!
//! `pci` discovers the device and hands us a `VirtioBlkInfo` (where the four
//! virtio structures live inside a BAR). This module maps that BAR, negotiates
//! features the modern way, stands up one split virtqueue, and does block I/O.
//! Stage 1 polls for completion (a bounded spin that faults on timeout, per
//! Design/filesystem.md D6); the interrupt-driven path is Stage 4.
//!
//! Discipline (clean-room, single trusted in-kernel driver): the kernel owns
//! the virtqueue rings (frames from the allocator) and programs descriptors at
//! a caller-provided I/O frame's *physical* address. The device DMAs by
//! physical address, so no IOMMU is needed while the driver is trusted and
//! in-kernel (D4). All MMIO is volatile; fences bracket the device handoff.

use core::fmt::Write;
use core::sync::atomic::{fence, Ordering};

use spin::Mutex;
use x86_64::structures::idt::InterruptStackFrame;

use crate::capability::{CapObject, RIGHT_READ, RIGHT_WRITE};
use crate::frame_alloc::{FRAME_ALLOC, FRAME_SIZE};
use crate::{interrupts, irq, memory, process, scheduler};
use crate::pci::{self, VirtioBlkInfo};

// --- virtio_pci_common_cfg field offsets (virtio 1.x, 4.1.4.3) ---
const CFG_DEVICE_FEATURE_SELECT: u64 = 0x00;
const CFG_DEVICE_FEATURE: u64 = 0x04;
const CFG_DRIVER_FEATURE_SELECT: u64 = 0x08;
const CFG_DRIVER_FEATURE: u64 = 0x0C;
const CFG_DEVICE_STATUS: u64 = 0x14;
const CFG_QUEUE_SELECT: u64 = 0x16;
const CFG_QUEUE_SIZE: u64 = 0x18;
const CFG_QUEUE_ENABLE: u64 = 0x1C;
const CFG_QUEUE_NOTIFY_OFF: u64 = 0x1E;
const CFG_QUEUE_DESC: u64 = 0x20;
const CFG_QUEUE_DRIVER: u64 = 0x28;
const CFG_QUEUE_DEVICE: u64 = 0x30;
// MSI-X vector selectors (virtio 1.x, 4.1.4.3): each holds an INDEX into the
// MSI-X table (which table entry signals this source), not a CPU vector
// number. 0xFFFF (VIRTIO_MSI_NO_VECTOR) means "deliver nothing" for that
// source.
const CFG_MSIX_CONFIG: u64 = 0x10;
const CFG_QUEUE_MSIX_VECTOR: u64 = 0x1A;
const VIRTIO_MSI_NO_VECTOR: u16 = 0xFFFF;

// --- device_status bits ---
const STATUS_ACK: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;

/// VIRTIO_F_VERSION_1 is feature bit 32: it lives in the high feature dword
/// (select = 1), as bit 0 there. A modern device requires it.
const FEATURE_VERSION_1_HI_BIT: u32 = 1;

// --- virtq_desc flags ---
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

// --- virtq_avail flags (offset 0 of the avail ring) ---
/// Set by the driver to tell the device NOT to raise an interrupt when it
/// completes a request. The polled boot path sets it (it spins on the used
/// ring); the blocking runtime path clears it (it wants the completion IRQ).
const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;

// --- virtio-blk request (virtio 1.x, 5.2.6) ---
const VIRTIO_BLK_T_IN: u32 = 0; // read: device -> memory
const VIRTIO_BLK_S_OK: u8 = 0;

/// virtio block I/O is always in 512-byte units, independent of any device
/// logical block size. This is the sector unit `BlockRange` will count (D3a).
const SECTOR_SIZE: u64 = 512;

/// `block_read` status words, returned in rax (the C1 status/payload split: the
/// data lands in the caller's frame, so no read-back byte can be confused for an
/// error). Mirrored in libplinth as BLK_*. `block_read` is an `int 0x80` op
/// (Stage 4 S4a) because a blocking call needs a resumable trap frame.
pub const BLK_OK: u64 = 0;
/// count is zero, or count*512 would overflow the I/O frame.
const BLK_E_BADARG: u64 = 1;
/// The request falls outside the holder's BlockRange (multiplexing guarantee).
const BLK_E_RANGE: u64 = 2;
/// Bad slot, wrong object kind, or a missing right on the range or frame cap.
const BLK_E_RIGHTS: u64 = 3;
/// The device reported an error or is not initialised.
pub const BLK_E_DEV: u64 = 4;

/// Cap the virtqueue at a small power of two: we only ever post a 3-descriptor
/// request at a time, so a large ring buys nothing and a smaller one keeps the
/// ring memory to a single frame each.
const QUEUE_SIZE_MAX: u16 = 64;

/// Bound on the completion poll. The device completes a single block read
/// almost immediately; a finite cap turns a wedged/absent device into a clean
/// fault instead of a kernel hang (D6).
const POLL_MAX: u64 = 50_000_000;

/// First CPU vector handed out for virtio MSI-X completions (Stage A3, D7).
/// Distinct from the legacy IRQ vector range (`VECTOR_BASE`..`VECTOR_BASE+16`)
/// and the LAPIC spurious vector (0xFF) -- MSI-X delivers straight to the
/// LAPIC, bypassing the I/O APIC and its line numbering entirely, so these
/// vectors have no relationship to `intr_line`. One per device, by index.
const MSIX_VECTOR_BASE: u8 = 0x30;

/// The brought-up device. Holds virtual addresses (and the physical addresses
/// the device DMAs to/from) as plain integers, so the struct is `Send` and can
/// live in a static behind a spinlock.
struct VirtioBlk {
    /// MMIO virtual address of the notify structure (the only register the I/O
    /// path touches after bring-up).
    notify: u64,
    notify_mult: u32,
    queue_notify_off: u16,
    qsize: u16,
    /// Virtqueue ring virtual addresses (the device has their physical bases).
    desc_va: u64,
    avail_va: u64,
    used_va: u64,
    /// Request header (16 B) and status byte (1 B), in one frame.
    hdr_va: u64,
    hdr_phys: u64,
    status_va: u64,
    status_phys: u64,
    /// Last used-ring index we have consumed (the ring's idx is free-running).
    last_used: u16,
    /// Device capacity in 512-byte sectors (virtio-blk device-config offset 0).
    /// The boot code grants a whole-device BlockRange of this many sectors.
    capacity: u64,
    /// MMIO virtual address of the ISR-status register. Read to ack/deassert
    /// the level-triggered INTx at the device (a read clears it) before EOIing
    /// the controller (Stage 4 S4c) -- only meaningful on the INTx fallback
    /// path (`msix_vector.is_none()`); MSI-X needs no such read (Stage A3).
    isr_va: u64,
    /// The INTx line IRQ this device is wired to (PCI config 0x3C), used only
    /// when `msix_vector` is `None`: the handler is installed at
    /// VECTOR_BASE+line and EOIs this line.
    intr_line: u8,
    /// The CPU vector this device's MSI-X table entry 0 was programmed to
    /// deliver, if MSI-X came up for it (Stage A3, D7: only under
    /// `irq::apic_mode()`, with a MADT-supplied LAPIC to target). `None` means
    /// this device is on the INTx fallback (`intr_line` instead).
    msix_vector: Option<u8>,
    /// The process slot blocked waiting for this device's I/O completion, if
    /// any. One waiter per device (a device's Mutex serialises requests, so at
    /// most one read is ever outstanding); woken by the completion IRQ. Named by
    /// the existing BlockRange, so no new capability (S4d).
    waiter: Option<usize>,
}

/// One slot per virtio-blk device the kernel tracks (pci::MAX_DEVICES). A
/// device's index here is its PCI-slot discovery order (see `pci`); the boot
/// code brings each up at its index and mints BlockRange capabilities against
/// it. Per-device locks (rather than one lock around the array) keep the
/// devices independent -- though Plinth is uniprocessor, so it is the cleaner
/// shape rather than a performance need. The `[CONST; N]` form is how a
/// non-Copy `Mutex` array is built in a `static`.
const NEW_DEVICE: Mutex<Option<VirtioBlk>> = Mutex::new(None);
static DEVICES: [Mutex<Option<VirtioBlk>>; pci::MAX_DEVICES] =
    [NEW_DEVICE; pci::MAX_DEVICES];

// --- volatile MMIO / ring accessors ---
#[inline]
unsafe fn r8(a: u64) -> u8 {
    core::ptr::read_volatile(a as *const u8)
}
#[inline]
unsafe fn r16(a: u64) -> u16 {
    core::ptr::read_volatile(a as *const u16)
}
#[inline]
unsafe fn r32(a: u64) -> u32 {
    core::ptr::read_volatile(a as *const u32)
}
#[inline]
unsafe fn r64(a: u64) -> u64 {
    core::ptr::read_volatile(a as *const u64)
}
#[inline]
unsafe fn w8(a: u64, v: u8) {
    core::ptr::write_volatile(a as *mut u8, v)
}
#[inline]
unsafe fn w16(a: u64, v: u16) {
    core::ptr::write_volatile(a as *mut u16, v)
}
#[inline]
unsafe fn w32(a: u64, v: u32) {
    core::ptr::write_volatile(a as *mut u32, v)
}
#[inline]
unsafe fn w64(a: u64, v: u64) {
    core::ptr::write_volatile(a as *mut u64, v)
}
/// Write a 64-bit common-config field as two 32-bit halves (low then high).
/// The virtio common-config MMIO region is defined for 32-bit accesses; a
/// single 64-bit write can be dropped, leaving a ring address half-programmed.
#[inline]
unsafe fn w64_split(a: u64, v: u64) {
    w32(a, v as u32);
    w32(a + 4, (v >> 32) as u32);
}

/// Allocate one frame, zero it, and return (physical, virtual) addresses.
fn alloc_zeroed(phys_offset: u64) -> Result<(u64, u64), &'static str> {
    let phys = {
        let mut g = FRAME_ALLOC.lock();
        let fa = g.as_mut().ok_or("frame allocator not initialised")?;
        fa.alloc().map_err(|_| "out of frames for virtio-blk")?
    };
    let va = phys_offset + phys;
    // SAFETY: the frame is freshly allocated and identity-mapped at phys_offset;
    // nothing else aliases it.
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, FRAME_SIZE as usize) };
    Ok((phys, va))
}

impl VirtioBlk {
    /// Post a 3-descriptor read of `count` 512-byte sectors starting at `sector`
    /// into the buffer at `data_phys` (the device DMAs there; the buffer must
    /// hold count*512 bytes) and notify the device. Does NOT wait: the caller
    /// either polls (`completed` + `take_completion`, the boot path) or blocks
    /// until the completion IRQ (the runtime path). `want_interrupt` sets the
    /// avail-ring NO_INTERRUPT flag accordingly.
    fn post_request(&mut self, sector: u64, count: u64, data_phys: u64, want_interrupt: bool) {
        // SAFETY: all addresses below are kernel-mapped ring/MMIO/buffer
        // addresses set up in `init`; data_phys is a caller-owned frame. The
        // device touches only what these descriptors name.
        unsafe {
            // Avail flags (offset 0): tell the device whether to raise a
            // completion interrupt for the requests we publish. Set before the
            // idx bump so the device sees it when it consumes this request.
            let flags = if want_interrupt { 0 } else { VIRTQ_AVAIL_F_NO_INTERRUPT };
            w16(self.avail_va, flags);

            // Request header (device reads it) and a status sentinel.
            w32(self.hdr_va, VIRTIO_BLK_T_IN);
            w32(self.hdr_va + 4, 0);
            w64(self.hdr_va + 8, sector);
            w8(self.status_va, 0xFF);

            // Three chained descriptors (each 16 B: addr, len, flags, next).
            let d = self.desc_va;
            w64(d, self.hdr_phys);
            w32(d + 8, 16);
            w16(d + 12, VIRTQ_DESC_F_NEXT);
            w16(d + 14, 1);

            w64(d + 16, data_phys);
            w32(d + 24, (count * SECTOR_SIZE) as u32);
            w16(d + 28, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);
            w16(d + 30, 2);

            w64(d + 32, self.status_phys);
            w32(d + 40, 1);
            w16(d + 44, VIRTQ_DESC_F_WRITE);
            w16(d + 46, 0);

            // Publish into the available ring (idx@2, ring@4), then bump idx.
            // Fence so the descriptors are visible before the index, and the
            // index before the notify.
            let avail_idx = r16(self.avail_va + 2);
            let ring_slot = (avail_idx % self.qsize) as u64;
            w16(self.avail_va + 4 + ring_slot * 2, 0); // head descriptor = 0
            fence(Ordering::SeqCst);
            w16(self.avail_va + 2, avail_idx.wrapping_add(1));
            fence(Ordering::SeqCst);

            // Notify queue 0.
            let notify_addr =
                self.notify + (self.queue_notify_off as u64) * (self.notify_mult as u64);
            w16(notify_addr, 0);
        }
    }

    /// True once the device has completed the outstanding request (its used-ring
    /// index moved past the one we last consumed). Side-effect free.
    fn completed(&self) -> bool {
        // SAFETY: used_va is the mapped used ring; reading its idx@2 is a read.
        unsafe { r16(self.used_va + 2) != self.last_used }
    }

    /// Consume one completion: advance our used-ring cursor and check the status
    /// byte the device wrote. Call exactly once per request the device finished
    /// (after `completed` is true, or from the completion IRQ handler).
    fn take_completion(&mut self) -> Result<(), &'static str> {
        fence(Ordering::SeqCst);
        self.last_used = self.last_used.wrapping_add(1);
        // SAFETY: status_va is the mapped status byte; the device wrote it before
        // advancing the used ring, and the fence orders our read after that.
        if unsafe { r8(self.status_va) } != VIRTIO_BLK_S_OK {
            return Err("virtio-blk read failed (device status)");
        }
        Ok(())
    }

    /// Synchronous polled read for the boot path: there is no scheduler yet, so
    /// nothing can be blocked. Post with the completion interrupt suppressed,
    /// spin (bounded, faulting on timeout per D6) on the used ring, and consume.
    fn read_block_polled(
        &mut self,
        sector: u64,
        count: u64,
        data_phys: u64,
    ) -> Result<(), &'static str> {
        self.post_request(sector, count, data_phys, false);
        let mut spins = 0u64;
        while !self.completed() {
            spins += 1;
            if spins >= POLL_MAX {
                return Err("virtio-blk read timed out");
            }
            core::hint::spin_loop();
        }
        self.take_completion()
    }
}

/// The span of a BAR we must map to cover all four virtio structures (the
/// highest offset+length among them). All four share one BAR on QEMU's modern
/// virtio-blk; `init` rejects a multi-BAR layout rather than guess.
fn required_span(info: &VirtioBlkInfo) -> u64 {
    let end = |c: pci::VirtioCfg| (c.offset as u64) + (c.length as u64);
    end(info.common)
        .max(end(info.isr))
        .max(end(info.device))
        .max(end(info.notify))
}

/// Bring up MSI-X for device `dev` if the controller is in APIC mode and the
/// device advertises the capability (Stage A3, D7): map its MSI-X table BAR
/// (QEMU puts it in a different BAR than the virtio structures -- confirmed
/// by discovery, not assumed), program table entry 0 to deliver
/// `MSIX_VECTOR_BASE + dev` to the boot CPU, enable MSI-X in the capability's
/// Message Control word, and disable the device's legacy INTx# pin so it
/// cannot also fire. Returns the assigned vector, or `None` if MSI-X is
/// unavailable (no MADT/LAPIC, or the device lacks the capability) -- the
/// caller then stays on the INTx path entirely, unchanged from Stage 4.
///
/// Deliberately does NOT touch virtio common-config here: the
/// `queue_msix_vector`/`msix_config` writes need `queue_select` already at 0,
/// which `init` only guarantees later in its own queue-setup block.
fn setup_msix(info: &VirtioBlkInfo, dev: usize) -> Result<Option<u8>, &'static str> {
    let Some(msix) = info.msix else {
        return Ok(None);
    };
    let Some(bsp_id) = irq::bsp_apic_id() else {
        return Ok(None); // no LAPIC/MADT: nothing to target, stay on INTx
    };

    let table_phys = pci::read_bar(info.loc, msix.table_bar);
    let table_span = (msix.table_offset as u64) + (msix.table_size as u64) * 16;
    let table_base = memory::map_kernel_mmio(table_phys, table_span)?;
    let table_va = table_base + msix.table_offset as u64;

    let vector = MSIX_VECTOR_BASE + dev as u8;
    // SAFETY: table_va is the freshly mapped MSI-X table; entry 0 is within
    // table_size (a working device reports table_size >= 1).
    unsafe {
        // Message Address: physical destination, no redirection hint -- the
        // same addressing the I/O APIC redirection entries already use for
        // their destination field (irq.rs), just inlined into the message
        // instead of going through a redirection table.
        w32(table_va, 0xFEE0_0000 | ((bsp_id as u32) << 12));
        w32(table_va + 4, 0); // address high: no x2APIC destination needed
        w32(table_va + 8, vector as u32); // data: fixed delivery, this vector
        w32(table_va + 12, 0); // vector control: unmasked
    }

    // Enable MSI-X (Message Control bit 15); leave the function-mask bit and
    // the read-only table-size field as the device reported them.
    let control = pci::read16(info.loc.bus, info.loc.slot, info.loc.func, msix.cap_ptr + 2);
    pci::write16(
        info.loc.bus,
        info.loc.slot,
        info.loc.func,
        msix.cap_ptr + 2,
        control | (1 << 15),
    );
    pci::disable_intx(info.loc);

    Ok(Some(vector))
}

/// Bring device `dev` up: enable bus mastering, map its BAR, negotiate
/// features, stand up queue 0, and stash it in `DEVICES[dev]` for `read`. Call
/// once per device at boot, after `pci::discover_all`; `dev` is the device's
/// index in the discovered map.
pub fn init<W: Write>(
    out: &mut W,
    info: &VirtioBlkInfo,
    phys_offset: u64,
    dev: usize,
) -> Result<(), &'static str> {
    if dev >= pci::MAX_DEVICES {
        return Err("virtio-blk device index out of range");
    }
    let bar = info.common.bar;
    if info.isr.bar != bar || info.device.bar != bar || info.notify.bar != bar {
        return Err("virtio-blk spreads structures across multiple BARs (unsupported)");
    }

    // MMIO + DMA must be enabled before we touch registers or post a request
    // (including the MSI-X table, which lives in its own BAR).
    pci::enable_bus_master(info.loc);
    let msix_vector = setup_msix(info, dev)?;

    let bar_phys = pci::read_bar(info.loc, bar);
    let base = memory::map_kernel_mmio(bar_phys, required_span(info))?;
    let common = base + info.common.offset as u64;
    let notify = base + info.notify.offset as u64;
    let device_cfg = base + info.device.offset as u64;
    let isr = base + info.isr.offset as u64;

    // SAFETY: `common` is the mapped common-config MMIO; the status handshake
    // and feature reads/writes below are the defined modern bring-up sequence.
    unsafe {
        // Reset, then wait for the device to acknowledge (status reads 0).
        w8(common + CFG_DEVICE_STATUS, 0);
        let mut spins = 0u64;
        while r8(common + CFG_DEVICE_STATUS) != 0 {
            spins += 1;
            if spins >= POLL_MAX {
                return Err("virtio-blk reset timed out");
            }
            core::hint::spin_loop();
        }

        w8(common + CFG_DEVICE_STATUS, STATUS_ACK);
        w8(common + CFG_DEVICE_STATUS, STATUS_ACK | STATUS_DRIVER);

        // Require VIRTIO_F_VERSION_1 (bit 32 -> bit 0 of the high dword).
        w32(common + CFG_DEVICE_FEATURE_SELECT, 1);
        if r32(common + CFG_DEVICE_FEATURE) & FEATURE_VERSION_1_HI_BIT == 0 {
            return Err("virtio-blk lacks VERSION_1");
        }
        // Accept VERSION_1 and nothing else (no optional blk features needed
        // for a plain sector read).
        w32(common + CFG_DRIVER_FEATURE_SELECT, 1);
        w32(common + CFG_DRIVER_FEATURE, FEATURE_VERSION_1_HI_BIT);
        w32(common + CFG_DRIVER_FEATURE_SELECT, 0);
        w32(common + CFG_DRIVER_FEATURE, 0);

        w8(
            common + CFG_DEVICE_STATUS,
            STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK,
        );
        if r8(common + CFG_DEVICE_STATUS) & STATUS_FEATURES_OK == 0 {
            return Err("virtio-blk rejected VERSION_1 negotiation");
        }
    }

    // Virtqueue rings + the request header/status buffer.
    let (desc_phys, desc_va) = alloc_zeroed(phys_offset)?;
    let (avail_phys, avail_va) = alloc_zeroed(phys_offset)?;
    let (used_phys, used_va) = alloc_zeroed(phys_offset)?;
    let (buf_phys, buf_va) = alloc_zeroed(phys_offset)?;

    let (qsize, queue_notify_off);
    // SAFETY: queue 0 programming on the mapped common-config MMIO.
    unsafe {
        w16(common + CFG_QUEUE_SELECT, 0);
        let dev_qsize = r16(common + CFG_QUEUE_SIZE);
        if dev_qsize == 0 {
            return Err("virtio-blk queue 0 unavailable");
        }
        qsize = dev_qsize.min(QUEUE_SIZE_MAX);
        w16(common + CFG_QUEUE_SIZE, qsize); // may only shrink; qsize <= dev

        w64_split(common + CFG_QUEUE_DESC, desc_phys);
        w64_split(common + CFG_QUEUE_DRIVER, avail_phys);
        w64_split(common + CFG_QUEUE_DEVICE, used_phys);
        queue_notify_off = r16(common + CFG_QUEUE_NOTIFY_OFF);
        if msix_vector.is_some() {
            // queue_select is still 0 from above; route queue 0's completions
            // to MSI-X table entry 0 and skip config-change notifications.
            w16(common + CFG_MSIX_CONFIG, VIRTIO_MSI_NO_VECTOR);
            w16(common + CFG_QUEUE_MSIX_VECTOR, 0);
        }
        w16(common + CFG_QUEUE_ENABLE, 1);

        w8(
            common + CFG_DEVICE_STATUS,
            STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
        );
    }

    // Device capacity (in 512-byte sectors) lives at device-config offset 0.
    // SAFETY: device_cfg is the mapped device-config MMIO.
    let capacity = unsafe { r64(device_cfg) };

    *DEVICES[dev].lock() = Some(VirtioBlk {
        notify,
        notify_mult: info.notify_mult,
        queue_notify_off,
        qsize,
        desc_va,
        avail_va,
        used_va,
        // Header at the start of the buffer frame, status byte just past it.
        hdr_va: buf_va,
        hdr_phys: buf_phys,
        status_va: buf_va + 16,
        status_phys: buf_phys + 16,
        last_used: 0,
        capacity,
        isr_va: isr,
        intr_line: info.intr_line,
        msix_vector,
        waiter: None,
    });

    let _ = writeln!(
        out,
        "plinth: virtio-blk[{dev}] ready (queue 0, size {qsize}, capacity {capacity} sectors)"
    );
    match msix_vector {
        Some(v) => {
            let _ = writeln!(out, "plinth: virtio-blk[{dev}] msix vector 0x{v:x}");
        }
        None => {
            let _ = writeln!(out, "plinth: virtio-blk[{dev}] msix unavailable, using INTx");
        }
    }
    Ok(())
}

/// True once device `dev` is brought up and ready for `read`. The block demo
/// and the block syscall gate on this. An out-of-range index is simply "not
/// ready".
pub fn ready(dev: usize) -> bool {
    DEVICES.get(dev).is_some_and(|m| m.lock().is_some())
}

/// Capacity of device `dev` in 512-byte sectors, or None if it is not present.
/// The boot code uses this to grant a BlockRange spanning the whole device.
pub fn capacity(dev: usize) -> Option<u64> {
    DEVICES.get(dev).and_then(|m| m.lock().as_ref().map(|d| d.capacity))
}

/// Read `count` 512-byte sectors starting at `sector` from device `dev` into
/// the frame at `data_phys` (the device DMAs there). The kernel block syscall
/// calls this; the caller is responsible for the buffer being at least
/// count*512 bytes and for validating the range (device, start, count) against
/// the holder's BlockRange capability.
pub fn read(dev: usize, sector: u64, count: u64, data_phys: u64) -> Result<(), &'static str> {
    let mut guard = DEVICES.get(dev).ok_or("invalid block device")?.lock();
    match guard.as_mut() {
        Some(d) => d.read_block_polled(sector, count, data_phys),
        None => Err("virtio-blk not initialised"),
    }
}

/// block_read(range_slot, frame_slot, sector_off, count): read `count` 512-byte
/// sectors -- starting `sector_off` sectors into the BlockRange capability at
/// `range_slot` -- into the frame named by `frame_slot`, BLOCKING until the
/// device completes. Reached from the `int 0x80` dispatch (op 5): a process that
/// must wait for I/O is parked and resumed by the completion IRQ, exactly like a
/// blocked IPC receiver -- which is why this needs the gate's resumable trap
/// frame, not the `syscall` fast path (Stage 4 S4a). `frame_ptr` is this call's
/// saved trap frame. Returns a status word (BLK_OK or a BLK_E_* code), never a
/// data value (the data DMAs into the caller's frame).
///
/// Two checks make this the exokernel multiplexing surface: the request must
/// fall inside the holder's range (so a BlockRange cannot read another libOS's
/// blocks), and the frame must be the holder's with RIGHT_WRITE (so the device
/// DMAs only into a frame the caller owns). The range start is added by the
/// kernel -- the holder names sectors relative to its range, never absolute.
pub fn block_read(
    range_slot: u64,
    frame_slot: u64,
    sector_off: u64,
    count: u64,
    frame_ptr: u64,
) -> u64 {
    // Bound the transfer: at least one sector, and it must fit the I/O frame.
    if count == 0 || count.saturating_mul(SECTOR_SIZE) > FRAME_SIZE {
        return BLK_E_BADARG;
    }

    // Resolve both capabilities under the CURRENT lock, then drop it before
    // touching the device or blocking -- nothing below needs CURRENT.
    let (dev, abs_sector, frame_phys) = {
        let cur = process::CURRENT.lock();
        let Some(proc) = cur.as_ref() else {
            return BLK_E_RIGHTS;
        };

        // The BlockRange: RIGHT_READ to read from the disk.
        let Ok(range) = proc.caps.lookup(range_slot as usize, RIGHT_READ) else {
            return BLK_E_RIGHTS;
        };
        let CapObject::BlockRange { dev, start, count: range_count } = range.object else {
            return BLK_E_RIGHTS;
        };
        // Multiplexing guarantee: [sector_off, sector_off+count) must lie inside
        // [0, range_count). Checked-add so a huge sector_off cannot wrap past it.
        let Some(end) = sector_off.checked_add(count) else {
            return BLK_E_RANGE;
        };
        if end > range_count {
            return BLK_E_RANGE;
        }

        // The I/O frame: RIGHT_WRITE, since the device DMAs into it.
        let Ok(frame) = proc.caps.lookup(frame_slot as usize, RIGHT_WRITE) else {
            return BLK_E_RIGHTS;
        };
        let CapObject::Frame { addr } = frame.object else {
            return BLK_E_RIGHTS;
        };

        (dev as usize, start + sector_off, addr)
    };

    // Post the request with completion interrupts enabled and record this
    // process as the device's waiter, then block. We are IF=0 from the int 0x80
    // entry, so the completion INTx cannot be delivered between recording the
    // waiter and blocking -- it stays latched at the PIC until the idle path's
    // `sti`, by which point this process is already Blocked. No lost wakeup, the
    // same IF=0 discipline IPC and event_recv rely on. The completion handler
    // (`complete_irq`) wakes us with BLK_OK / BLK_E_DEV in rax.
    {
        let mut guard = match DEVICES.get(dev) {
            Some(g) => g.lock(),
            None => return BLK_E_DEV,
        };
        let Some(d) = guard.as_mut() else {
            return BLK_E_DEV;
        };
        d.waiter = Some(scheduler::current_slot());
        d.post_request(abs_sector, count, frame_phys, true);
    } // drop the device lock BEFORE blocking -- block_current never returns here.
    scheduler::block_current(frame_ptr)
}

/// True if any device has a process blocked waiting for I/O completion. The
/// scheduler reads this (alongside `input::any_waiter`) to treat a process
/// blocked on disk as a legitimate idle -- the completion IRQ can still wake it
/// -- rather than an IPC deadlock (S4e).
pub fn any_waiter() -> bool {
    (0..pci::MAX_DEVICES).any(|dev| DEVICES[dev].lock().as_ref().is_some_and(|d| d.waiter.is_some()))
}

/// Service device `dev`'s completion interrupt, consume the completion, wake
/// the blocked reader with the status, and EOI. Shared by the per-device IRQ
/// stubs, on either path:
///
/// - **INTx** (`msix_vector` is `None`): ack at the device first -- read ISR
///   to deassert the level-triggered line (a read clears it, without which it
///   re-fires after EOI) -- then EOI `intr_line` at the controller.
/// - **MSI-X** (Stage A3, D7): no ISR read (the spec defines it as
///   meaningless once MSI-X is enabled -- a shared level-triggered status
///   register has nothing to deassert for a per-vector, edge-triggered
///   source) and no line to resolve; EOI is the same blanket Local APIC write
///   `irq::eoi` already makes under APIC mode for any vector, so passing `0`
///   is just "not a PIC line", never read on this path.
fn complete_irq(dev: usize) {
    let (woken, eoi_line) = {
        let Some(g) = DEVICES.get(dev) else {
            return;
        };
        let mut guard = g.lock();
        let Some(d) = guard.as_mut() else {
            return;
        };
        let eoi_line = match d.msix_vector {
            Some(_) => 0, // MSI-X: irq::eoi ignores this under APIC mode
            None => {
                // SAFETY: isr_va is the device's mapped ISR-status MMIO;
                // reading it acks and deasserts the device's INTx.
                let _isr = unsafe { r8(d.isr_va) };
                d.intr_line
            }
        };
        let woken = if d.completed() {
            let status = match d.take_completion() {
                Ok(()) => BLK_OK,
                Err(_) => BLK_E_DEV,
            };
            d.waiter.take().map(|w| (w, status))
        } else {
            None
        };
        (woken, eoi_line)
    };
    // Wake outside the device lock (wake_with touches the scheduler table, not
    // the device). NO_CAP in rdx -- block_read returns a status word only.
    if let Some((waiter, status)) = woken {
        scheduler::wake_with(waiter, status, 0, u64::MAX);
    }
    irq::eoi(eoi_line);
}

// The completion-IRQ stubs: one per device, because the two devices sit on
// distinct INTx lines (QEMU q35), so each vector maps to a known device and EOIs
// its own line. Raising pci::MAX_DEVICES means adding a stub here.
extern "x86-interrupt" fn blk_interrupt_dev0(_frame: InterruptStackFrame) {
    complete_irq(0);
}
extern "x86-interrupt" fn blk_interrupt_dev1(_frame: InterruptStackFrame) {
    complete_irq(1);
}

/// Install each present device's completion-IRQ handler and (on the INTx
/// fallback) unmask its line. Call once at boot AFTER the devices are brought
/// up (their vectors -- MSI-X or INTx -- are known) and AFTER the polled
/// selftests (which suppress completion interrupts): from here on, runtime
/// `block_read` blocks and is woken by these handlers. The IDT is already
/// loaded, so the handler is installed live into it
/// (interrupts::set_irq_handler); the IDTR points at the same table.
///
/// MSI-X (Stage A3, D7) needs no `irq::unmask` call: MSI/MSI-X delivers
/// straight to the LAPIC, bypassing the I/O APIC and its line-based
/// masking entirely -- the device starts asserting the vector the moment its
/// MSI-X table entry is unmasked, which `setup_msix` already did.
pub fn enable_completion_irqs() {
    for dev in 0..pci::MAX_DEVICES {
        let (vector, needs_unmask) = {
            let guard = DEVICES[dev].lock();
            match guard.as_ref() {
                Some(d) => match d.msix_vector {
                    Some(v) => (v, false),
                    None => (irq::VECTOR_BASE + d.intr_line, true),
                },
                None => continue,
            }
        };
        let handler: extern "x86-interrupt" fn(InterruptStackFrame) = match dev {
            0 => blk_interrupt_dev0,
            1 => blk_interrupt_dev1,
            _ => continue, // add a stub if MAX_DEVICES grows
        };
        interrupts::set_irq_handler(vector, handler);
        if needs_unmask {
            let line = vector - irq::VECTOR_BASE;
            irq::unmask(line);
        }
    }
}

/// Storage bring-up proof: read sector 0 of device `dev` into a scratch frame
/// and check it. `ramp` selects the expectation:
///
/// - `true` (the ramp/test disk, device 0): the sector must match the
///   deterministic byte ramp the xtask image is filled with (byte i == i%256).
/// - `false` (any other disk, e.g. the archive on device 1): the read must
///   succeed and the sector must NOT be the ramp -- proving it is a distinct,
///   separately readable device, without the kernel knowing that disk's format
///   (the on-disk layout is the FS libOS's business, not the kernel's).
///
/// Allocates and frees the scratch frame, so it leaves the frame pool as it
/// found it. Returns true on success.
pub fn selftest_read<W: Write>(out: &mut W, phys_offset: u64, dev: usize, ramp: bool) -> bool {
    let (phys, va) = match alloc_zeroed(phys_offset) {
        Ok(x) => x,
        Err(e) => {
            let _ = writeln!(out, "plinth: virtio-blk[{dev}] selftest: {e}");
            return false;
        }
    };

    let result = read(dev, 0, 1, phys);

    // Whether the first SECTOR_SIZE bytes match the ramp formula (byte i==i%256).
    let is_ramp = || {
        for i in 0..SECTOR_SIZE {
            // SAFETY: va is the scratch frame, mapped, and 512 bytes were just
            // DMA'd into it.
            if unsafe { r8(va + i) } != (i % 256) as u8 {
                return false;
            }
        }
        true
    };

    let ok = match result {
        Ok(()) if ramp => is_ramp(),
        // A non-ramp disk: the read succeeded; require it to differ from the
        // ramp so we know it is genuinely a second disk and not the same image.
        Ok(()) => !is_ramp(),
        Err(e) => {
            let _ = writeln!(out, "plinth: virtio-blk[{dev}] read error: {e}");
            false
        }
    };

    if let Some(fa) = FRAME_ALLOC.lock().as_mut() {
        let _ = fa.dealloc(phys);
    }

    match (ok, ramp) {
        (true, true) => {
            let _ = writeln!(out, "plinth: virtio-blk[{dev}] sector 0 read ok (ramp verified)");
        }
        (true, false) => {
            let _ = writeln!(out, "plinth: virtio-blk[{dev}] sector 0 read ok (distinct disk)");
        }
        (false, _) => {
            let _ = writeln!(out, "plinth: virtio-blk[{dev}] sector 0 read FAILED");
        }
    }
    ok
}
