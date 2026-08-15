//! A reference async executor over the kernel's completion rings -- block I/O
//! and input events on one ring.
//!
//! This is the library-OS half of Design/async_rings.md and Design/event_rings.md:
//! the kernel ships the ring *mechanism* (register/submit/wait + the in-flight
//! demux); this is one *policy* built on it -- a minimal `no_std` futures executor
//! a real library OS may replace. It is what makes depth observable: a caller
//! issues several reads that overlap on the device, then awaits them all, the
//! kernel demuxing each completion back by its `user_data` cookie.
//!
//! Two future shapes ride the same reactor (event_rings.md s2/s6):
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
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use libplinth::{
    sys_frame_alloc, sys_frame_map, sys_ring_dropped, sys_ring_register, sys_ring_submit,
    sys_ring_wait, sys_write, write_dec, MAP_END, PAGE_SIZE, SYS_ERR,
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

// Ring header / entry layout (Design/async_rings.md s4), byte offsets.
const HDR_HEAD: u64 = 0;
const HDR_TAIL: u64 = 4;
const HDR_MASK: u64 = 8;
const HDR_SIZE: u64 = 16;
const SQ_ENTRY: u64 = 32;
const CQ_ENTRY: u64 = 16;

// SQ entry `op` selectors (Design/async_rings.md s4, event_rings.md s4,
// Design/block_write.md S1).
const RING_OP_READ: u32 = 0;
const RING_OP_EVENT_SUB: u32 = 1;
const RING_OP_CANCEL: u32 = 2;
const RING_OP_WRITE: u32 = 3;

/// Drop-flag bit in an event completion's `status` (event_rings.md s5): the
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

/// The per-process reactor: the registered ring plus the table of completions
/// reaped from the CQ but not yet consumed by a future's `poll`.
struct Reactor {
    ready: bool,
    handle: u64,
    /// Monotonic cookie source; each read gets a unique `user_data`.
    next_ud: u64,
    /// Reaped completions awaiting a matching `poll`: (user_data, status).
    done: [(u64, u64); CAP],
    done_len: usize,
    /// Count of event completions reaped carrying the kernel's `EVENT_DROPPED`
    /// flag -- one per burst of CQ-full drops the kernel surfaced (each flagged
    /// completion marks at least one lost event since the previous one on that
    /// stream, event_rings.md s5). A lower bound on total loss, not the exact
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

/// How many event completions this reactor has reaped carrying the kernel's
/// `EVENT_DROPPED` flag since the process started -- a monotonically rising
/// count of the bursts in which the CQ overran and the kernel dropped input
/// (event_rings.md s5). Zero on a reader that keeps up. `block_on` already voices
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
/// `RING_OP_WRITE` (Design/block_write.md S1) -- same entry shape either way.
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
/// path's `range_slot` field, event_rings.md s4) and `ud` is the subscription
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

/// A pending block write -- the write half of `Read` (Design/block_write.md
/// S1). Same shape and lifecycle: nothing is submitted until first polled,
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
/// the flipped direction from `read` (Design/block_write.md S3).
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

/// A multishot event subscription over the ring (event_rings.md s6). Unlike a
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

/// Await several reads together: polls each unfinished child on every poll, so
/// they all enqueue up front and overlap on the device. Resolves to each read's
/// status, indexed as the input array.
pub struct JoinReads<const N: usize> {
    reads: [Read; N],
    status: [u64; N],
    done: [bool; N],
}

/// Join `N` reads into one future. `block_on(join([...]))` issues them all,
/// then resolves once every one has completed.
pub fn join<const N: usize>(reads: [Read; N]) -> JoinReads<N> {
    JoinReads { reads, status: [0; N], done: [false; N] }
}

impl<const N: usize> Future for JoinReads<N> {
    type Output = [u64; N];

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<[u64; N]> {
        let me = self.get_mut(); // all fields Unpin
        let mut all = true;
        for i in 0..N {
            if !me.done[i] {
                // Read is Unpin, so a fresh Pin over the array element is sound.
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
        // The poll above enqueued any not-yet-posted submissions; ring the
        // doorbell (drains the whole SQ in one kernel entry), then block for the
        // next completion and reap it. A redundant submit (nothing new) is a
        // cheap no-op that posts zero.
        let handle = reactor().handle;
        let _ = sys_ring_submit(handle);
        let _ = sys_ring_wait(handle);
        let r = reactor();
        r.reap();
        // Voice any loss counted during the reap before re-polling -- both the
        // kernel's CQ drops and the reactor's own table overruns. Runs inside the
        // blocking loop deliberately: a stream the caller is not draining (a mouse
        // flood behind a keyboard wait) surfaces here or not at all. No-op unless
        // something was lost, so the deterministic tour is unaffected.
        r.report_losses();
    }
}
