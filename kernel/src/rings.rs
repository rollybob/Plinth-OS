//! Async completion rings (Design/async_rings.md): per-process shared-memory
//! submission/completion queues for block I/O.
//!
//! The library OS produces SQ entries and reaps CQ entries from memory; the
//! kernel drains the SQ on a doorbell (`ring_submit`), bridges each logical
//! request into a virtqueue descriptor chain -- it is the only writer of
//! physical descriptor addresses, so the device stays safely shared with no
//! IOMMU (R3) -- and posts completions into the CQ from the device IRQ
//! (`virtio_blk::complete_irq` -> `post_completion`). A request names
//! capability *slots*, never addresses; the two cap-checks in the drain are the
//! exokernel multiplexing surface, lifted verbatim from the retired `block_read`
//! syscall.
//!
//! Stage 2 (this module): the ring mechanism + the in-context drain. The
//! reference `no_std` executor and the many-in-flight demo are Stage 3; the
//! ABI here is shaped so they layer on without changing it.

use core::ptr::addr_of_mut;
use core::sync::atomic::{fence, Ordering};

use crate::capability::{CapObject, RIGHT_READ, RIGHT_WRITE};
use crate::frame_alloc::FRAME_SIZE;
use crate::process;
use crate::scheduler;
use crate::virtio_blk::{
    self, BLK_E_BADARG, BLK_E_DEV, BLK_E_RANGE, BLK_E_RIGHTS,
};

/// Returned by the non-blocking ring syscalls on error (mirrors `syscall::ERR`).
pub const ERR: u64 = u64::MAX;

/// One bound ring per (process, BlockRange); 8 matches the endpoint pool scale.
const MAX_RINGS: usize = 8;

/// SQ/CQ entry sizes (Design s4). SQ entry: op,flags,range_slot,frame_slot
/// (4x u32), sector_off, user_data (2x u64). CQ entry: user_data (u64), status
/// (i32), reserved (u32).
const SQ_ENTRY_SIZE: u64 = 32;
const CQ_ENTRY_SIZE: u64 = 16;

/// Shared ring header (both SQ and CQ): head (consumer index), tail (producer
/// index), mask (entries - 1). Indices are free-running u32; the slot for an
/// index is `index & mask`. The kernel keeps its own authoritative copy of the
/// index it produces (SQ head, CQ tail) in `Ring` and mirrors it to the frame
/// header for the libOS; it reads the libOS-produced index straight from the
/// frame.
const HDR_HEAD: u64 = 0;
const HDR_TAIL: u64 = 4;
const HDR_MASK: u64 = 8;
const RING_HDR_SIZE: u64 = 16;

/// Largest SQ that fits one frame: HDR + entries * 32 <= 4096, power of two ->
/// 64. (A 64-entry CQ of 16-byte entries is 1040 bytes, well within a frame.)
const MAX_ENTRIES: u64 = 64;

/// SQ entry `op` field. v1 was reads only (R1); event rings add two multishot
/// control ops (event_rings.md s4).
const RING_OP_READ: u32 = 0;
/// Open a multishot event subscription: the `range_slot` field names an
/// `EventSource` cap (RIGHT_READ), `user_data` is the stream cookie echoed in
/// every event completion. Armed until a matching RING_OP_CANCEL.
const RING_OP_EVENT_SUB: u32 = 1;
/// Cancel a live subscription named by its `user_data` cookie on this ring.
const RING_OP_CANCEL: u32 = 2;

/// virtio block I/O is always in 512-byte units (mirrors `virtio_blk`).
const SECTOR_SIZE: u64 = 512;

/// A bound ring: two libOS frames (SQ readable by the kernel, CQ writable by
/// the kernel) plus the kernel's authoritative producer/consumer indices.
/// Bound to its `owner` process at register; `ring_submit`/`ring_wait` from any
/// other process is refused (ring confinement, s9).
#[derive(Clone, Copy)]
struct Ring {
    in_use: bool,
    owner: usize,
    /// Physical base of the SQ / CQ frame (the kernel maps via phys_offset).
    sq_phys: u64,
    cq_phys: u64,
    mask: u32,
    /// Kernel's authoritative SQ consumer index (entries it has drained).
    sq_head: u32,
    /// Kernel's authoritative CQ producer index (completions it has posted).
    cq_tail: u32,
    /// True while `owner` is parked in `ring_wait` on this ring.
    waiting: bool,
}

