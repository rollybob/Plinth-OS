//! A reference async executor over the kernel's completion rings -- block I/O
//! and input events on one ring.
//!
//! This is the library-OS half of the ring ABI:
//! the kernel ships the ring *mechanism* (register/submit/wait + the in-flight
//! demux); this is one *policy* built on it -- a minimal `no_std` futures executor
//! a real library OS may replace. It is what makes depth observable: a caller
//! issues several reads that overlap on the device, then awaits them all, the
//! kernel demuxing each completion back by its `user_data` cookie.
//!
//! Two future shapes ride the same reactor (s2/s6):
//!
//!   - A block `read` is a *one-shot* future: a unique cookie that retires when
//!     its single completion is reaped.
//!   - An event `subscribe` is a *multishot* stream: one `RING_OP_EVENT_SUB`
//!     arms a standing subscription, and a persistent cookie yields a *sequence*
//!     of event completions (each `next()` reaps one) until `cancel`. Input is
//!     producer-initiated -- a keystroke answers no request -- so it is a stream,
//!     not a request/response. The reactor (drain CQ, `ring_wait` when empty) is
//!     reused unchanged; only the future on top differs.
//!
//! Because both are demuxed by `user_data` in the same CQ, one `ring_wait` loop
//! multiplexes reads and events -- the unified event loop a real OS is built on.
//!
//! Design choices, deliberately minimal (complexity must earn its place):
//!
//!   - A submitted read is a `Future` whose `poll` returns `Ready(status)` once
//!     its completion has been reaped from the CQ, `Pending` until then. The
//!     correlation a completion needs is its `user_data`, so the reactor keeps a
//!     small `user_data -> status` table of reaped-but-unconsumed completions --
//!     the io_uring-style cookie match, not a registry of `Waker`s.
//!   - The waker is a no-op: this is a single-threaded cooperative executor, so
//!     `block_on` simply re-polls its whole future tree after each batch of
//!     completions. A waker registry would buy nothing here.
//!   - Every future is `Unpin` (plain data, no self-reference), so the executor
//!     polls through `Pin::new(&mut _)` and needs no unsafe pinning. Combinators
//!     are concrete (`join`) rather than `async`/`await` blocks, which keeps the
//!     whole thing allocation-free and explicit.
//!
//! The ring is a per-process singleton (a user process is single-threaded, so
//! the static is race-free); `init` sets it up once. Its SQ/CQ frames sit below
//! libplinth's single-in-flight shim frames so the two never collide.

use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{fence, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use libplinth::{
    sys_bind_device, sys_bind_wait, sys_frame_alloc, sys_frame_map, sys_ring_dropped,
    sys_ring_register, sys_ring_submit, sys_ring_wait, sys_write, write_dec, MAP_END, PAGE_SIZE,
    SYS_ERR,
};

/// Ring depth: a power of two that fits one frame and exceeds any realistic
/// in-flight count for the demo. The device's own in-flight pool bounds true
/// concurrency well below this.
const ENTRIES: u64 = 16;

/// Capacity of the reaped-completion table: at most `ENTRIES` can be in flight,
/// so this never overflows while the consumer keeps up (it polls after each
/// reap). When it does -- a burst larger than `CAP` reaped before the consumer
/// polls -- the excess completions are counted as `Reactor::undelivered` and
/// voiced on serial, rather than silently dropped.
const CAP: usize = ENTRIES as usize;

/// SQ/CQ frames, just below libplinth's shim frames (MAP_END-1/-2 pages) so a
/// process that somehow used both never collides. The demos use one or the
/// other; data frames grow up from MAP_BASE, far below these.
const SQ_VA: u64 = MAP_END - 3 * PAGE_SIZE;
const CQ_VA: u64 = MAP_END - 4 * PAGE_SIZE;

// Ring header / entry layout (s4), byte offsets.
const HDR_HEAD: u64 = 0;
const HDR_TAIL: u64 = 4;
const HDR_MASK: u64 = 8;
const HDR_SIZE: u64 = 16;
const SQ_ENTRY: u64 = 32;
const CQ_ENTRY: u64 = 16;

// SQ entry `op` selectors (s4, S1).
const RING_OP_READ: u32 = 0;
const RING_OP_EVENT_SUB: u32 = 1;
const RING_OP_CANCEL: u32 = 2;
const RING_OP_WRITE: u32 = 3;

/// Drop-flag bit in an event completion's `status` (s5): the
/// kernel sets it on the first event posted after one or more were dropped on a
/// full CQ. Two things happen with it here, at different points. `reap` COUNTS it
/// -- every completion draining out of the CQ is inspected, so a drop is tallied
/// into `Reactor::drops` even when the flagged completion is a mouse event no
/// future ever consumes (the view-loop case: the shell awaits only the keyboard
/// while mouse packets accumulate). The event futures then MASK it off the word
/// they return, since it overlays the packed event's `value` field and is not
/// event data. Counting at reap rather than at consume is why a slow reader that
/// never drains a given stream still sees `events_dropped()` climb.
///
/// Safe to test at reap without knowing whether a completion is a block read or
/// an event: only the kernel event path ever sets bit 31, and block status codes
/// are small (`BLK_OK`=0 .. `BLK_E_DEV`=4), so a set bit 31 is unambiguously a
/// dropped-event marker.
const EVENT_DROPPED: u32 = 1 << 31;

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

// --- direct-bound path (D9 slice 1+2) ---
//
// A second reactor mode drives a *directly-bound* device: bind_device maps the
// device's virtqueue + notify page into this process, and the reactor writes its
// own descriptors and rings the doorbell, with the kernel off the submit path
// (direct_binding.md D1/D9). Everything above the reactor -- the `block_on` loop
// and the future it drives -- is written against the reactor, not the mode.

/// Which submission/completion path this reactor drives. Chosen once, at `init`
/// (kernel-bridged) or `init_bound` (direct-bound), and fixed for the reactor's
/// life -- honoring D9's ruling that a device is one or the other, never both.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// SQ/CQ frames registered with the kernel; submit via the `ring_submit`
    /// doorbell, completions posted by the kernel into the CQ (the original path).
    KernelBridged,
    /// The reactor owns the bound device's virtqueue and writes its own
    /// descriptors; the kernel is off the submit path. Slice 2 waits for a
    /// completion by busy-polling the used ring (slice 3 swaps in a blocking wait).
    Bound,
}

