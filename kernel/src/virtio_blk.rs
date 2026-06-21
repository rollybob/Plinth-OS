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

use crate::bkl;
use crate::frame_alloc::{FRAME_ALLOC, FRAME_SIZE};
use crate::{interrupts, irq, memory, scheduler};
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

/// Block I/O status codes. The data lands in the caller's frame, so status is
/// its own word and no read-back byte can be confused for an error. These ride
/// the CQ entry's `status` field (Design/async_rings.md s4) and are mirrored in
/// libplinth as BLK_*; the block I/O path is the ring ABI (`rings`), the
/// `block_read` syscall having been retired in ABI v2.4.
pub const BLK_OK: u64 = 0;
/// count is zero, or count*512 would overflow the I/O frame.
pub const BLK_E_BADARG: u64 = 1;
/// The request falls outside the holder's BlockRange (multiplexing guarantee).
pub const BLK_E_RANGE: u64 = 2;
/// Bad slot, wrong object kind, or a missing right on the range or frame cap.
pub const BLK_E_RIGHTS: u64 = 3;
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

/// Descriptors per request: a virtio-blk read is a 3-descriptor chain (the
/// 16-byte header the device reads, the data buffer it writes, the 1-byte status
/// it writes). The in-flight depth is `qsize / DESC_PER_REQ` requests.
const DESC_PER_REQ: u16 = 3;

/// Upper bound on concurrent in-flight requests, fixed by the largest queue we
/// accept (`QUEUE_SIZE_MAX`). Sizes the per-device pool arrays and the
/// header/status buffer layout; the actual usable count (`Inflights::slots`) is
/// `qsize / DESC_PER_REQ` for the device's negotiated `qsize`.
const MAX_SLOTS: usize = (QUEUE_SIZE_MAX / DESC_PER_REQ) as usize;

/// Bytes per request header in the shared buffer frame. The status byte regions
/// start after all `MAX_SLOTS` headers, so a slot's header and status never
/// overlap another slot's (the device writes each concurrently).
const HDR_BYTES: u64 = 16;
const STATUS_REGION_OFF: u64 = MAX_SLOTS as u64 * HDR_BYTES;

/// Where a completed request's result must go -- the routing the completion
/// handler reads out of the in-flight table.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Completion {
    /// Boot polled selftest: there is no scheduler yet, so nothing is parked;
    /// the poller reads the status directly from the drain.
    Poll,
    /// Ring path: post the result to ring `ring`'s CQ with this `user_data`
    /// cookie, and wake the ring's owner if it is parked in `ring_wait`.
    Ring { ring: usize, user_data: u64 },
}

/// Routing recorded for one in-flight request (Design/async_rings.md s5): the
/// completion handler reads `target` to route the result.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Inflight {
    pub(crate) target: Completion,
}

/// Per-device in-flight bookkeeping: a free pool of descriptor-chain slots plus
/// the routing entry for each live slot. This is the completion-demux core
/// (Design/async_rings.md section 5) factored as pure logic over plain arrays --
/// no MMIO -- so it is unit-testable host-side with a fake used ring, exactly
/// like `ipc::WaitQueue`. The MMIO (writing descriptors, reading the status
/// byte and used ring) stays in `VirtioBlk`; this owns only the bookkeeping.
///
/// Slot `i` owns the descriptor chain anchored at head `i * DESC_PER_REQ`, so a
/// head and its slot convert by multiply/divide. The device echoes the chain
/// head back in the used-ring `id`, which is the demux key.
pub(crate) struct Inflights {
    /// `routing[slot]` is `Some` iff that slot is live (submitted, not yet
    /// completed). Doubles as the liveness marker the completion path validates.
    routing: [Option<Inflight>; MAX_SLOTS],
    /// Free slot indices, as a stack: `free[..free_len]` are available.
    free: [u16; MAX_SLOTS],
    free_len: usize,
    /// Usable slot count for this device (`qsize / DESC_PER_REQ`, <= MAX_SLOTS).
    slots: usize,
}

impl Inflights {
    /// Build a pool of `slots` usable request slots (caller passes
    /// `qsize / DESC_PER_REQ`, capped at `MAX_SLOTS`). All slots start free.
    pub(crate) fn new(slots: usize) -> Self {
        let slots = slots.min(MAX_SLOTS);
        let mut free = [0u16; MAX_SLOTS];
        for i in 0..slots {
            free[i] = i as u16;
        }
        Inflights { routing: [None; MAX_SLOTS], free, free_len: slots, slots }
    }

