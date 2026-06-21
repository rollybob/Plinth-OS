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

/// SQ entry `op` field. v1 reads only (R1).
const RING_OP_READ: u32 = 0;

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

/// Validate one SQ entry (read-once kernel copy) against CURRENT and post it.
/// The cap-checks are the exokernel multiplexing surface, lifted verbatim from
/// the retired `block_read`: the request must lie inside the holder's BlockRange
/// (RIGHT_READ), and the I/O frame must be the holder's with RIGHT_WRITE (the
/// device DMAs into it). A failed check posts an immediate CQ error completion
/// and consumes the entry.
fn post_entry(
    id: usize,
    op: u32,
    count: u64,
    range_slot: u32,
    frame_slot: u32,
    sector_off: u64,
    user_data: u64,
) -> Outcome {
    if op != RING_OP_READ || count == 0 || count.saturating_mul(SECTOR_SIZE) > FRAME_SIZE {
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
/// A released ring (owner exited) drops the completion. The CQ is sized by the
/// libOS to hold the device's in-flight depth, so a reaping libOS never
/// overflows it; Stage 2's shim is single-in-flight.
pub fn post_completion(id: usize, user_data: u64, status: u64) -> Option<usize> {
    // SAFETY: single CPU, IF=0 (IRQ/syscall under BKL); RINGS is only touched
    // from these paths.
    let rings = unsafe { &mut *addr_of_mut!(RINGS) };
    if id >= MAX_RINGS || !rings[id].in_use {
        return None;
    }
    let cq_base = va(rings[id].cq_phys);
    let mask = rings[id].mask;
    let tail = rings[id].cq_tail;
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

    if rings[id].waiting {
        rings[id].waiting = false;
        Some(rings[id].owner)
    } else {
        None
    }
}

/// Post a completion during the submit drain (the submitter is running, never
/// parked), so the wake target is always `None` -- ignore it.
fn post_status(id: usize, user_data: u64, status: u64) {
    let _ = post_completion(id, user_data, status);
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
/// `post_completion` drops. Idempotent.
pub fn release(id: usize) {
    // SAFETY: single CPU, IF=0 (teardown runs under the BKL).
    let rings = unsafe { &mut *addr_of_mut!(RINGS) };
    if id < MAX_RINGS {
        rings[id] = Ring::empty();
    }
}