// virtio split-ring descriptor flags and the virtio-blk read request type, for the
// bound path where the libOS writes its own descriptors (mirrors bind-user).
const VD_F_NEXT: u16 = 1;
const VD_F_WRITE: u16 = 2;
const VBLK_T_IN: u32 = 0;
/// One 512-byte sector per bound read.
const B_SECTOR_LEN: u32 = 512;
/// Max concurrent bound reads (slice 4). The reactor owns one 4-KiB data page and
/// one 4-KiB header/status page and subdivides each per slot, so the ceiling is the
/// data page over the 512-byte sector: 4096 / 512 = 8. (The desc page holds 256
/// descriptors = 85 three-descriptor chains, and a real queue is far deeper, so the
/// data page is the binding constraint.) Multi-in-flight now lives in userspace
/// because the kernel is off the submit path (direct_binding.md D9 / section 9.4).
const B_MAX_INFLIGHT: usize = 8;
/// Descriptors per read: a 3-chain (header -> data -> status). Slot i owns the chain
/// whose head descriptor index is `i * B_DESC_PER_READ`.
const B_DESC_PER_READ: u16 = 3;
/// Per-slot sub-layout of the one header/status buffer page: slot i's 16-byte
/// request header at `i*16`, its status byte at `B_STATUS_BASE + i`. Non-overlapping
/// for i < B_MAX_INFLIGHT (headers occupy 0..128, statuses 2048..2056).
const B_HDR_STRIDE: u64 = 16;
const B_STATUS_BASE: u64 = 2048;
/// Status the bound path stages if `sys_bind_wait` errors (bad cap / non-bound
/// device) or a read cannot get a slot; distinct from any real virtio-blk status
/// (BLK_OK..BLK_E_DEV are small).
const B_STATUS_TIMEOUT: u64 = 0xFE;
/// Sentinel for a `read_bound` that could not get an in-flight slot (all busy).
const B_SLOT_NONE: usize = usize::MAX;

#[inline]
unsafe fn r16(a: u64) -> u16 {
    core::ptr::read_volatile(a as *const u16)
}
#[inline]
unsafe fn w16(a: u64, v: u16) {
    core::ptr::write_volatile(a as *mut u16, v)
}
#[inline]
unsafe fn r8(a: u64) -> u8 {
    core::ptr::read_volatile(a as *const u8)
}
#[inline]
unsafe fn w8(a: u64, v: u8) {
    core::ptr::write_volatile(a as *mut u8, v)
}
/// Write one 16-byte split-ring descriptor at `at`: addr, len, flags, next.
/// SAFETY: `at` is inside the mapped, writable desc-ring page.
#[inline]
unsafe fn write_desc(at: u64, addr: u64, len: u32, flags: u16, next: u16) {
    core::ptr::write_volatile(at as *mut u64, addr);
    core::ptr::write_volatile((at + 8) as *mut u32, len);
    core::ptr::write_volatile((at + 12) as *mut u16, flags);
    core::ptr::write_volatile((at + 14) as *mut u16, next);
}

/// Geometry of a directly-bound device, filled by `init_bound` from `bind_device`.
/// The six pages are the contiguous window bind_device maps; the two IOVAs are the
/// names the device's IOMMU domain resolves (the libOS writes IOVAs, never physical
/// addresses -- D1/I5). Valid only while the reactor's mode is `Bound`.
#[derive(Clone, Copy)]
struct Bound {
    notify: u64,
    desc: u64,
    avail: u64,
    used: u64,
    buf: u64,
    data: u64,
    qsize: u16,
    data_iova: u64,
    buf_iova: u64,
    /// The BoundDevice capability slot, passed to `sys_bind_wait` to park on this
    /// device's completion IRQ (slice 3).
    bind_slot: u64,
    /// In-flight slots this device supports = min(qsize / 3, B_MAX_INFLIGHT).
    chains: usize,
    /// The used-ring index already reaped; a completion is "the used index moved
    /// past this." Advances by one per completed chain (slice 4: many per wake).
    used_seen: u16,
    /// Per-slot in-flight cookie: slot i is busy with read `slot_cookie[i]`, or 0
    /// when free (cookies start at 1). Slot i owns chain head `i*3`, data sub-buffer
    /// i, and the header/status sub-region i -- the userspace demux that replaces the
    /// kernel's `Inflights` on the bound path.
    slot_cookie: [u64; B_MAX_INFLIGHT],
}

impl Bound {
    const fn empty() -> Self {
        Bound {
            notify: 0,
            desc: 0,
            avail: 0,
            used: 0,
            buf: 0,
            data: 0,
            qsize: 0,
            data_iova: 0,
            buf_iova: 0,
            bind_slot: 0,
            chains: 0,
            used_seen: 0,
            slot_cookie: [0; B_MAX_INFLIGHT],
        }
    }
}

/// The per-process reactor: the registered ring plus the table of completions
/// reaped from the CQ but not yet consumed by a future's `poll`.
struct Reactor {
    ready: bool,
    /// Which submit/completion path this reactor drives (set at init/init_bound).
    mode: Mode,
    /// Direct-bound device geometry; meaningful only while `mode == Bound`.
    bound: Bound,
    handle: u64,
    /// Monotonic cookie source; each read gets a unique `user_data`.
    next_ud: u64,
    /// Reaped completions awaiting a matching `poll`: (user_data, status).
    done: [(u64, u64); CAP],
    done_len: usize,
    /// Count of event completions reaped carrying the kernel's `EVENT_DROPPED`
    /// flag -- one per burst of CQ-full drops the kernel surfaced (each flagged
    /// completion marks at least one lost event since the previous one on that
    /// stream, s5). A lower bound on total loss, not the exact
    /// count; the exact per-subscription tally lives in the kernel. Read via
    /// `events_dropped()` -- the reason input loss is a reported number rather
    /// than a silent stall.
    drops: u32,
    /// Completions reaped from the CQ but discarded because the `done` table was
    /// full -- a *second*, distinct loss from `drops`. `drops` is the kernel
    /// dropping input before the libOS ever saw it (the CQ overran); this is the
    /// libOS reaping a completion and then having nowhere to stage it (the reactor
    /// table overran). It is the loss the refuted K-027 `done`-table hypothesis
    /// was about, and it is silent no longer. Read via `undelivered()`.
    undelivered: u32,
    /// The value of `drops` last announced on serial by `report_losses`. Lets the
    /// executor emit one line per new drop burst rather than repeating the total
    /// on every wake.
    reported_drops: u32,
    /// The same, for `undelivered` -- one line per new reactor-table overrun.
    reported_undelivered: u32,
}