    /// Claim a free slot, record its routing, and return the descriptor-chain
    /// head to post. `None` when the pool is full (in-flight at capacity).
    pub(crate) fn submit(&mut self, info: Inflight) -> Option<u16> {
        if self.free_len == 0 {
            return None;
        }
        self.free_len -= 1;
        let slot = self.free[self.free_len] as usize;
        self.routing[slot] = Some(info);
        Some(slot as u16 * DESC_PER_REQ)
    }

    /// Map a used-ring `id` (a chain head) to its slot, if it is a head we could
    /// have issued. Rejects a non-chain-aligned or out-of-range id.
    fn slot_of(&self, head: u16) -> Option<usize> {
        if head % DESC_PER_REQ != 0 {
            return None;
        }
        let slot = (head / DESC_PER_REQ) as usize;
        (slot < self.slots).then_some(slot)
    }

    /// Complete the request at chain `head`: take its routing and return the
    /// slot to the free pool. `None` if `head` is not a live, kernel-issued head
    /// (a device/spec violation: an id we never issued or already completed) --
    /// the caller drops it rather than waking a stale or wrong waiter.
    pub(crate) fn complete(&mut self, head: u16) -> Option<Inflight> {
        let slot = self.slot_of(head)?;
        let info = self.routing[slot].take()?;
        self.free[self.free_len] = slot as u16;
        self.free_len += 1;
        Some(info)
    }

