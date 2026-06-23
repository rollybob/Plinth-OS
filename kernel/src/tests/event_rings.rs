//! Event-ring routing + CQ-full backpressure tests (event_rings.md, Stage 1).
//!
//! `Subscriptions` is the multishot-routing core, factored as pure logic over a
//! fixed pool -- no CQ frame, no IRQ, no scheduler -- exactly as `Inflights` is
//! the block-completion demux core. Here we drive it the way `record` will in
//! Stage 2: deliver an event on a source and route it, via a closure standing in
//! for the real CQ post, into each subscribing ring's fake CQ. These pin the two
//! invariants the smoke cannot isolate:
//!
//!   - an event reaches *exactly* the subscriptions on its source, each in its
//!     own ring's CQ with its own cookie, never cross-delivered (the security
//!     edge: cross-tenant input delivery on a routing bug); and
//!   - a full CQ drops the *newest* event, bumps a sticky per-subscription count,
//!     and surfaces the loss as the drop flag on the next admitted completion
//!     (no silent input loss).

use super::TestCtx;
use crate::rings::{Subscriptions, EVENT_DROPPED, MAX_SUBS};
use crate::test_assert;

/// A fake CQ: a bounded queue of `(user_data, status)` the routing closure posts
/// into, standing in for one ring's shared-memory CQ. `post` returns false when
/// full -- the drop-newest signal the real CQ-full check (`tail - head ==
/// entries`) will give in Stage 2. Capacity is deliberately tiny so overflow is
/// easy to drive.
const FCAP: usize = 4;
const NRINGS: usize = 3;

struct FakeCq {
    buf: [(u64, u32); FCAP],
    head: usize,
    len: usize,
}

impl FakeCq {
    const fn new() -> FakeCq {
        FakeCq { buf: [(0, 0); FCAP], head: 0, len: 0 }
    }