impl Ring {
    const fn empty() -> Self {
        Ring {
            in_use: false,
            owner: 0,
            sq_phys: 0,
            cq_phys: 0,
            mask: 0,
            sq_head: 0,
            cq_tail: 0,
            waiting: false,
        }
    }
}

static mut RINGS: [Ring; MAX_RINGS] = [const { Ring::empty() }; MAX_RINGS];

// --- volatile ring-frame accessors (the frames are libOS-shared memory) ---
#[inline]
unsafe fn r32(a: u64) -> u32 {
    core::ptr::read_volatile(a as *const u32)
}
#[inline]
unsafe fn r64(a: u64) -> u64 {
    core::ptr::read_volatile(a as *const u64)
}
#[inline]
unsafe fn w32(a: u64, v: u32) {
    core::ptr::write_volatile(a as *mut u32, v)
}
#[inline]
unsafe fn w64(a: u64, v: u64) {
    core::ptr::write_volatile(a as *mut u64, v)
}

/// Kernel virtual address of a ring frame's physical base.
fn va(phys: u64) -> u64 {
    process::phys_offset() + phys
}

/// ring_register(sq_slot, cq_slot, entries): bind two caller-owned frames as an
/// SQ/CQ pair and return a Ring capability slot. `entries` must be a power of
/// two that fits one frame. Each frame must be the caller's `Frame` with
/// RIGHT_READ|RIGHT_WRITE (the kernel reads the SQ and writes the CQ). Zeroes
/// the ring headers and binds the ring to CURRENT. Returns the cap slot, or ERR.
pub fn ring_register(sq_slot: u64, cq_slot: u64, entries: u64) -> u64 {
    if entries == 0 || entries > MAX_ENTRIES || !entries.is_power_of_two() {
        return ERR;
    }
    let entries = entries as u32;
    let owner = scheduler::current_slot();

    let mut cur = process::current().lock();
    let Some(proc) = cur.as_mut() else {
        return ERR;
    };

    // Both frames must be the caller's, readable+writable.
    let Ok(sq) = proc.caps.lookup(sq_slot as usize, RIGHT_READ | RIGHT_WRITE) else {
        return ERR;
    };
    let CapObject::Frame { addr: sq_phys } = sq.object else {
        return ERR;
    };
    let Ok(cq) = proc.caps.lookup(cq_slot as usize, RIGHT_READ | RIGHT_WRITE) else {
        return ERR;
    };
    let CapObject::Frame { addr: cq_phys } = cq.object else {
        return ERR;
    };
    if sq_phys == cq_phys {
        return ERR; // SQ and CQ must be distinct frames
    }

    // Claim a ring slot. SAFETY: single CPU, IF masked (syscall SFMask); RINGS
    // is touched only from the IF=0 syscall/IRQ paths.
    let id = {
        let rings = unsafe { &mut *addr_of_mut!(RINGS) };
        let Some(id) = rings.iter().position(|r| !r.in_use) else {
            return ERR;
        };
        rings[id] = Ring {
            in_use: true,
            owner,
            sq_phys,
            cq_phys,
            mask: entries - 1,
            sq_head: 0,
            cq_tail: 0,
            waiting: false,
        };
        id
    };

    // Zero the headers and publish the mask. SAFETY: both frames are the
    // caller's, kernel-mapped at phys_offset; single-CPU IF=0 means nothing
    // else touches them here.
    unsafe {
        for base in [va(sq_phys), va(cq_phys)] {
            w32(base + HDR_HEAD, 0);
            w32(base + HDR_TAIL, 0);
            w32(base + HDR_MASK, entries - 1);
            w32(base + 12, 0);
        }
    }

    match proc.caps.mint(CapObject::Ring { id }, RIGHT_READ | RIGHT_WRITE) {
        Ok(slot) => slot as u64,
        Err(_) => {
            // No room for the cap: release the ring slot we just claimed.
            // SAFETY: as above.
            unsafe {
                (*addr_of_mut!(RINGS))[id].in_use = false;
            }
            ERR
        }
    }
}