    /// True if any slot is live. The scheduler treats a process blocked on disk
    /// as a legitimate idle (not a deadlock) while this holds.
    pub(crate) fn any_live(&self) -> bool {
        self.free_len < self.slots
    }
}

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
    /// One frame holding the per-slot request headers (16 B each, slot `i` at
    /// offset `i*HDR_BYTES`) followed by the per-slot status bytes (1 B each,
    /// slot `i` at `STATUS_REGION_OFF + i`). Per-slot so concurrent requests do
    /// not share the device-written status byte.
    buf_va: u64,
    buf_phys: u64,
    /// In-flight free pool + completion-demux routing (Design/async_rings.md s5).
    inflights: Inflights,
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
    // Per-slot header/status addresses in the shared buffer frame. Slot `i`'s
    // header is at `i*HDR_BYTES`; its status byte follows all the headers, at
    // `STATUS_REGION_OFF + i`.
    fn hdr_va(&self, slot: usize) -> u64 {
        self.buf_va + slot as u64 * HDR_BYTES
    }
    fn hdr_phys(&self, slot: usize) -> u64 {
        self.buf_phys + slot as u64 * HDR_BYTES
    }
    fn status_va(&self, slot: usize) -> u64 {
        self.buf_va + STATUS_REGION_OFF + slot as u64
    }
    fn status_phys(&self, slot: usize) -> u64 {
        self.buf_phys + STATUS_REGION_OFF + slot as u64
    }

    /// Post a 3-descriptor read of `count` 512-byte sectors starting at `sector`
    /// into the buffer at `data_phys` (the device DMAs there; the buffer must
    /// hold count*512 bytes), using the descriptor chain anchored at `head` (a
    /// slot the caller claimed from `inflights`), and notify the device. Does
    /// NOT wait: the caller either polls (`drain_completions`, the boot path) or
    /// blocks until the completion IRQ (the runtime path). `want_interrupt` sets
    /// the avail-ring NO_INTERRUPT flag accordingly.
    fn post_request(
        &mut self,
        head: u16,
        sector: u64,
        count: u64,
        data_phys: u64,
        want_interrupt: bool,
    ) {
        let slot = (head / DESC_PER_REQ) as usize;
        let hdr_phys = self.hdr_phys(slot);
        let status_phys = self.status_phys(slot);
        // SAFETY: all addresses below are kernel-mapped ring/MMIO/buffer
        // addresses set up in `init`; data_phys is a caller-owned frame. The
        // descriptor chain [head, head+1, head+2] is within the queue (head <=
        // (slots-1)*DESC_PER_REQ < qsize). The device touches only what these
        // descriptors name.
        unsafe {
            // Avail flags (offset 0): tell the device whether to raise a
            // completion interrupt for the requests we publish. Set before the
            // idx bump so the device sees it when it consumes this request.
            let flags = if want_interrupt { 0 } else { VIRTQ_AVAIL_F_NO_INTERRUPT };
            w16(self.avail_va, flags);

            // This slot's request header (device reads it) and status sentinel.
            w32(self.hdr_va(slot), VIRTIO_BLK_T_IN);
            w32(self.hdr_va(slot) + 4, 0);
            w64(self.hdr_va(slot) + 8, sector);
            w8(self.status_va(slot), 0xFF);

            // Three chained descriptors (each 16 B: addr, len, flags, next) at
            // this chain's head, linking head -> head+1 -> head+2.
            let d = self.desc_va + head as u64 * 16;
            w64(d, hdr_phys);
            w32(d + 8, 16);
            w16(d + 12, VIRTQ_DESC_F_NEXT);
            w16(d + 14, head + 1);

            w64(d + 16, data_phys);
            w32(d + 24, (count * SECTOR_SIZE) as u32);
            w16(d + 28, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);
            w16(d + 30, head + 2);

            w64(d + 32, status_phys);
            w32(d + 40, 1);
            w16(d + 44, VIRTQ_DESC_F_WRITE);
            w16(d + 46, 0);

            // Publish this chain head into the available ring (idx@2, ring@4),
            // then bump idx. Fence so the descriptors are visible before the
            // index, and the index before the notify.
            let avail_idx = r16(self.avail_va + 2);
            let ring_slot = (avail_idx % self.qsize) as u64;
            w16(self.avail_va + 4 + ring_slot * 2, head);
            fence(Ordering::SeqCst);
            w16(self.avail_va + 2, avail_idx.wrapping_add(1));
            fence(Ordering::SeqCst);

            // Notify queue 0.
            let notify_addr =
                self.notify + (self.queue_notify_off as u64) * (self.notify_mult as u64);
            w16(notify_addr, 0);
        }
    }

    /// Drain every newly-completed used-ring element (from `last_used` to the
    /// device's current used idx). For each: read the chain head the device
    /// echoes in `id`, read that slot's status byte, and route the completion
    /// through `inflights` (free the slot, take its routing). Records each
    /// completion's `(target, status)` into `out` for the caller to act on after
    /// dropping the device lock. Returns how many it recorded.
    ///
    /// This is the completion demux (Design/async_rings.md s5): the device's
    /// echoed head is the routing key, mapping one-to-one to the slot that
    /// issued it. Call under the device lock.
    fn drain_completions(&mut self, out: &mut [(Completion, u64)]) -> usize {
        // SAFETY: used_va is the mapped used ring; idx@2 and the 8-byte elements
        // at +4 are device-written. The fence orders the status read after the
        // device advanced the used ring (and thus wrote the status byte).
        let used_idx = unsafe { r16(self.used_va + 2) };
        let mut n = 0;
        while self.last_used != used_idx {
            fence(Ordering::SeqCst);
            let ring_slot = (self.last_used % self.qsize) as u64;
            // Used element: { id: u32, len: u32 }; id is the chain head.
            let head = unsafe { r32(self.used_va + 4 + ring_slot * 8) } as u16;

            // Read the status byte before returning the slot to the pool. Guard
            // the address on a valid head so a device that echoes garbage cannot
            // make us read outside the buffer frame.
            let status = match self.inflights.slot_of(head) {
                // SAFETY: status_va(slot) is this slot's mapped status byte.
                Some(slot) => {
                    if unsafe { r8(self.status_va(slot)) } == VIRTIO_BLK_S_OK {
                        BLK_OK
                    } else {
                        BLK_E_DEV
                    }
                }
                None => BLK_E_DEV,
            };

            // Route it. `complete` returns None for an id we never issued or
            // already completed (a device/spec violation) -- drop it rather than
            // wake a stale waiter.
            if let Some(info) = self.inflights.complete(head) {
                if n < out.len() {
                    out[n] = (info.target, status);
                    n += 1;
                }
            }
            self.last_used = self.last_used.wrapping_add(1);
        }
        n
    }

    /// Synchronous polled read for the boot path: there is no scheduler yet, so
    /// nothing can be blocked. Claim a slot, post with the completion interrupt
    /// suppressed, spin (bounded, faulting on timeout per D6) on the used ring,
    /// then drain the one completion.
    fn read_block_polled(
        &mut self,
        sector: u64,
        count: u64,
        data_phys: u64,
    ) -> Result<(), &'static str> {
        let head = self
            .inflights
            .submit(Inflight { target: Completion::Poll })
            .ok_or("virtio-blk free pool exhausted")?;
        self.post_request(head, sector, count, data_phys, false);
        let mut spins = 0u64;
        // SAFETY: used_va is the mapped used ring; reading idx@2 is a read.
        while unsafe { r16(self.used_va + 2) } == self.last_used {
            spins += 1;
            if spins >= POLL_MAX {
                self.inflights.complete(head); // do not leak the slot on timeout
                return Err("virtio-blk read timed out");
            }
            core::hint::spin_loop();
        }
        let mut done = [(Completion::Poll, 0u64); 1];
        if self.drain_completions(&mut done) == 0 {
            return Err("virtio-blk completion drain found nothing");
        }
        if done[0].1 == BLK_OK {
            Ok(())
        } else {
            Err("virtio-blk read failed (device status)")
        }
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

    // In-flight depth: how many 3-descriptor chains fit the negotiated queue.
    // One buffer frame holds all the per-slot headers + status bytes (at most
    // MAX_SLOTS*HDR_BYTES + MAX_SLOTS bytes, well under a 4 KiB frame).
    let slots = (qsize / DESC_PER_REQ) as usize;

    *DEVICES[dev].lock() = Some(VirtioBlk {
        notify,
        notify_mult: info.notify_mult,
        queue_notify_off,
        qsize,
        desc_va,
        avail_va,
        used_va,
        buf_va,
        buf_phys,
        inflights: Inflights::new(slots),
        last_used: 0,
        capacity,
        isr_va: isr,
        intr_line: info.intr_line,
        msix_vector,
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

/// Post one ring-submitted read into device `dev`: claim an in-flight slot
/// recording the CQ routing (`ring` + `user_data`), then post the 3-descriptor
/// chain at the caller's frame physical address with completion interrupts
/// enabled. Returns `Ok(())` once posted, or `Err(())` when the device's
/// in-flight pool is full -- the ring drain treats that as backpressure (R6) and
/// stops, leaving the remaining SQ entries for the libOS to resubmit. The
/// completion IRQ (`complete_irq`) routes the result to the ring's CQ.
///
/// The device-facing half of `rings::ring_submit`: the caller has already run
/// the two cap-checks and resolved `abs_sector`/`frame_phys`, so this only owns
/// the slot claim + virtqueue post. The IF=0 no-lost-wakeup argument lives in
/// `ring_wait`: the completion IRQ cannot wake a ring before the libOS parks on
/// it, the same discipline the IPC ops rely on.
pub fn ring_post(
    dev: usize,
    abs_sector: u64,
    count: u64,
    frame_phys: u64,
    ring: usize,
    user_data: u64,
) -> Result<(), ()> {
    let mut guard = DEVICES.get(dev).ok_or(())?.lock();
    let d = guard.as_mut().ok_or(())?;
    let head = d
        .inflights
        .submit(Inflight { target: Completion::Ring { ring, user_data } })
        .ok_or(())?;
    d.post_request(head, abs_sector, count, frame_phys, true);
    Ok(())
}

/// True if any device has a process blocked waiting for I/O completion. The
/// scheduler reads this (alongside `input::any_waiter`) to treat a process
/// blocked on disk as a legitimate idle -- the completion IRQ can still wake it
/// -- rather than an IPC deadlock (S4e).
pub fn any_waiter() -> bool {
    (0..pci::MAX_DEVICES)
        .any(|dev| DEVICES[dev].lock().as_ref().is_some_and(|d| d.inflights.any_live()))
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
    // One completion IRQ can cover several finished requests; drain them all,
    // then route each demuxed completion.
    let mut done = [(Completion::Poll, 0u64); MAX_SLOTS];
    let (n, eoi_line) = {
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
        (d.drain_completions(&mut done), eoi_line)
    };
    // Route each completion after dropping the device lock. Posting to a CQ
    // touches the ring frame + RINGS table, not the device, and the whole IRQ
    // runs under the BKL (which serialises CQ writes), so the per-device lock is
    // not needed for it. Collect the ring owners to wake, then wake them last
    // (wake_with touches the scheduler table).
    let mut wakes = [0usize; MAX_SLOTS];
    let mut wn = 0;
    for &(target, status) in &done[..n] {
        if let Completion::Ring { ring, user_data } = target {
            if let Some(owner) = crate::rings::post_completion(ring, user_data, status) {
                wakes[wn] = owner;
                wn += 1;
            }
        }
        // Completion::Poll cannot occur here: the boot poller drains its own
        // request synchronously, before completion IRQs are enabled.
    }
    for &w in &wakes[..wn] {
        // Woken from ring_wait: it just returns 0 and the libOS reaps the CQ.
        scheduler::wake_with(w, 0, 0, u64::MAX);
    }
    irq::eoi(eoi_line);
}

// The completion-IRQ stubs: one per device, because the two devices sit on
// distinct INTx lines (QEMU q35), so each vector maps to a known device and EOIs
// its own line. Raising pci::MAX_DEVICES means adding a stub here. BKL (D4):
// acquired/released around complete_irq -- it calls scheduler::wake_with,
// which touches the scheduler table.
extern "x86-interrupt" fn blk_interrupt_dev0(_frame: InterruptStackFrame) {
    bkl::acquire();
    complete_irq(0);
    unsafe { bkl::release() };
}
extern "x86-interrupt" fn blk_interrupt_dev1(_frame: InterruptStackFrame) {
    bkl::acquire();
    complete_irq(1);
    unsafe { bkl::release() };
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