static mut REACTOR: Reactor = Reactor {
    ready: false,
    mode: Mode::KernelBridged,
    bound: Bound::empty(),
    handle: SYS_ERR,
    next_ud: 1,
    done: [(0, 0); CAP],
    done_len: 0,
    drops: 0,
    undelivered: 0,
    reported_drops: 0,
    reported_undelivered: 0,
};

/// Access the per-process reactor. SAFETY: a user process is single-threaded
/// and the executor never re-enters itself, so there is no aliasing.
fn reactor() -> &'static mut Reactor {
    unsafe { &mut *core::ptr::addr_of_mut!(REACTOR) }
}

impl Reactor {
    /// Take the oldest reaped completion for `ud`, if present. Removes it (each
    /// completion is consumed exactly once).
    ///
    /// `reap` appends in CQ (delivery) order, so the lowest-index match is the
    /// oldest, and removal shifts the tail down to preserve that order. A one-shot
    /// `Read` has a unique `ud` (at most one match), so order is irrelevant to it;
    /// but a multishot `EventStream` reuses one cookie across a *sequence* of
    /// events, and a keystroke stream must surface in arrival order -- so the
    /// shared reactor keeps the `done` table FIFO per cookie rather than
    /// swap-removing. The shift is O(done_len) over a CAP=16 table: negligible.
    fn take(&mut self, ud: u64) -> Option<u64> {
        let mut i = 0;
        while i < self.done_len {
            if self.done[i].0 == ud {
                let status = self.done[i].1;
                // Order-preserving remove: shift the rest down one.
                let mut j = i;
                while j + 1 < self.done_len {
                    self.done[j] = self.done[j + 1];
                    j += 1;
                }
                self.done_len -= 1;
                return Some(status);
            }
            i += 1;
        }
        None
    }

    /// Drain every completion the kernel has posted into the CQ since last time
    /// into the `done` table, advancing the CQ head (this process is the CQ
    /// consumer). SAFETY: CQ_VA is this process's mapped CQ frame.
    fn reap(&mut self) {
        unsafe {
            let mask = r32(CQ_VA + HDR_MASK);
            loop {
                let head = r32(CQ_VA + HDR_HEAD);
                let tail = r32(CQ_VA + HDR_TAIL);
                if head == tail {
                    break;
                }
                let e = CQ_VA + HDR_SIZE + (head & mask) as u64 * CQ_ENTRY;
                let ud = r64(e);
                let status = r32(e + 8) as u64;
                // Tally the kernel's drop flag as each completion passes through,
                // before it is (or is not) matched to a future. This is the one
                // point that sees every event, so loss is counted even on a stream
                // the caller is not currently draining. Only event completions set
                // bit 31, so this never miscounts a block read (see EVENT_DROPPED).
                if status as u32 & EVENT_DROPPED != 0 {
                    self.drops = self.drops.saturating_add(1);
                }
                if self.done_len < CAP {
                    self.done[self.done_len] = (ud, status);
                    self.done_len += 1;
                } else {
                    // The staging table is full, so this completion -- already
                    // reaped from the CQ, whose head advances below regardless --
                    // has nowhere to go and is lost here, in the libOS, not the
                    // kernel. Count it as a distinct loss from `drops`. This is the
                    // refuted K-027 trigger; counting it, not fixing it (delivery
                    // is unchanged), makes the second silent-loss site a number.
                    self.undelivered = self.undelivered.saturating_add(1);
                }
                w32(CQ_VA + HDR_HEAD, head.wrapping_add(1));
            }
        }
    }

    /// Announce on serial any loss counted since the last announcement, and mark
    /// it announced. Called from `block_on` after each reap, so loss surfaces the
    /// instant it happens -- crucially even while a consumer is parked awaiting one
    /// stream and the loss is landing on another. That is the view-loop hang the
    /// K-027 investigation hit: the shell blocks in `block_on(kbd.next())`, a mouse
    /// flood overruns the shared CQ and swallows the keystroke, and every wake
    /// reaps mouse events while `kbd.next()` stays Pending and never returns to the
    /// caller. A caller-side poll cannot see that; this can, because it runs inside
    /// the blocking loop.
    ///
    /// Two distinct losses, each with its own line and its own shadow counter so a
    /// new occurrence of either is announced exactly once:
    ///   - `drops`: the kernel dropped input on a full CQ before the libOS saw it
    ///     (`EVENT_DROPPED`).
    ///   - `undelivered`: the libOS reaped a completion but its staging table was
    ///     full, so it discarded it. The refuted-as-trigger K-027 site.
    ///
    /// Silent when nothing was lost, so a reader that keeps up -- every
    /// deterministic demo and the scripted tour -- prints nothing and the smoke
    /// transcript is unchanged. This is reference-executor policy, not mechanism: a
    /// real library OS may route these to a log, a counter, or a UI instead of
    /// serial. What the kernel guarantees is the counts; where they are voiced is
    /// the tenant's choice.
    fn report_losses(&mut self) {
        if self.drops != self.reported_drops {
            self.reported_drops = self.drops;
            sys_write(b"libos: input dropped on a full queue, bursts ");
            write_dec(self.drops as u64);
            sys_write(b"\n");
        }
        if self.undelivered != self.reported_undelivered {
            self.reported_undelivered = self.undelivered;
            sys_write(b"libos: completion discarded, reactor table full, total ");
            write_dec(self.undelivered as u64);
            sys_write(b"\n");
        }
    }

    /// Claim a free in-flight slot for cookie `ud`, or `None` if all `chains` slots
    /// are busy (backpressure -- the caller retries after a reap frees one). This is
    /// the userspace analogue of the kernel's `Inflights::submit` on the bound path.
    fn bound_alloc(&mut self, ud: u64) -> Option<usize> {
        for i in 0..self.bound.chains {
            if self.bound.slot_cookie[i] == 0 {
                self.bound.slot_cookie[i] = ud;
                return Some(i);
            }
        }
        None
    }