/// Resolve a Ring capability slot to its ring id, requiring the caller both
/// holds the cap (RIGHT_READ|RIGHT_WRITE) and is the bound owner -- ring
/// confinement (s9): a handle another process does not own is refused.
fn ring_id_for(slot: u64) -> Option<usize> {
    let owner = scheduler::current_slot();
    let id = {
        let cur = process::current().lock();
        let proc = cur.as_ref()?;
        let cap = proc.caps.lookup(slot as usize, RIGHT_READ | RIGHT_WRITE).ok()?;
        let CapObject::Ring { id } = cap.object else {
            return None;
        };
        id
    };
    // SAFETY: single CPU, IF=0.
    let rings = unsafe { &*addr_of_mut!(RINGS) };
    (id < MAX_RINGS && rings[id].in_use && rings[id].owner == owner).then_some(id)
}

/// What happened to one SQ entry during the drain.
enum Outcome {
    /// Posted to the device, or completed immediately with an error status.
    /// Either way the entry is consumed (its SQ slot is freed) -- a bad entry
    /// gets a CQ completion and never wedges the ring.
    Consumed,
    /// The device's in-flight pool is full: stop draining and leave this and the
    /// remaining entries unconsumed (backpressure, R6). The libOS resubmits
    /// after it reaps.
    Full,
}

/// ring_submit(ring): the doorbell. Drain the SQ -- for each entry, copy it out
/// (read-once, s9 TOCTOU), run the two cap-checks against CURRENT, and post it
/// to the device -- and return the number of entries consumed (posted or
/// completed-with-error). May be < SQ depth under backpressure. Never blocks.
pub fn ring_submit(ring_slot: u64) -> u64 {
    let Some(id) = ring_id_for(ring_slot) else {
        return ERR;
    };

    let (sq_base, mut sq_head, mask) = {
        // SAFETY: single CPU, IF=0.
        let rings = unsafe { &*addr_of_mut!(RINGS) };
        (va(rings[id].sq_phys), rings[id].sq_head, rings[id].mask)
    };
    // SAFETY: sq_base is the caller's mapped SQ frame; the header is in-frame.
    let sq_tail = unsafe { r32(sq_base + HDR_TAIL) };

    let mut consumed = 0u64;
    while sq_head != sq_tail {
        let slot = (sq_head & mask) as u64;
        let e = sq_base + RING_HDR_SIZE + slot * SQ_ENTRY_SIZE;
        // Read-once: pull every field into kernel locals before validating, so
        // a concurrent libOS rewrite cannot redirect the DMA after the check.
        // SAFETY: `e` is within the mapped SQ frame (slot < entries).
        let (op, flags, range_slot, frame_slot, sector_off, user_data) = unsafe {
            (r32(e), r32(e + 4), r32(e + 8), r32(e + 12), r64(e + 16), r64(e + 24))
        };
        let count = (flags & 0xFFFF) as u64;

        match post_entry(id, op, count, range_slot, frame_slot, sector_off, user_data) {
            Outcome::Consumed => {
                consumed += 1;
                sq_head = sq_head.wrapping_add(1);
            }
            Outcome::Full => break,
        }
    }

    // Publish the consumed SQ head: kernel-authoritative + frame mirror so the
    // libOS can reuse the freed SQ slots.
    // SAFETY: single CPU, IF=0; sq_base is the mapped SQ frame.
    unsafe {
        (*addr_of_mut!(RINGS))[id].sq_head = sq_head;
        w32(sq_base + HDR_HEAD, sq_head);
    }
    consumed
}

