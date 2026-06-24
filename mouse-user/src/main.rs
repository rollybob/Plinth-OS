//! Mouse input demo (Design/mouse_input.md S2): an event STREAM over the
//! second `EventSource`.
//!
//! Mirrors `evtstream-user` exactly, but subscribes to source 1 (the mouse)
//! instead of source 0 (the keyboard): one multishot subscription through
//! the libos reference executor, reaping a SEQUENCE of packed
//! `EVENT_MOUSE_MOVE` events and asserting each decodes to the expected
//! dx/dy/buttons, in order -- never transcript-matched.
//!
//! The kernel grants this process one capability: a read EventSource on the
//! mouse (source 1). In headless smoke a scripted packet sequence is
//! injected (input::arm_synthetic_mouse in main.rs), driving the exact
//! record -> route -> wake path a real IRQ12 packet would; a real mouse
//! drives it otherwise.

#![no_std]
#![no_main]

use libos::ring;
use libplinth::{
    event_kind, mouse_buttons, mouse_dx, mouse_dy, sys_exit, sys_write, write_dec,
    EVENT_MOUSE_MOVE, EVENT_SOURCE_SLOT,
};

/// The scripted packets this demo expects, in order: (dx, dy, buttons). Must
/// match the sequence main.rs arms via `input::arm_synthetic_mouse`. Three
/// distinct packets -- including one with a button down -- so a misrouted,
/// reordered, or mis-decoded event is caught.
const SEQUENCE: [(i8, i8, u8); 3] = [(10, -5, 0x00), (-20, 15, 0x01), (3, 3, 0x02)];

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_write(b"mouse: start\n");

    if !ring::init() {
        sys_write(b"mouse: ring init failed\n");
        sys_exit(1);
    }

    // Arm one multishot subscription on the mouse. Nothing blocks yet; from
    // here every packet posts a completion tagged with this stream's cookie.
    let mut stream = ring::subscribe(EVENT_SOURCE_SLOT);

    // Reap the scripted sequence: one event per next(), in arrival order.
    // Each block_on parks in ring_wait until the next packet's completion is
    // posted and reaped -- the kernel idles on input meanwhile.
    let mut ok = true;
    for &(want_dx, want_dy, want_buttons) in SEQUENCE.iter() {
        let ev = ring::block_on(stream.next());
        if event_kind(ev) != EVENT_MOUSE_MOVE
            || mouse_dx(ev) != want_dx
            || mouse_dy(ev) != want_dy
            || mouse_buttons(ev) != want_buttons
        {
            ok = false;
        }
    }

    // Cancel the live subscription: posts RING_OP_CANCEL, exercising the
    // cancel dispatch end to end, same as evtstream-user.
    stream.cancel();

    if ok {
        sys_write(b"mouse: ok (");
        write_dec(SEQUENCE.len() as u64);
        sys_write(b" packets in order)\n");
        sys_exit(0);
    } else {
        sys_write(b"mouse: FAIL\n");
        sys_exit(4);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