    /// Enqueue slot `i`'s read of `sector`: write its 3-descriptor chain into the
    /// desc ring (naming this slot's header/data/status IOVAs) and publish the chain
    /// head in the avail ring. Does NOT ring the doorbell -- `block_on` rings once
    /// per batch, so overlapping reads post together and run on the device at once.
    /// SAFETY: the geometry VAs are this process's `bind_device` mapping and
    /// `i < chains`, so every sub-region is within the mapped pages.
    unsafe fn bound_enqueue(&mut self, i: usize, sector: u64) {
        let b = self.bound;
        let head = (i as u16) * B_DESC_PER_READ;
        let hdr_va = b.buf + i as u64 * B_HDR_STRIDE;
        let hdr_iova = b.buf_iova + i as u64 * B_HDR_STRIDE;
        let status_iova = b.buf_iova + B_STATUS_BASE + i as u64;
        let data_iova = b.data_iova + i as u64 * B_SECTOR_LEN as u64;

        // Request header + a status sentinel the device overwrites, in slot i's
        // header/status sub-region.
        w32(hdr_va, VBLK_T_IN);
        w32(hdr_va + 4, 0);
        w64(hdr_va + 8, sector);
        w8(b.buf + B_STATUS_BASE + i as u64, 0xFF);

        // The chain at head i*3: hdr (device-read) -> data (device-write) -> status
        // (device-write). Descriptor k lives at desc + k*16; addresses are IOVAs,
        // never physical (D1/I5).
        let d = b.desc + head as u64 * 16;
        write_desc(d, hdr_iova, 16, VD_F_NEXT, head + 1);
        write_desc(d + 16, data_iova, B_SECTOR_LEN, VD_F_NEXT | VD_F_WRITE, head + 2);
        write_desc(d + 32, status_iova, 1, VD_F_WRITE, 0);

        // Publish the head in the avail ring (flags = 0: interrupts NOT suppressed,
        // so the device raises the completion IRQ the executor parks on).
        w16(b.avail, 0);
        let idx = r16(b.avail + 2);
        w16(b.avail + 4 + (idx % b.qsize) as u64 * 2, head);
        fence(Ordering::SeqCst);
        w16(b.avail + 2, idx.wrapping_add(1));
        fence(Ordering::SeqCst);
    }

    /// Ring the bound device's doorbell (queue 0) for whatever was enqueued since
    /// the last ring. SAFETY: notify is this process's mapped doorbell page.
    unsafe fn bound_doorbell(&self) {
        w16(self.bound.notify, 0);
    }

    /// Park until the device advances its used ring past what we have reaped
    /// (`used_seen`), then return (slice 3). The kernel re-checks the
    /// device-advanced used index under IF=0 before parking, so a completion landing
    /// in the gap is not lost; a spurious wake just re-parks. On a `sys_bind_wait`
    /// error, fail every outstanding slot so `block_on` cannot hang. SAFETY: used VA
    /// is this process's bind mapping.
    unsafe fn bound_park(&mut self) {
        while r16(self.bound.used + 2) == self.bound.used_seen {
            if sys_bind_wait(self.bound.bind_slot, self.bound.used_seen as u64) == SYS_ERR {
                self.bound_fail_all();
                return;
            }
        }
    }

    /// Reap every completion the device has posted since `used_seen` (slice 4: many
    /// per wake). For each used element, map its echoed chain head back to a slot,
    /// read that slot's status byte, stage it under the slot's cookie (the shared
    /// `take`/done-table path), and free the slot. SAFETY: used/buf VAs are this
    /// process's bind mapping.
    unsafe fn bound_reap(&mut self) {
        let (used, buf, qsize) = (self.bound.used, self.bound.buf, self.bound.qsize);
        loop {
            let used_idx = r16(used + 2);
            if self.bound.used_seen == used_idx {
                break;
            }
            // Order the status/id reads after observing the used-index advance.
            fence(Ordering::SeqCst);
            let ring_slot = (self.bound.used_seen % qsize) as u64;
            // Used element { id: u32, len: u32 }; id is the chain head we submitted.
            let head = r32(used + 4 + ring_slot * 8);
            let i = (head / B_DESC_PER_READ as u32) as usize;
            if i < self.bound.chains {
                let status = r8(buf + B_STATUS_BASE + i as u64) as u64;
                let ud = self.bound.slot_cookie[i];
                self.bound.slot_cookie[i] = 0;
                self.stage_done(ud, status);
            }
            self.bound.used_seen = self.bound.used_seen.wrapping_add(1);
        }
    }

    /// Stage one bound completion into the done table under its cookie. Drops it
    /// (counts an `undelivered`, matching the kernel-bridged overrun accounting) if
    /// the table is full; a zero cookie (a freed/unknown slot) is ignored.
    fn stage_done(&mut self, ud: u64, status: u64) {
        if ud == 0 {
            return;
        }
        if self.done_len < CAP {
            self.done[self.done_len] = (ud, status);
            self.done_len += 1;
        } else {
            self.undelivered = self.undelivered.saturating_add(1);
        }
    }

    /// Fail every outstanding bound slot with a timeout status (used only when
    /// `sys_bind_wait` errors), so the parked reads resolve instead of hanging.
    fn bound_fail_all(&mut self) {
        for i in 0..self.bound.chains {
            let ud = self.bound.slot_cookie[i];
            if ud != 0 {
                self.bound.slot_cookie[i] = 0;
                self.stage_done(ud, B_STATUS_TIMEOUT);
            }
        }
    }
}

/// Set up the executor's ring once: allocate + map an SQ and a CQ frame and
/// register them. Returns false if any step fails. Call before any `read`.
pub fn init() -> bool {
    let r = reactor();
    if r.ready {
        return true;
    }
    let sq = sys_frame_alloc();
    if sq == SYS_ERR || sys_frame_map(sq, SQ_VA) == SYS_ERR {
        return false;
    }
    let cq = sys_frame_alloc();
    if cq == SYS_ERR || sys_frame_map(cq, CQ_VA) == SYS_ERR {
        return false;
    }
    let handle = sys_ring_register(sq, cq, ENTRIES);
    if handle == SYS_ERR {
        return false;
    }
    r.handle = handle;
    r.ready = true;
    true
}