/// Dispatch one SQ entry (read-once kernel copy) by `op`. READ is the block
/// path; EVENT_SUB / CANCEL are the multishot event-subscription control ops
/// (event_rings.md s4). An unknown op is completed with an error and consumed,
/// so a malformed entry never wedges the ring.
fn post_entry(
    id: usize,
    op: u32,
    count: u64,
    range_slot: u32,
    frame_slot: u32,
    sector_off: u64,
    user_data: u64,
) -> Outcome {
    match op {
        RING_OP_READ => post_read(id, count, range_slot, frame_slot, sector_off, user_data),
        // For EVENT_SUB the `range_slot` field carries the EventSource cap slot.
        RING_OP_EVENT_SUB => post_event_sub(id, range_slot, user_data),
        RING_OP_CANCEL => post_cancel(id, user_data),
        _ => finish(id, user_data, BLK_E_BADARG),
    }
}

/// Validate one block-read entry against CURRENT and post it to the device.
/// The cap-checks are the exokernel multiplexing surface, lifted verbatim from
/// the retired `block_read`: the request must lie inside the holder's BlockRange
/// (RIGHT_READ), and the I/O frame must be the holder's with RIGHT_WRITE (the
/// device DMAs into it). A failed check posts an immediate CQ error completion
/// and consumes the entry.
fn post_read(
    id: usize,
    count: u64,
    range_slot: u32,
    frame_slot: u32,
    sector_off: u64,
    user_data: u64,
) -> Outcome {
    if count == 0 || count.saturating_mul(SECTOR_SIZE) > FRAME_SIZE {
        post_status(id, user_data, BLK_E_BADARG);
        return Outcome::Consumed;
    }

    // Resolve both capabilities against CURRENT, yielding the error status on
    // failure. In-context drain (R2/R3): CURRENT is the submitter, so these are
    // the right capability tables with no rebinding. The lock drops at the end
    // of this block, before any CQ completion is posted.
    let resolved: Result<(usize, u64, u64), u64> = {
        let cur = process::current().lock();
        (|| {
            let proc = cur.as_ref().ok_or(BLK_E_RIGHTS)?;
            // The BlockRange: RIGHT_READ, and the request must lie inside it.
            let range = proc.caps.lookup(range_slot as usize, RIGHT_READ).map_err(|_| BLK_E_RIGHTS)?;
            let CapObject::BlockRange { dev, start, count: range_count } = range.object else {
                return Err(BLK_E_RIGHTS);
            };
            let end = sector_off.checked_add(count).ok_or(BLK_E_RANGE)?;
            if end > range_count {
                return Err(BLK_E_RANGE);
            }
            // The I/O frame: RIGHT_WRITE, since the device DMAs into it.
            let frame = proc.caps.lookup(frame_slot as usize, RIGHT_WRITE).map_err(|_| BLK_E_RIGHTS)?;
            let CapObject::Frame { addr } = frame.object else {
                return Err(BLK_E_RIGHTS);
            };
            Ok((dev as usize, start + sector_off, addr))
        })()
    };

    let (dev, abs_sector, frame_phys) = match resolved {
        Ok(t) => t,
        Err(status) => return finish(id, user_data, status),
    };

    if !virtio_blk::ready(dev) {
        return finish(id, user_data, BLK_E_DEV);
    }

    // Post into the device, recording the CQ routing for this request. Err means
    // the device free pool is full -- backpressure, leave the entry unconsumed.
    match virtio_blk::ring_post(dev, abs_sector, count, frame_phys, id, user_data) {
        Ok(()) => Outcome::Consumed,
        Err(()) => Outcome::Full,
    }
}

/// Post an immediate completion and report the entry consumed. A small helper so
/// the validation arms read as straight-line early returns.
fn finish(id: usize, user_data: u64, status: u64) -> Outcome {
    post_status(id, user_data, status);
    Outcome::Consumed
}

/// Write a {user_data, status} completion into ring `id`'s CQ and bump the CQ
/// tail (the kernel is the CQ producer). Returns `Some(owner)` if the owner is
/// parked in `ring_wait` and must be woken (the wait is cleared), else `None`.
/// Called from the completion IRQ (`virtio_blk::complete_irq`) and, for
/// immediate error completions, from the submit drain. Both run under the BKL.
///
/// A released ring (owner exited) drops the completion. The block CQ is sized by
/// the libOS to hold the device's in-flight depth, so a reaping libOS never
/// overflows it -- hence no full-check here (`drop_if_full = false`). The
/// producer-initiated event path, which can outrun a slow reader, uses
/// `post_event` instead.
pub fn post_completion(id: usize, user_data: u64, status: u64) -> Option<usize> {
    cq_post(id, user_data, status, false).1
}

