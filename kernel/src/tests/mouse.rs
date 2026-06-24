//! PS/2 mouse tests: pure packet-decode logic (Design/mouse_input.md S1/S2).
//!
//! No IRQ, no device -- `Packet::push`/`decode_axis` are exercised directly,
//! the same way `tests::input` exercises `Event` encoding without a real
//! keystroke.

use super::TestCtx;
use crate::input::{Event, EVENT_MOUSE_MOVE};
use crate::mouse::{decode_axis, Packet};
use crate::test_assert;

/// A mouse_move event packs dx (high byte of `code`) and dy (low byte),
/// masks buttons to 7 bits (clear of the CQ's bit-31 drop flag), and the
/// packing round-trips through `pack()`.
pub fn mouse_event_encoding(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let ev = Event::mouse_move(-10, 20, 0x03);
    test_assert!(ev.kind == EVENT_MOUSE_MOVE, "mouse event has wrong kind");
    test_assert!(ev.code == 0xF614, "dx/dy not packed into code as expected");
    test_assert!(ev.value == 0x03, "button bitmask not preserved");

    // A button bitmask with bit 7 set must be masked off -- it would
    // otherwise collide with the CQ's EVENT_DROPPED flag (rings.rs bit 31).
    let masked = Event::mouse_move(0, 0, 0xFF);
    test_assert!(masked.value == 0x7F, "button bitmask not masked to 7 bits");

    let packed = ev.pack();
    test_assert!(packed & 0xFF == EVENT_MOUSE_MOVE as u64, "packed kind wrong");
    test_assert!((packed >> 8) & 0xFFFF == 0xF614, "packed code wrong");
    test_assert!((packed >> 24) & 0xFF == 0x03, "packed value wrong");
    Ok(())
}

/// A packet assembles after exactly 3 bytes, decoding positive dx/dy and an
/// empty button mask when no sign/button bits are set.
pub fn mouse_packet_assembles(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut p = Packet::new();
    test_assert!(p.push(0x08).is_none(), "byte 0 alone must not complete a packet");
    test_assert!(p.push(10).is_none(), "byte 1 alone must not complete a packet");
    test_assert!(p.push(20) == Some((10, 20, 0)), "byte 2 must complete and decode the packet");
    Ok(())
}

/// Button bits and both sign bits decode correctly together.
pub fn mouse_packet_buttons_and_signs(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut p = Packet::new();
    let flags = 0x08 | 0x03 | 0x10 | 0x20; // framing, left+right, X sign, Y sign
    test_assert!(p.push(flags).is_none(), "byte 0 alone must not complete a packet");
    test_assert!(p.push(0xF6).is_none(), "byte 1 alone must not complete a packet");
    test_assert!(p.push(0xEC) == Some((-10, -20, 0x03)), "signed packet decoded wrong");
    Ok(())
}

/// A byte seen at packet position 0 without the framing bit set is dropped,
/// not accepted as a new packet's start -- the resync check.
pub fn mouse_packet_resyncs(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut p = Packet::new();
    test_assert!(p.push(0x00).is_none(), "a non-framing byte must be dropped, not started");
    test_assert!(p.push(0x08).is_none(), "the next genuine byte 0 must start a fresh packet");
    test_assert!(p.push(1).is_none(), "byte 1 alone must not complete a packet");
    test_assert!(p.push(2) == Some((1, 2, 0)), "packet after resync decoded wrong");
    Ok(())
}

/// `decode_axis` clamps a 9-bit two's-complement value to i8 range rather
/// than wrapping or panicking.
pub fn mouse_axis_clamps(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    test_assert!(decode_axis(5, false) == 5, "positive axis decoded wrong");
    test_assert!(decode_axis(0xFB, true) == -5, "negative axis decoded wrong");
    test_assert!(decode_axis(0, true) == i8::MIN, "large negative axis not clamped to i8::MIN");
    Ok(())
}
