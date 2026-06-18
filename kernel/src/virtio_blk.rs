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

use crate::frame_alloc::{FRAME_ALLOC, FRAME_SIZE};
use crate::memory;
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

// --- virtio-blk request (virtio 1.x, 5.2.6) ---
const VIRTIO_BLK_T_IN: u32 = 0; // read: device -> memory
const VIRTIO_BLK_S_OK: u8 = 0;

/// virtio block I/O is always in 512-byte units, independent of any device
/// logical block size. This is the sector unit `BlockRange` will count (D3a).
const SECTOR_SIZE: u64 = 512;

/// Cap the virtqueue at a small power of two: we only ever post a 3-descriptor
/// request at a time, so a large ring buys nothing and a smaller one keeps the
/// ring memory to a single frame each.
const QUEUE_SIZE_MAX: u16 = 64;

/// Bound on the completion poll. The device completes a single block read
/// almost immediately; a finite cap turns a wedged/absent device into a clean
/// fault instead of a kernel hang (D6).
const POLL_MAX: u64 = 50_000_000;

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
}

static VIRTIO_BLK: Mutex<Option<VirtioBlk>> = Mutex::new(None);

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
    /// Read `count` 512-byte sectors starting at `sector` into the buffer at
    /// `data_phys` (the device DMAs there; the buffer must hold count*512
    /// bytes). Synchronous: post a 3-descriptor request and poll to completion.
    fn read_block(&mut self, sector: u64, count: u64, data_phys: u64) -> Result<(), &'static str> {
        // SAFETY: all addresses below are kernel-mapped ring/MMIO/buffer
        // addresses set up in `init`; data_phys is a caller-owned frame. The
        // device touches only what these descriptors name.
        unsafe {
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

            // Publish into the available ring (flags@0, idx@2, ring@4), then
            // bump idx. Fence so the descriptors are visible before the index,
            // and the index before the notify.
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

            // Poll the used ring (idx@2) for completion, bounded.
            let mut spins = 0u64;
            while r16(self.used_va + 2) == self.last_used {
                spins += 1;
                if spins >= POLL_MAX {
                    return Err("virtio-blk read timed out");
                }
                core::hint::spin_loop();
            }
            fence(Ordering::SeqCst);
            self.last_used = self.last_used.wrapping_add(1);

            if r8(self.status_va) != VIRTIO_BLK_S_OK {
                return Err("virtio-blk read failed (device status)");
            }
        }
        Ok(())
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

/// Bring the device up: enable bus mastering, map its BAR, negotiate features,
/// stand up queue 0, and stash it for `read_block`. Call once at boot, after
/// `pci::discover`.
pub fn init<W: Write>(
    out: &mut W,
    info: &VirtioBlkInfo,
    phys_offset: u64,
) -> Result<(), &'static str> {
    let bar = info.common.bar;
    if info.isr.bar != bar || info.device.bar != bar || info.notify.bar != bar {
        return Err("virtio-blk spreads structures across multiple BARs (unsupported)");
    }

    // MMIO + DMA must be enabled before we touch registers or post a request.
    pci::enable_bus_master(info.loc);

    let bar_phys = pci::read_bar(info.loc, bar);
    let base = memory::map_kernel_mmio(bar_phys, required_span(info))?;
    let common = base + info.common.offset as u64;
    let notify = base + info.notify.offset as u64;
    let device_cfg = base + info.device.offset as u64;

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
        w16(common + CFG_QUEUE_ENABLE, 1);

        w8(
            common + CFG_DEVICE_STATUS,
            STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
        );
    }

    // Device capacity (in 512-byte sectors) lives at device-config offset 0.
    // SAFETY: device_cfg is the mapped device-config MMIO.
    let capacity = unsafe { r64(device_cfg) };

    *VIRTIO_BLK.lock() = Some(VirtioBlk {
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
    });

    let _ = writeln!(
        out,
        "plinth: virtio-blk ready (queue 0, size {qsize}, capacity {capacity} sectors)"
    );
    Ok(())
}

/// True once the device is brought up and ready for `read`. The block demo
/// and the block syscall gate on this.
pub fn ready() -> bool {
    VIRTIO_BLK.lock().is_some()
}

/// Read `count` 512-byte sectors starting at `sector` into the frame at
/// `data_phys` (the device DMAs there). The kernel block syscall calls this;
/// the caller is responsible for the buffer being at least count*512 bytes and
/// for validating the range against the holder's BlockRange capability.
pub fn read(sector: u64, count: u64, data_phys: u64) -> Result<(), &'static str> {
    let mut guard = VIRTIO_BLK.lock();
    match guard.as_mut() {
        Some(dev) => dev.read_block(sector, count, data_phys),
        None => Err("virtio-blk not initialised"),
    }
}

/// Stage 1 proof: read sector 0 into a scratch frame and verify it against the
/// deterministic byte ramp the xtask image is filled with (byte i == i % 256).
/// Allocates and frees the scratch frame, so it leaves the frame pool as it
/// found it. Returns true on success.
pub fn selftest_read<W: Write>(out: &mut W, phys_offset: u64) -> bool {
    let (phys, va) = match alloc_zeroed(phys_offset) {
        Ok(x) => x,
        Err(e) => {
            let _ = writeln!(out, "plinth: virtio-blk selftest: {e}");
            return false;
        }
    };

    let result = {
        let mut guard = VIRTIO_BLK.lock();
        match guard.as_mut() {
            Some(dev) => dev.read_block(0, 1, phys),
            None => Err("virtio-blk not initialised"),
        }
    };

    let ok = match result {
        Ok(()) => {
            let mut good = true;
            for i in 0..SECTOR_SIZE {
                // SAFETY: va is the scratch frame, mapped and 512 bytes were
                // just DMA'd into it.
                let b = unsafe { r8(va + i) };
                if b != (i % 256) as u8 {
                    good = false;
                    break;
                }
            }
            good
        }
        Err(e) => {
            let _ = writeln!(out, "plinth: virtio-blk read error: {e}");
            false
        }
    };

    if let Some(fa) = FRAME_ALLOC.lock().as_mut() {
        let _ = fa.dealloc(phys);
    }

    if ok {
        let _ = writeln!(out, "plinth: virtio-blk sector 0 read ok (ramp verified)");
    } else {
        let _ = writeln!(out, "plinth: virtio-blk sector 0 read FAILED");
    }
    ok
}