/// Post an event completion into ring `id`'s CQ, dropping the newest event if
/// the CQ is full (the producer-initiated backpressure the block path never
/// needs, event_rings.md s5). Returns `(admitted, wake)`: `admitted` is false
/// iff the event was dropped on a full CQ (the caller bumps the subscription's
/// dropped count); `wake` is the owner to wake if it was parked in `ring_wait`.
fn post_event(id: usize, user_data: u64, status: u32) -> (bool, Option<usize>) {
    cq_post(id, user_data, status as u64, true)
}

/// The shared CQ producer. Writes one {user_data, status} completion at the CQ
/// tail and publishes the bumped tail. With `drop_if_full` it first checks the
/// CQ against the libOS-owned head and, when full, drops the newest completion
/// rather than lapping an unreaped entry (silent loss is the event path's
/// correct failure mode; the reader learns of it via the drop flag). Returns
/// `(admitted, wake)`: `admitted` is false only on a full-CQ drop; `wake` is the
/// owner to wake if it was parked in `ring_wait`. A released ring drops the
/// completion (admitted = true: not backpressure, nothing to count).
fn cq_post(id: usize, user_data: u64, status: u64, drop_if_full: bool) -> (bool, Option<usize>) {
    // SAFETY: single CPU, IF=0 (IRQ/syscall under BKL); RINGS is only touched
    // from these paths.
    let rings = unsafe { &mut *addr_of_mut!(RINGS) };
    if id >= MAX_RINGS || !rings[id].in_use {
        return (true, None);
    }
    let cq_base = va(rings[id].cq_phys);
    let mask = rings[id].mask;
    let tail = rings[id].cq_tail;
    if drop_if_full {
        let entries = mask.wrapping_add(1);
        // CQ head is the libOS consumer index, read from the shared frame.
        // SAFETY: cq_base header is in the mapped CQ frame.
        let head = unsafe { r32(cq_base + HDR_HEAD) };
        if tail.wrapping_sub(head) >= entries {
            return (false, None); // CQ full: drop the newest event
        }
    }
    let slot = (tail & mask) as u64;
    let e = cq_base + RING_HDR_SIZE + slot * CQ_ENTRY_SIZE;
    // SAFETY: `e` is within the caller's mapped CQ frame (slot < entries).
    unsafe {
        w64(e, user_data);
        w32(e + 8, status as u32);
        w32(e + 12, 0);
    }
    let new_tail = tail.wrapping_add(1);
    rings[id].cq_tail = new_tail;
    // Order the entry write before the visible tail bump: the libOS reads the
    // tail, then the entry it points past.
    fence(Ordering::SeqCst);
    // SAFETY: cq_base header is in the mapped CQ frame.
    unsafe { w32(cq_base + HDR_TAIL, new_tail) };

    let wake = if rings[id].waiting {
        rings[id].waiting = false;
        Some(rings[id].owner)
    } else {
        None
    };
    (true, wake)
}

/// Post a completion during the submit drain (the submitter is running, never
/// parked), so the wake target is always `None` -- ignore it.
fn post_status(id: usize, user_data: u64, status: u64) {
    let _ = post_completion(id, user_data, status);
}

/// Arm a multishot subscription: gate it on the `EventSource` cap at
/// `source_slot` (RIGHT_READ -- the multiplexing check, `input::source_for`),
/// then record it against ring `id` under `user_data`. A successful subscribe
/// posts NO completion (events arrive later); a bad source cap, or a full/
/// duplicate subscription pool, posts an event-error completion the shim reads.
/// Always consumes the entry.
fn post_event_sub(id: usize, source_slot: u32, user_data: u64) -> Outcome {
    let Some(source) = crate::input::source_for(source_slot as u64) else {
        return finish_event_err(id, user_data);
    };
    // SAFETY: single CPU, IF=0 under the BKL; SUBSCRIPTIONS is touched only on
    // the submit-drain, event-delivery (record), and ring-release paths.
    let subs = unsafe { &mut *addr_of_mut!(SUBSCRIPTIONS) };
    if subs.subscribe(source, id, user_data).is_some() {
        Outcome::Consumed
    } else {
        finish_event_err(id, user_data)
    }
}