    /// Admit a completion, or refuse (false) when full -- never overwrite an
    /// unreaped slot.
    fn post(&mut self, user_data: u64, status: u32) -> bool {
        if self.len == FCAP {
            return false;
        }
        let tail = (self.head + self.len) % FCAP;
        self.buf[tail] = (user_data, status);
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<(u64, u32)> {
        if self.len == 0 {
            return None;
        }
        let e = self.buf[self.head];
        self.head = (self.head + 1) % FCAP;
        self.len -= 1;
        Some(e)
    }

    fn depth(&self) -> usize {
        self.len
    }
}

/// Route one event on `source` into the bank of fake CQs, indexed by ring id --
/// the closure is the per-subscription CQ post the live `record` path supplies.
fn deliver_into(subs: &mut Subscriptions, cqs: &mut [FakeCq; NRINGS], source: usize, event: u32) {
    subs.deliver(source, event, |ring, ud, status| cqs[ring].post(ud, status));
}

fn fresh_cqs() -> [FakeCq; NRINGS] {
    [FakeCq::new(), FakeCq::new(), FakeCq::new()]
}

/// An event reaches exactly the subscriptions on its source, each landing in its
/// own ring's CQ under its own cookie -- and never in a CQ subscribed to a
/// different source.
pub fn routes_by_source_and_cookie(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut subs = Subscriptions::new();
    let mut cqs = fresh_cqs();
    subs.subscribe(0, 0, 100).ok_or("subscribe source0->ring0")?;
    subs.subscribe(0, 1, 200).ok_or("subscribe source0->ring1")?;
    subs.subscribe(1, 2, 300).ok_or("subscribe source1->ring2")?;

    // A source-0 event reaches ring0 and ring1, each with its cookie; not ring2.
    deliver_into(&mut subs, &mut cqs, 0, 0xABCD);
    test_assert!(cqs[0].pop() == Some((100, 0xABCD)), "ring0 missed its source-0 event");
    test_assert!(cqs[1].pop() == Some((200, 0xABCD)), "ring1 missed its source-0 event");
    test_assert!(cqs[2].depth() == 0, "source-0 event leaked to a source-1 ring");

    // A source-1 event reaches only ring2.
    deliver_into(&mut subs, &mut cqs, 1, 0x0011);
    test_assert!(cqs[2].pop() == Some((300, 0x0011)), "ring2 missed its source-1 event");
    test_assert!(cqs[0].depth() == 0 && cqs[1].depth() == 0, "source-1 event leaked to source-0 rings");
    Ok(())
}

/// An event on a source nobody subscribed is dropped (S6: no pre-subscription
/// buffering) -- it reaches no CQ.
pub fn no_subscription_drops(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut subs = Subscriptions::new();
    let mut cqs = fresh_cqs();
    subs.subscribe(0, 0, 1).ok_or("subscribe source0")?;

    deliver_into(&mut subs, &mut cqs, 1, 0x55); // no subscription on source 1
    test_assert!(cqs[0].depth() == 0 && cqs[1].depth() == 0 && cqs[2].depth() == 0,
        "an unsubscribed source's event was delivered somewhere");
    Ok(())
}

/// Overflowing a CQ drops the newest events and counts each drop; the queued
/// prefix (the oldest FCAP) survives in order. A subscription on another source
/// and ring is unaffected -- drop accounting is per-subscription, per-CQ.
pub fn overflow_drops_newest_and_counts(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut subs = Subscriptions::new();
    let mut cqs = fresh_cqs();
    subs.subscribe(0, 0, 1).ok_or("subscribe source0->ring0 (overflow target)")?;
    subs.subscribe(1, 1, 2).ok_or("subscribe source1->ring1 (sibling)")?;

    // Fill ring0's CQ exactly (distinct payloads to check order); no drops yet.
    for i in 0..FCAP as u32 {
        deliver_into(&mut subs, &mut cqs, 0, 0x10 + i);
    }
    test_assert!(cqs[0].depth() == FCAP, "CQ did not fill to capacity");
    test_assert!(subs.dropped(0, 1) == Some(0), "dropped count nonzero before overflow");

    // Two more overflow ring0 -- dropped and counted, not overwriting the prefix.
    deliver_into(&mut subs, &mut cqs, 0, 0xAA);
    deliver_into(&mut subs, &mut cqs, 0, 0xBB);
    test_assert!(cqs[0].depth() == FCAP, "overflow overwrote an unreaped CQ slot");
    test_assert!(subs.dropped(0, 1) == Some(2), "wrong drop count after two overflows");

    // The sibling subscription delivers normally and carries no drops of its own.
    deliver_into(&mut subs, &mut cqs, 1, 0x07);
    test_assert!(cqs[1].pop() == Some((2, 0x07)), "sibling subscription stopped delivering");
    test_assert!(subs.dropped(1, 2) == Some(0), "ring0's overflow leaked onto the sibling's count");

    // The retained ring0 prefix is the oldest FCAP events, in order (drop-newest).
    for i in 0..FCAP as u32 {
        test_assert!(cqs[0].pop() == Some((1, 0x10 + i)), "drop-newest kept the wrong events");
    }
    Ok(())
}

/// After a drop, the next successfully-posted event for that subscription carries
/// the drop flag (and only that one), and the sticky count then clears.
pub fn drop_flag_on_next_admitted(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut subs = Subscriptions::new();
    let mut cqs = fresh_cqs();
    subs.subscribe(0, 0, 7).ok_or("subscribe ring0")?;

    // Fill, then overflow once.
    for i in 0..FCAP as u32 {
        deliver_into(&mut subs, &mut cqs, 0, 0x10 + i);
    }
    deliver_into(&mut subs, &mut cqs, 0, 0x99); // dropped
    test_assert!(subs.dropped(0, 7) == Some(1), "overflow not counted");

    // Free a slot, then deliver -- this one is admitted and must carry the flag.
    let first = cqs[0].pop().ok_or("CQ empty")?;
    test_assert!(first == (7, 0x10), "wrong oldest event");
    deliver_into(&mut subs, &mut cqs, 0, 0x21);
    test_assert!(subs.dropped(0, 7) == Some(0), "drop count not cleared after flag surfaced");

    // The prefix events carry no flag; the final (newly admitted) one does.
    for i in 1..FCAP as u32 {
        let (ud, status) = cqs[0].pop().ok_or("prefix event missing")?;
        test_assert!(ud == 7 && status == 0x10 + i, "wrong prefix event");
        test_assert!(status & EVENT_DROPPED == 0, "drop flag set on a pre-drop event");
    }
    let last = cqs[0].pop().ok_or("flagged event missing")?;
    test_assert!(last == (7, 0x21 | EVENT_DROPPED), "drop flag not set on the next admitted event");
    Ok(())
}

/// Cancel removes exactly one subscription; siblings keep delivering, and a
/// second cancel of the same key is a no-op.
pub fn cancel_stops_delivery(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut subs = Subscriptions::new();
    let mut cqs = fresh_cqs();
    subs.subscribe(0, 0, 100).ok_or("subscribe ring0")?;
    subs.subscribe(0, 1, 200).ok_or("subscribe ring1")?;

    test_assert!(subs.cancel(0, 100), "cancel of a live subscription returned false");
    test_assert!(!subs.cancel(0, 100), "second cancel of the same key returned true");

    deliver_into(&mut subs, &mut cqs, 0, 0x77);
    test_assert!(cqs[0].depth() == 0, "cancelled subscription still received events");
    test_assert!(cqs[1].pop() == Some((200, 0x77)), "sibling subscription stopped delivering");
    Ok(())
}

/// Releasing a ring drops all of its subscriptions (teardown), across sources,
/// while leaving other rings' subscriptions intact.
pub fn release_ring_clears_all(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut subs = Subscriptions::new();
    let mut cqs = fresh_cqs();
    subs.subscribe(0, 0, 1).ok_or("subscribe source0->ring0")?;
    subs.subscribe(1, 0, 2).ok_or("subscribe source1->ring0")?;
    subs.subscribe(0, 1, 3).ok_or("subscribe source0->ring1")?;

    subs.release_ring(0);

    // Both ring0 subscriptions (on either source) are gone; ring1's survives.
    deliver_into(&mut subs, &mut cqs, 0, 0x33);
    deliver_into(&mut subs, &mut cqs, 1, 0x44);
    test_assert!(cqs[0].depth() == 0, "released ring still received events");
    test_assert!(cqs[1].pop() == Some((3, 0x33)), "release wrongly dropped another ring's subscription");
    Ok(())
}

/// The pool accepts up to `MAX_SUBS` distinct subscriptions then refuses; a
/// duplicate `(ring, user_data)` is refused even with room, while the same
/// cookie on a different ring (or a new cookie on the same ring) is accepted.
pub fn pool_full_and_duplicates_rejected(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut subs = Subscriptions::new();
    for i in 0..MAX_SUBS as u64 {
        subs.subscribe(0, 0, i).ok_or("pool should accept up to MAX_SUBS")?;
    }
    test_assert!(subs.subscribe(0, 0, 9999).is_none(), "full pool accepted another subscription");

    let mut s2 = Subscriptions::new();
    s2.subscribe(0, 0, 100).ok_or("first subscribe")?;
    test_assert!(s2.subscribe(0, 0, 100).is_none(), "duplicate (ring, user_data) accepted");
    test_assert!(s2.subscribe(0, 0, 101).is_some(), "same ring, new cookie rejected");
    test_assert!(s2.subscribe(0, 1, 100).is_some(), "same cookie, different ring rejected");
    Ok(())
}