/// Set up the executor over a *directly-bound* device instead of a kernel-bridged
/// ring (D9). Binds the device named by the `BoundDevice` capability at
/// `bind_slot`, mapping its virtqueue + notify page as six contiguous pages at
/// `base_va` (notify, desc, avail, used, buf, data), and records the geometry the
/// reactor needs to write its own descriptors. `info_out` receives
/// `[qsize, data_iova, buf_iova]` -- the same values `bind_device` returns -- for a
/// caller that also drives manual probes over the same mapping. Returns false if
/// the bind fails or reports a zero-size queue. Mutually exclusive with `init`: a
/// reactor is one mode for its life, so calling this after `init` (or twice)
/// returns whether the reactor is already bound rather than rebinding.
pub fn init_bound(bind_slot: u64, base_va: u64, info_out: &mut [u64; 3]) -> bool {
    let r = reactor();
    if r.ready {
        return r.mode == Mode::Bound;
    }
    let mut info = [0u64; 3];
    if sys_bind_device(bind_slot, base_va, info.as_mut_ptr() as u64) != 0 {
        return false;
    }
    let qsize = info[0];
    if qsize == 0 || qsize > u16::MAX as u64 {
        return false;
    }
    // In-flight ceiling: min(qsize / 3, MAX). Never zero for a real queue (qsize is
    // at least 3), but guard so a pathological queue disables the bound path rather
    // than dividing by zero downstream.
    let chains = ((qsize / B_DESC_PER_READ as u64) as usize).min(B_MAX_INFLIGHT);
    if chains == 0 {
        return false;
    }
    let used = base_va + 3 * PAGE_SIZE;
    r.mode = Mode::Bound;
    r.bound = Bound {
        notify: base_va,
        desc: base_va + PAGE_SIZE,
        avail: base_va + 2 * PAGE_SIZE,
        used,
        buf: base_va + 4 * PAGE_SIZE,
        data: base_va + 5 * PAGE_SIZE,
        qsize: qsize as u16,
        data_iova: info[1],
        buf_iova: info[2],
        bind_slot,
        chains,
        // Baseline the reap cursor at the current used index: the kernel's boot-time
        // bind selftest already drove one read through this device, so the index is
        // not zero. All later reaps are relative to this.
        // SAFETY: `used` is this process's mapped used-ring page (just bound).
        used_seen: unsafe { r16(used + 2) },
        slot_cookie: [0; B_MAX_INFLIGHT],
    };
    r.ready = true;
    *info_out = info;
    true
}

/// How many event completions this reactor has reaped carrying the kernel's
/// `EVENT_DROPPED` flag since the process started -- a monotonically rising
/// count of the bursts in which the CQ overran and the kernel dropped input
/// (s5). Zero on a reader that keeps up. `block_on` already voices
/// each new burst on serial; this is the same number as a value, for a tenant
/// that wants to poll it (a status line, a metric) rather than read the log. It
/// is a lower bound on events lost (one tick per drop burst, not per event); the
/// exact per-subscription tally would need a kernel query.
pub fn events_dropped() -> u32 {
    reactor().drops
}

/// How many completions this reactor has reaped from the CQ but discarded because
/// its staging table (`done`, CAP entries) was full -- loss inside the libOS, as
/// distinct from `events_dropped()` (loss inside the kernel). Nonzero only when a
/// burst larger than the table arrives before the consumer polls, which is the
/// refuted-as-trigger K-027 site; zero for a reader that keeps up. `block_on`
/// already voices each new overrun on serial; this is the same number as a value.
pub fn undelivered() -> u32 {
    reactor().undelivered
}

/// Push one block I/O submission entry into the SQ at its tail (the kernel
/// reads it on the next doorbell). `op` selects `RING_OP_READ` or
/// `RING_OP_WRITE` (S1) -- same entry shape either way.
/// SAFETY: SQ_VA is this process's mapped SQ frame.
unsafe fn push_sq(op: u32, ud: u64, range_slot: u64, frame_slot: u64, sector_off: u64, count: u64) {
    let mask = r32(SQ_VA + HDR_MASK);
    let tail = r32(SQ_VA + HDR_TAIL);
    let e = SQ_VA + HDR_SIZE + (tail & mask) as u64 * SQ_ENTRY;
    w32(e, op);
    w32(e + 4, (count & 0xFFFF) as u32); // flags: count in the low 16 bits
    w32(e + 8, range_slot as u32);
    w32(e + 12, frame_slot as u32);
    w64(e + 16, sector_off);
    w64(e + 24, ud);
    w32(SQ_VA + HDR_TAIL, tail.wrapping_add(1));
}

/// Push one event-control entry (EVENT_SUB or CANCEL) into the SQ at its tail.
/// For EVENT_SUB, `source_slot` names the EventSource cap (it reuses the read
/// path's `range_slot` field, s4) and `ud` is the subscription
/// cookie echoed in every event completion; for CANCEL, `ud` names the live
/// subscription and `source_slot` is ignored. SAFETY: SQ_VA is this process's
/// mapped SQ frame.
unsafe fn push_ctrl(op: u32, ud: u64, source_slot: u64) {
    let mask = r32(SQ_VA + HDR_MASK);
    let tail = r32(SQ_VA + HDR_TAIL);
    let e = SQ_VA + HDR_SIZE + (tail & mask) as u64 * SQ_ENTRY;
    w32(e, op);
    w32(e + 4, 0); // flags: unused for control ops
    w32(e + 8, source_slot as u32); // range_slot field = EventSource cap (EVENT_SUB)
    w32(e + 12, 0); // frame_slot: unused
    w64(e + 16, 0); // sector_off: unused
    w64(e + 24, ud);
    w32(SQ_VA + HDR_TAIL, tail.wrapping_add(1));
}

/// A pending block read. On its first `poll` it enqueues its submission entry;
/// thereafter it reports `Ready(status)` once its completion has been reaped.
pub struct Read {
    ud: u64,
    posted: bool,
    range_slot: u64,
    frame_slot: u64,
    sector_off: u64,
    count: u64,
}

/// Create a read future: `count` 512-byte sectors at `sector_off` into the
/// BlockRange at `range_slot`, DMA'd into the frame at `frame_slot`. Nothing is
/// submitted until the future is first polled (so a batch of reads posts in one
/// doorbell). Each future gets a unique `user_data` cookie.
pub fn read(range_slot: u64, frame_slot: u64, sector_off: u64, count: u64) -> Read {
    let r = reactor();
    let ud = r.next_ud;
    r.next_ud = r.next_ud.wrapping_add(1);
    Read { ud, posted: false, range_slot, frame_slot, sector_off, count }
}