/// Cancel the subscription named by `user_data` on ring `id` (owner-scoped: the
/// caller resolved `id` from its own Ring cap). No completion; always consumed.
fn post_cancel(id: usize, user_data: u64) -> Outcome {
    // SAFETY: as post_event_sub.
    let subs = unsafe { &mut *addr_of_mut!(SUBSCRIPTIONS) };
    subs.cancel(id, user_data);
    Outcome::Consumed
}

/// Post an event-subscription error completion and consume the entry. The status
/// is `EVENT_SUB_ERR` (0): a real event always packs a nonzero kind byte
/// (`Event::pack`), so a zero status is the shim's unambiguous "not an event ->
/// subscribe failed" signal (event_rings.md s5).
fn finish_event_err(id: usize, user_data: u64) -> Outcome {
    post_status(id, user_data, EVENT_SUB_ERR);
    Outcome::Consumed
}

/// ring_wait(ring): block until the ring's CQ has at least one unreaped
/// completion (R5: CQ non-empty, not a specific user_data -- the libOS demuxes).
/// Returns 0 once woken; the libOS then reaps from the CQ in memory. On the
/// `int 0x80` gate because it parks and resumes the process.
pub fn ring_wait(ring_slot: u64, frame_ptr: u64) -> u64 {
    let Some(id) = ring_id_for(ring_slot) else {
        return ERR;
    };

    // If completions are already pending, do not block -- let the libOS reap.
    // We are IF=0 from the int 0x80 entry, so a completion cannot land between
    // this check and block_current: the completion IRQ stays latched until the
    // idle path's `sti`, by which point this process is already Blocked. Same
    // no-lost-wakeup discipline the retired block_read and the IPC ops rely on.
    {
        // SAFETY: single CPU, IF=0.
        let rings = unsafe { &mut *addr_of_mut!(RINGS) };
        let cq_base = va(rings[id].cq_phys);
        // CQ head is libOS-written (its consumer index); tail is ours.
        // SAFETY: cq_base header is in the mapped CQ frame.
        let head = unsafe { r32(cq_base + HDR_HEAD) };
        if rings[id].cq_tail != head {
            return 0;
        }
        rings[id].waiting = true;
    }
    scheduler::block_current(frame_ptr)
}

/// Release ring `id` -- process teardown dropping its Ring capability. Any
/// requests still in flight against it complete into a released slot, which
/// `cq_post` drops; and every subscription routing into its CQ is dropped, so a
/// reused slot inherits no stale subscription. Idempotent.
pub fn release(id: usize) {
    // SAFETY: single CPU, IF=0 (teardown runs under the BKL).
    let rings = unsafe { &mut *addr_of_mut!(RINGS) };
    if id < MAX_RINGS {
        rings[id] = Ring::empty();
    }
    // SAFETY: as above; SUBSCRIPTIONS is touched only on these IF=0/BKL paths.
    let subs = unsafe { &mut *addr_of_mut!(SUBSCRIPTIONS) };
    subs.release_ring(id);
}

// --- event subscriptions: the multishot routing core (event_rings.md) --------
//
// Input is producer-initiated -- a keystroke answers no request -- so it rides
// the ring as a *multishot* subscription, not a one-shot read (event_rings.md
// s2): one RING_OP_EVENT_SUB arms a standing subscription naming an EventSource,
// and every event on that source posts a completion into the subscribing ring's
// CQ tagged with the subscription's user_data, until a RING_OP_CANCEL. The SQ
// thus carries control (subscribe, cancel), not one entry per event.
//
// The table is the routing + CQ-full-backpressure core, factored as pure logic
// over a fixed pool -- no volatile CQ frame, no IRQ, no scheduler -- so it is
// unit-testable host-side exactly like `virtio_blk::Inflights` (the block-
// completion demux). The live path: `input::record` (keyboard IRQ + the
// synthetic scaffold) calls `deliver_event`, which routes through this table and
// posts each event with `post_event` (drop-newest on a full CQ), waking any
// owner parked in `ring_wait`.

