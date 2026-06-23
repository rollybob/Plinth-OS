//! Input event tests.
//!
//! The per-source `EventRing` retired with the move to multishot event rings
//! (event_rings.md S6): its bounded-SPSC + drop-newest logic now lives on the
//! CQ and is exercised by `tests::event_rings`. What remains here is the pure
//! `Event` encoding -- the raw scancode -> packed-event mapping the kernel ships
//! and the libOS keymap consumes.

use super::TestCtx;
use crate::input::{Event, EVENT_KEY};
use crate::test_assert;

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