impl Future for Read {
    /// The block status word (BLK_OK or a BLK_E_*).
    type Output = u64;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u64> {
        let me = self.get_mut(); // Read is Unpin
        if !me.posted {
            // SAFETY: the ring is set up (init) before any read is polled.
            unsafe {
                push_sq(RING_OP_READ, me.ud, me.range_slot, me.frame_slot, me.sector_off, me.count)
            };
            me.posted = true;
        }
        match reactor().take(me.ud) {
            Some(status) => Poll::Ready(status),
            None => Poll::Pending,
        }
    }
}

/// A pending block write -- the write half of `Read` (S1). Same shape and
/// lifecycle: nothing is submitted until first polled,
/// `Ready(status)` once the completion is reaped.
pub struct Write {
    ud: u64,
    posted: bool,
    range_slot: u64,
    frame_slot: u64,
    sector_off: u64,
    count: u64,
}

/// Create a write future: `count` 512-byte sectors at `sector_off` of the
/// BlockRange at `range_slot`, DMA'd out of the frame at `frame_slot`. The
/// `BlockRange` cap must carry `RIGHT_WRITE` and the frame cap `RIGHT_READ`
/// (the kernel reads the frame's existing contents to hand to the device) --
/// the flipped direction from `read` (S3).
pub fn write(range_slot: u64, frame_slot: u64, sector_off: u64, count: u64) -> Write {
    let r = reactor();
    let ud = r.next_ud;
    r.next_ud = r.next_ud.wrapping_add(1);
    Write { ud, posted: false, range_slot, frame_slot, sector_off, count }
}

impl Future for Write {
    /// The block status word (BLK_OK or a BLK_E_*).
    type Output = u64;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u64> {
        let me = self.get_mut(); // Write is Unpin
        if !me.posted {
            // SAFETY: the ring is set up (init) before any write is polled.
            unsafe {
                push_sq(RING_OP_WRITE, me.ud, me.range_slot, me.frame_slot, me.sector_off, me.count)
            };
            me.posted = true;
        }
        match reactor().take(me.ud) {
            Some(status) => Poll::Ready(status),
            None => Poll::Pending,
        }
    }
}

/// A pending read over a *directly-bound* device (D9). Same lifecycle as `Read`,
/// but its submit is the reactor writing a descriptor chain itself -- no kernel
/// entry -- and its completion is staged by the reactor's bound reap. The sector's
/// bytes land in the reactor's own per-slot data sub-buffer (`data_va()`), not a
/// caller-named frame. Many `BoundRead`s can be in flight at once (slice 4), each on
/// its own slot. Requires the reactor be in bound mode (`init_bound`).
pub struct BoundRead {
    ud: u64,
    sector: u64,
    /// The in-flight slot claimed at creation, or `B_SLOT_NONE` if all were busy
    /// (that read resolves immediately with an error rather than blocking a join).
    slot: usize,
    posted: bool,
}

/// Create a bound read of one 512-byte `sector`. A free in-flight slot is claimed
/// now (so `data_va()` is known before the join), but nothing is submitted until the
/// future is first polled -- so a batch of reads posts together and overlaps on the
/// device. Each future draws a unique `user_data` cookie and rides the same
/// `take`/done-table path as a kernel-bridged read.
pub fn read_bound(sector: u64) -> BoundRead {
    let r = reactor();
    let ud = r.next_ud;
    r.next_ud = r.next_ud.wrapping_add(1);
    let slot = r.bound_alloc(ud).unwrap_or(B_SLOT_NONE);
    BoundRead { ud, sector, slot, posted: false }
}

impl BoundRead {
    /// The VA where this read's sector lands -- the reactor owns the buffer, so it
    /// names the address; the caller verifies the bytes here once the read resolves.
    /// Zero if the read got no slot (all in-flight slots were busy).
    pub fn data_va(&self) -> u64 {
        if self.slot == B_SLOT_NONE {
            0
        } else {
            reactor().bound.data + self.slot as u64 * B_SECTOR_LEN as u64
        }
    }
}

impl Future for BoundRead {
    /// The virtio-blk status byte, zero-extended (BLK_OK or a device error).
    type Output = u64;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u64> {
        let me = self.get_mut(); // BoundRead is Unpin
        if me.slot == B_SLOT_NONE {
            // No slot was available at creation: resolve as an error rather than
            // blocking the executor forever. (The demo stays within B_MAX_INFLIGHT,
            // so this is the honest failure mode, not a normal path.)
            return Poll::Ready(B_STATUS_TIMEOUT);
        }
        if !me.posted {
            // SAFETY: init_bound mapped the bound geometry before any read is polled,
            // and `slot < chains`.
            unsafe { reactor().bound_enqueue(me.slot, me.sector) };
            me.posted = true;
        }
        match reactor().take(me.ud) {
            Some(status) => Poll::Ready(status),
            None => Poll::Pending,
        }
    }
}

/// A multishot event subscription over the ring (s6). Unlike a
/// one-shot `Read`, its `user_data` cookie persists across completions: one
/// `RING_OP_EVENT_SUB` arms a standing subscription on an `EventSource`, and
/// every event on that source posts a completion the kernel tags with this
/// cookie, until `cancel`. `next()` reaps one event; the stream yields them in
/// arrival order (the reactor keeps the `done` table FIFO per cookie).
pub struct EventStream {
    ud: u64,
    source_slot: u64,
    /// The EVENT_SUB entry is posted lazily on the first `next().poll()`, so a
    /// just-created stream holds the source cap without yet touching the ring.
    armed: bool,
}

/// Open an event-stream subscription on the `EventSource` capability at
/// `source_slot`. Nothing is submitted until the first `next()` is polled (so the
/// subscribe rides the same doorbell as anything else enqueued). The stream draws
/// a unique cookie, so it coexists with reads and other streams on one ring.
pub fn subscribe(source_slot: u64) -> EventStream {
    let r = reactor();
    let ud = r.next_ud;
    r.next_ud = r.next_ud.wrapping_add(1);
    EventStream { ud, source_slot, armed: false }
}

impl EventStream {
    /// Arm the subscription on first use: post the one `RING_OP_EVENT_SUB` that
    /// turns this stream live. Idempotent -- every `next`/`collect` poll calls it,
    /// but only the first touches the ring. SAFETY: the ring is set up (`init`)
    /// before any stream is polled.
    fn arm(&mut self) {
        if !self.armed {
            unsafe { push_ctrl(RING_OP_EVENT_SUB, self.ud, self.source_slot) };
            self.armed = true;
        }
    }