/// The single global subscription table. Touched only from the submit drain
/// (subscribe/cancel), event delivery (`deliver_event`), and ring release --
/// all IF=0 under the BKL, like `RINGS`.
static mut SUBSCRIPTIONS: Subscriptions = Subscriptions::new();

/// Status written for a failed `RING_OP_EVENT_SUB` (bad source cap, or a full/
/// duplicate pool). Zero is unambiguous: a real event packs a nonzero kind byte
/// (`Event::pack`), so the shim reads a zero-kind status as "subscribe failed".
const EVENT_SUB_ERR: u64 = 0;

/// Route a packed input `event` to every ring subscribed to `source`: post it
/// into each subscriber's CQ (dropping the newest on a full CQ and counting it)
/// and wake any owner parked in `ring_wait`. Called from `input::record` under
/// the BKL. The wakes are collected and performed after the routing, like
/// `virtio_blk::complete_irq`, since `wake_with` touches the scheduler table.
pub fn deliver_event(source: usize, event: u32) {
    let mut wakes = [0usize; MAX_SUBS];
    let mut wn = 0;
    {
        // SAFETY: single CPU, IF=0 under the BKL; see SUBSCRIPTIONS.
        let subs = unsafe { &mut *addr_of_mut!(SUBSCRIPTIONS) };
        subs.deliver(source, event, |ring, user_data, status| {
            let (admitted, wake) = post_event(ring, user_data, status);
            if let Some(w) = wake {
                wakes[wn] = w;
                wn += 1;
            }
            admitted
        });
    }
    for &w in &wakes[..wn] {
        // Woken from ring_wait: it returns 0 and the libOS reaps the CQ.
        scheduler::wake_with(w, 0, 0, u64::MAX);
    }
}

/// True if any ring holds a live event subscription -- someone a keystroke could
/// wake. The scheduler reads this (via `input::any_waiter`) to treat a process
/// parked in `ring_wait` on an event subscription as a legitimate idle.
pub fn any_event_sub() -> bool {
    // SAFETY: single CPU, IF=0 under the BKL.
    let subs = unsafe { &*addr_of_mut!(SUBSCRIPTIONS) };
    subs.any()
}

/// Subscription pool size (S8): a few subscriptions per ring -- one ring may
/// subscribe several sources at once (keyboard, later a mouse, ...). A small
/// fixed pool, scaled to `MAX_RINGS`, matching the endpoint/ring pool style.
pub(crate) const MAX_SUBS: usize = MAX_RINGS * 4;

/// Drop-flag bit in an event completion's `status` (S5): set on the next event
/// successfully posted after one or more were dropped on a full CQ, so a slow
/// reader learns it missed events between the last reaped completion and this
/// one. It overlays the always-zero high bits of a packed key event
/// (`Event::pack` puts the make/break value, 0 or 1, in bits 24..32), so it
/// never collides with a real event field. (event_rings.md s4/s5.)
pub(crate) const EVENT_DROPPED: u32 = 1 << 31;

/// One multishot subscription: every event on `source` posts a completion into
/// ring `ring`'s CQ tagged with `user_data`. `dropped` is the sticky count of
/// events lost to a full CQ since the reader last observed the drop flag (D5,
/// relocated from the in-kernel `EventRing` onto the CQ).
#[derive(Clone, Copy, PartialEq, Eq)]
struct Subscription {
    source: usize,
    ring: usize,
    user_data: u64,
    dropped: u32,
}

/// The subscription table: the event path's routing core, pure data over a
/// fixed pool (mirroring `Inflights`). Owns only the bookkeeping -- the actual
/// CQ post is supplied by the caller (`deliver`'s closure), so the live volatile
/// CQ write stays in `post_event` and the test substitutes a fake CQ.
pub(crate) struct Subscriptions {
    subs: [Option<Subscription>; MAX_SUBS],
}

