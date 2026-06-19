//! Event-ring tests.
//!
//! The ring is the pure core of the input path -- a bounded single-producer /
//! single-consumer queue -- so it is exercised directly here, without a device
//! or an interrupt, the same way `elf::parse` and the IPC wait queue are.

use super::TestCtx;
use crate::input::{Event, EventRing, EVENT_KEY, RING_CAP};
use crate::test_assert;

pub fn fifo_order(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut ring = EventRing::new();
    for code in 0..5u16 {
        ring.push(Event { kind: EVENT_KEY, code, value: 1 });
    }
    test_assert!(ring.len() == 5, "wrong length after pushes");
    for code in 0..5u16 {
        let ev = ring.pop().ok_or("ring emptied early")?;
        test_assert!(ev.code == code, "events came back out of order");
    }
    test_assert!(ring.pop().is_none(), "ring not empty after draining");
    Ok(())
}

pub fn empty_pop_is_none(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut ring = EventRing::new();
    test_assert!(ring.is_empty(), "fresh ring not empty");
    test_assert!(ring.pop().is_none(), "pop on empty ring returned an event");
    Ok(())
}

/// Filling past capacity drops the *newest* events (the queued prefix is
/// preserved) and counts every drop.
pub fn overflow_drops_newest(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut ring = EventRing::new();
    // Push CAP+3 events; codes 0..CAP fill the ring, the last 3 are dropped.
    for code in 0..(RING_CAP as u16 + 3) {
        ring.push(Event { kind: EVENT_KEY, code, value: 1 });
    }
    test_assert!(ring.len() == RING_CAP, "ring grew past capacity");
    test_assert!(ring.take_dropped() == 3, "wrong dropped count");
    test_assert!(ring.take_dropped() == 0, "dropped count not cleared after read");

    // The retained events are the oldest CAP (0..CAP), in order.
    for code in 0..RING_CAP as u16 {
        let ev = ring.pop().ok_or("ring emptied early")?;
        test_assert!(ev.code == code, "drop-newest kept the wrong events");
    }
    Ok(())
}

/// After draining, the ring wraps and accepts new events (the head/tail
/// arithmetic is modular, not a one-shot fill).
pub fn wraps_after_drain(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut ring = EventRing::new();
    for code in 0..RING_CAP as u16 {
        ring.push(Event { kind: EVENT_KEY, code, value: 1 });
    }
    for _ in 0..RING_CAP {
        ring.pop().ok_or("ring emptied early")?;
    }
    // A second full cycle must succeed with no drops.
    for code in 100..(100 + RING_CAP as u16) {
        ring.push(Event { kind: EVENT_KEY, code, value: 0 });
    }
    test_assert!(ring.take_dropped() == 0, "spurious drops after wrap");
    let ev = ring.pop().ok_or("ring empty after refill")?;
    test_assert!(ev.code == 100, "wrap lost the first refilled event");
    Ok(())
}

/// A Set-1 scancode becomes a raw key event: the byte is preserved in `code`,
/// the make/break bit surfaces in `value`, and the packing round-trips.
pub fn key_event_encoding(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let make = Event::key(0x1E); // 'A' make
    test_assert!(make.kind == EVENT_KEY, "key event has wrong kind");
    test_assert!(make.code == 0x1E, "make code not preserved raw");
    test_assert!(make.value == 1, "make not flagged as a press");

    let brk = Event::key(0x1E | 0x80); // 'A' break
    test_assert!(brk.code == 0x9E, "break code not preserved raw");
    test_assert!(brk.value == 0, "break not flagged as a release");

    // pack() lays kind/code/value into bits 0..8 / 8..24 / 24..32.
    test_assert!(make.pack() == 0x01_001E_01u64 & 0xFFFFFFFF, "key event packed wrong");
    Ok(())
}