    /// A future for the next event on this stream. Borrows the stream so the
    /// subscription's lazy-arm bookkeeping is shared across calls; `block_on` it
    /// to read one event (the demos' "subscribe, then reap N" loop).
    pub fn next(&mut self) -> NextEvent<'_> {
        NextEvent { stream: self }
    }

    /// A future that reaps the next `N` events into an array, in arrival order.
    /// Like `next` but batched: `block_on(stream.collect::<N>())` resolves once
    /// `N` events have arrived. Used by the unified-loop demo, where it is joined
    /// (`join2`) with a block read so one `ring_wait` drives both -- the payoff of
    /// carrying input and block I/O on a single ring.
    pub fn collect<const N: usize>(&mut self) -> Collect<'_, N> {
        Collect { stream: self, events: [0; N], count: 0 }
    }

    /// Cancel the subscription: post a `RING_OP_CANCEL` naming this cookie, so the
    /// kernel stops routing events here. After this, `next()` will re-arm a fresh
    /// subscription on the next poll. Idempotent on an unarmed stream (a CANCEL
    /// for an unknown cookie is a no-op drain in the kernel). The doorbell rings
    /// on the next `block_on`; teardown (process exit) also drops the
    /// subscription, so an explicit cancel is only needed to stop a *live* stream
    /// early.
    pub fn cancel(&mut self) {
        if self.armed {
            // SAFETY: the ring is set up (init) before any stream is used.
            unsafe { push_ctrl(RING_OP_CANCEL, self.ud, 0) };
            let handle = reactor().handle;
            let _ = sys_ring_submit(handle);
            self.armed = false;
        }
    }

    /// The kernel's exact count of events dropped on THIS subscription's full CQ
    /// since it was armed (ABI v2.11, `sys_ring_dropped`) -- read straight from
    /// the kernel rather than inferred from the drop flag. Unlike the reactor-wide
    /// `events_dropped()`, which counts flag bursts seen across every stream, this
    /// is the precise number for this stream's cookie, and it is readable even
    /// while the CQ is jammed and no completion is riding out to carry the flag.
    /// Zero on a stream that has never overrun; also zero for a stream not yet
    /// armed or already cancelled (the kernel has no subscription to count, which
    /// it reports as `SYS_ERR` and this folds to 0 -- "nothing dropped" either
    /// way).
    pub fn dropped(&self) -> u32 {
        let n = sys_ring_dropped(reactor().handle, self.ud);
        if n == SYS_ERR {
            0
        } else {
            n as u32
        }
    }
}

/// The future returned by `EventStream::next`: on its first poll it arms the
/// subscription (once per stream), then reports `Ready(event)` as soon as one
/// event for this cookie has been reaped, `Pending` until then. The event word
/// is the packed `Event` (kind/code/value, unpack with libplinth's
/// `event_code`/`event_kind`/`event_value`); the CQ drop flag is masked off.
pub struct NextEvent<'a> {
    stream: &'a mut EventStream,
}

impl Future for NextEvent<'_> {
    type Output = u64;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u64> {
        let me = self.get_mut(); // NextEvent is Unpin (it holds only a &mut)
        me.stream.arm();
        match reactor().take(me.stream.ud) {
            Some(status) => Poll::Ready(status & !(EVENT_DROPPED as u64)),
            None => Poll::Pending,
        }
    }
}

/// The future returned by `EventStream::collect`: arms the subscription on first
/// poll, then drains every available event for this cookie into `events` (in
/// arrival order -- the reactor's `done` table is FIFO per cookie), reporting
/// `Ready([N events])` once `N` have accumulated. The drop flag is masked off
/// each event word.
pub struct Collect<'a, const N: usize> {
    stream: &'a mut EventStream,
    events: [u64; N],
    count: usize,
}

impl<const N: usize> Future for Collect<'_, N> {
    type Output = [u64; N];

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<[u64; N]> {
        let me = self.get_mut(); // Collect is Unpin (plain data + a &mut)
        me.stream.arm();
        while me.count < N {
            match reactor().take(me.stream.ud) {
                Some(status) => {
                    me.events[me.count] = status & !(EVENT_DROPPED as u64);
                    me.count += 1;
                }
                None => break,
            }
        }
        if me.count == N {
            Poll::Ready(me.events)
        } else {
            Poll::Pending
        }
    }
}

/// Await several homogeneous reads together: polls each unfinished child on every
/// poll, so they all enqueue up front and overlap on the device. Resolves to each
/// read's status, indexed as the input array. Generic over the read future, so it
/// joins kernel-bridged `Read`s or directly-bound `BoundRead`s the same way -- the
/// D9 property that the executor's overlap machinery does not change with the mode
/// underneath (both have `Output = u64`).
pub struct JoinReads<F: Future<Output = u64> + Unpin, const N: usize> {
    reads: [F; N],
    status: [u64; N],
    done: [bool; N],
}

/// Join `N` reads into one future. `block_on(join([...]))` issues them all, then
/// resolves once every one has completed. `F` is inferred from the array -- `Read`
/// for a kernel-bridged ring, `BoundRead` for a directly-bound device.
pub fn join<F: Future<Output = u64> + Unpin, const N: usize>(reads: [F; N]) -> JoinReads<F, N> {
    JoinReads { reads, status: [0; N], done: [false; N] }
}

impl<F: Future<Output = u64> + Unpin, const N: usize> Future for JoinReads<F, N> {
    type Output = [u64; N];

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<[u64; N]> {
        let me = self.get_mut(); // all fields Unpin
        let mut all = true;
        for i in 0..N {
            if !me.done[i] {
                // F is Unpin, so a fresh Pin over the array element is sound.
                match Pin::new(&mut me.reads[i]).poll(cx) {
                    Poll::Ready(s) => {
                        me.status[i] = s;
                        me.done[i] = true;
                    }
                    Poll::Pending => all = false,
                }
            }
        }
        if all {
            Poll::Ready(me.status)
        } else {
            Poll::Pending
        }
    }
}

/// Await two *different* futures together. Where `join` is homogeneous (`N`
/// reads), this joins two unlike futures -- the unified-loop case: a block `Read`
/// and an event `Collect` driven on one ring, one `ring_wait`. Each child is
/// polled until it resolves; its output is held until the other catches up, then
/// both return as a pair.
pub struct Join2<A: Future, B: Future> {
    a: A,
    b: B,
    a_out: Option<A::Output>,
    b_out: Option<B::Output>,
}