impl Subscriptions {
    /// An empty table. `const` so Stage 2 can place it in a `static`.
    pub(crate) const fn new() -> Self {
        Subscriptions { subs: [None; MAX_SUBS] }
    }

    /// Arm a subscription routing `source`'s events into ring `ring`'s CQ under
    /// `user_data`; returns its slot. Rejects (`None`) a full pool or a
    /// duplicate `(ring, user_data)` -- that pair is the cancel/route key, so
    /// one cookie names at most one stream on a ring.
    pub(crate) fn subscribe(&mut self, source: usize, ring: usize, user_data: u64) -> Option<usize> {
        if self.find(ring, user_data).is_some() {
            return None;
        }
        let slot = self.subs.iter().position(|s| s.is_none())?;
        self.subs[slot] = Some(Subscription { source, ring, user_data, dropped: 0 });
        Some(slot)
    }

    /// Cancel the subscription named by `(ring, user_data)`; returns whether one
    /// was live. Owner-scoping is the caller's responsibility (it resolves
    /// `ring` from its own Ring cap, s9); this clears strictly by exact key.
    pub(crate) fn cancel(&mut self, ring: usize, user_data: u64) -> bool {
        match self.find(ring, user_data) {
            Some(i) => {
                self.subs[i] = None;
                true
            }
            None => false,
        }
    }

    /// Drop every subscription on `ring` -- ring teardown (`release`): a freed
    /// ring must leave no subscription that would post into its reused CQ.
    pub(crate) fn release_ring(&mut self, ring: usize) {
        for s in self.subs.iter_mut() {
            if matches!(s, Some(sub) if sub.ring == ring) {
                *s = None;
            }
        }
    }

    /// Route one packed `event` (low 32 bits) to every subscription on `source`.
    /// For each, `post(ring, user_data, status)` attempts the CQ post and
    /// returns whether the CQ admitted it; `status` is the packed event with the
    /// drop flag set iff events were dropped on this subscription since the
    /// reader last saw one. A full CQ (`post` returns false) drops the *newest*
    /// event -- the queued prefix the reader holds stays coherent -- and bumps
    /// the sticky dropped count; a successful post clears it (the flag has now
    /// been surfaced). A source with no subscription drops the event (S6: no
    /// pre-subscription buffering). The per-event work is bounded (one
    /// post-or-drop per subscription), so a slow reader cannot stall the IRQ (s9).
    pub(crate) fn deliver<F>(&mut self, source: usize, event: u32, mut post: F)
    where
        F: FnMut(usize, u64, u32) -> bool,
    {
        for entry in self.subs.iter_mut() {
            let Some(sub) = entry else { continue };
            if sub.source != source {
                continue;
            }
            let status = if sub.dropped > 0 { event | EVENT_DROPPED } else { event };
            if post(sub.ring, sub.user_data, status) {
                sub.dropped = 0;
            } else {
                sub.dropped = sub.dropped.saturating_add(1);
            }
        }
    }

    /// True if any subscription is live -- the scheduler's "input pending"
    /// signal (`any_event_sub`).
    pub(crate) fn any(&self) -> bool {
        self.subs.iter().any(|s| s.is_some())
    }

    /// The sticky dropped-event count for `(ring, user_data)`, if live -- the
    /// backpressure signal behind `EVENT_DROPPED`. Drives the routing tests now;
    /// a `dropped`-query path may surface it to the libOS later.
    #[allow(dead_code)]
    pub(crate) fn dropped(&self, ring: usize, user_data: u64) -> Option<u32> {
        self.find(ring, user_data).map(|i| self.subs[i].unwrap().dropped)
    }

    /// Index of the live subscription named by `(ring, user_data)`.
    fn find(&self, ring: usize, user_data: u64) -> Option<usize> {
        self.subs
            .iter()
            .position(|s| matches!(s, Some(sub) if sub.ring == ring && sub.user_data == user_data))
    }
}
