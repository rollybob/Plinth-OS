//! Async input demo (Stage 3): a multishot event STREAM made observable.
//!
//! Where evt-user reads a single event through the blocking shim, this opens a
//! multishot subscription on the keyboard EventSource through the libos reference
//! executor -- a stream future over the same completion-ring reactor the async
//! block demo uses -- and reaps a SEQUENCE of events from it. One
//! RING_OP_EVENT_SUB arms the subscription; every scancode then posts a
//! completion the kernel tags with this stream's cookie, and the executor yields
//! them one per `next()`. Correctness is asserted, never transcript-matched: each
//! event must arrive once, in order, carrying its scancode -- the many-event path
//! the single-shot event_recv could not express (Design/event_rings.md s2/s6/s8).
//!
//! The kernel grants this process one capability: a read EventSource on the
//! keyboard (source 0). In headless smoke a scripted scancode sequence is
//! injected (input::arm_synthetic in main.rs), driving the exact
//! record -> route -> wake path a real keypress would; a real keyboard drives it
//! otherwise.

#![no_std]
#![no_main]

use libos::ring;
use libplinth::{
    event_code, event_kind, sys_exit, sys_write, write_dec, EVENT_KEY, EVENT_SOURCE_SLOT,
};

/// The scripted scancodes this demo expects, in order. Must match the sequence
/// main.rs arms via `input::arm_synthetic`. Set-1 make codes for 'a','b','c','d'
/// -- four distinct keys, so a misrouted or reordered event is caught.
const SEQUENCE: [u16; 4] = [0x1E, 0x30, 0x2E, 0x20];

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_write(b"evtstream: start\n");

    if !ring::init() {
        sys_write(b"evtstream: ring init failed\n");
        sys_exit(1);
    }

    // Arm one multishot subscription on the keyboard. Nothing blocks yet; from
    // here every scancode posts a completion tagged with this stream's cookie.
    let mut stream = ring::subscribe(EVENT_SOURCE_SLOT);

    // Reap the scripted sequence: one event per next(), in arrival order. Each
    // block_on parks in ring_wait until the next event's completion is posted and
    // reaped -- the kernel idles on input meanwhile.
    let mut ok = true;
    for &want in SEQUENCE.iter() {
        let ev = ring::block_on(stream.next());
        if event_kind(ev) != EVENT_KEY || event_code(ev) != want {
            ok = false;
        }
    }

    // Cancel the live subscription: posts RING_OP_CANCEL, exercising the cancel
    // dispatch end to end. Process teardown would drop the subscription anyway,
    // but a long-lived reader that is done with a source cancels it explicitly.
    stream.cancel();

    if ok {
        sys_write(b"evtstream: ok (");
        write_dec(SEQUENCE.len() as u64);
        sys_write(b" events in order)\n");
        sys_exit(0);
    } else {
        sys_write(b"evtstream: FAIL\n");
        sys_exit(4);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