/// Join two unlike futures. `block_on(join2(read, stream.collect::<N>()))` issues
/// both onto the same ring and resolves once each has completed, returning
/// `(read_output, events)`. The outputs must be `Unpin` (they are held in an
/// `Option` until both finish); every output this module produces -- `u64`,
/// `[u64; N]` -- is.
pub fn join2<A, B>(a: A, b: B) -> Join2<A, B>
where
    A: Future + Unpin,
    B: Future + Unpin,
    A::Output: Unpin,
    B::Output: Unpin,
{
    Join2 { a, b, a_out: None, b_out: None }
}

impl<A, B> Future for Join2<A, B>
where
    A: Future + Unpin,
    B: Future + Unpin,
    A::Output: Unpin,
    B::Output: Unpin,
{
    type Output = (A::Output, B::Output);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut(); // all fields Unpin, so Join2 is Unpin
        if me.a_out.is_none() {
            if let Poll::Ready(v) = Pin::new(&mut me.a).poll(cx) {
                me.a_out = Some(v);
            }
        }
        if me.b_out.is_none() {
            if let Poll::Ready(v) = Pin::new(&mut me.b).poll(cx) {
                me.b_out = Some(v);
            }
        }
        if me.a_out.is_some() && me.b_out.is_some() {
            // Both done: take the held outputs and resolve as a pair.
            Poll::Ready((me.a_out.take().unwrap(), me.b_out.take().unwrap()))
        } else {
            Poll::Pending
        }
    }
}

/// Which side of a `select2` resolved first.
pub enum Either<A, B> {
    A(A),
    B(B),
}

/// Await two futures and resolve as soon as **either** completes -- the "or" to
/// `join2`'s "and".
///
/// This is what a UI event loop needs and `join2` cannot express: a shell waiting
/// on a keyboard stream and a mouse stream wants whichever packet arrives first,
/// not both. Both subscriptions ride one ring and one `ring_wait`, exactly as the
/// module header describes; only the combinator differs.
///
/// **The loser is not lost.** When one side wins, the other's future is dropped
/// without ever having consumed a completion -- but the reactor's `done` table is
/// owned by the *reactor*, not by the future, and `take` removes an entry only on
/// a cookie match. So an event that arrived for the losing side stays queued and
/// is returned by the next `select2` over the same stream. This is the property
/// that makes dropping a half-finished `NextEvent` safe, and it is why a stream's
/// `arm` flag lives on the `EventStream` rather than on `NextEvent`.
///
/// **Biased toward `a`.** `a` is polled first, so if both are ready in the same
/// wake, `a` wins and `b` waits for the next call. With two finite scripted
/// sequences (the smoke case) this only fixes the interleaving; with two live
/// devices a saturated `a` would starve `b`. Callers that need fairness should
/// alternate the argument order. Documented rather than fixed, because the shell
/// drains one event per iteration and neither device can saturate it.
pub struct Select2<A: Future, B: Future> {
    a: A,
    b: B,
}

/// Join two unlike futures into a first-past-the-post race. See `Select2`.
pub fn select2<A, B>(a: A, b: B) -> Select2<A, B>
where
    A: Future + Unpin,
    B: Future + Unpin,
{
    Select2 { a, b }
}

impl<A, B> Future for Select2<A, B>
where
    A: Future + Unpin,
    B: Future + Unpin,
{
    type Output = Either<A::Output, B::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut(); // both fields Unpin, so Select2 is Unpin
        if let Poll::Ready(v) = Pin::new(&mut me.a).poll(cx) {
            return Poll::Ready(Either::A(v));
        }
        if let Poll::Ready(v) = Pin::new(&mut me.b).poll(cx) {
            return Poll::Ready(Either::B(v));
        }
        Poll::Pending
    }
}

// A no-op waker: the executor re-polls its whole future tree after each reap, so
// the waker has nothing to do. (RawWaker boilerplate for a do-nothing Waker.)
const NOOP_VTABLE: RawWakerVTable =
    RawWakerVTable::new(|_| noop_raw(), |_| {}, |_| {}, |_| {});
fn noop_raw() -> RawWaker {
    RawWaker::new(core::ptr::null(), &NOOP_VTABLE)
}
fn noop_waker() -> Waker {
    // SAFETY: the vtable's clone/wake/drop are all no-ops over a null pointer
    // that is never dereferenced.
    unsafe { Waker::from_raw(noop_raw()) }
}

/// Drive `fut` to completion: poll it, and whenever it is `Pending`, ring the
/// doorbell for everything enqueued so far and block in `ring_wait` until the
/// kernel posts a completion, then reap and re-poll. The one place the executor
/// blocks. `fut` must be `Unpin` (every future this module builds is).
pub fn block_on<F: Future + Unpin>(mut fut: F) -> F::Output {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = Pin::new(&mut fut).poll(&mut cx) {
            return v;
        }
        let r = reactor();
        match r.mode {
            Mode::KernelBridged => {
                // The poll above enqueued any not-yet-posted submissions; ring the
                // doorbell (drains the whole SQ in one kernel entry), then block for
                // the next completion and reap it. A redundant submit (nothing new)
                // is a cheap no-op that posts zero.
                let handle = r.handle;
                let _ = sys_ring_submit(handle);
                let _ = sys_ring_wait(handle);
                r.reap();
                // Voice any loss counted during the reap before re-polling -- both
                // the kernel's CQ drops and the reactor's own table overruns. Runs
                // inside the blocking loop deliberately: a stream the caller is not
                // draining (a mouse flood behind a keyboard wait) surfaces here or
                // not at all. No-op unless something was lost, so the deterministic
                // tour is unaffected.
                r.report_losses();
            }
            Mode::Bound => {
                // The poll above enqueued any new reads into the desc/avail rings
                // (the submit is the libOS's own, no kernel entry) but did not ring.
                // Ring once for the whole batch so overlapping reads run together,
                // then park for the next completion(s) and reap them all -- the
                // kernel wakes us on the device's completion IRQ (slice 3).
                // SAFETY: bound geometry is set (init_bound ran before any poll).
                unsafe {
                    r.bound_doorbell();
                    r.bound_park();
                    r.bound_reap();
                }
            }
        }
    }
}
