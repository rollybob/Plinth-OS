//! Input event demo (Stage 2): read an input event through an EventSource
//! capability the kernel granted, and show the multiplexing gate -- a holder
//! cannot read through a capability that is not an event source.

#![no_std]
#![no_main]

use libplinth::{
    event_code, sys_event_recv, sys_exit, sys_write, write_hex, CPU_CAP_SLOT, EVENT_OK,
    EVENT_SOURCE_SLOT,
};

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    // Multiplexing gate: the CPU-time budget is not a readable event source, so
    // reading through its slot is rejected.
    let (status, _) = sys_event_recv(CPU_CAP_SLOT);
    if status != EVENT_OK {
        sys_write(b"evt: non-source rejected\n");
    }

    // Read one event through the granted source. The ring is empty, so this
    // blocks until an event arrives -- the kernel idles waiting for input. In
    // the smoke a synthetic scancode is delivered; a real keypress does so
    // otherwise.
    let (status, ev) = sys_event_recv(EVENT_SOURCE_SLOT);
    if status == EVENT_OK {
        sys_write(b"evt: got scancode ");
        write_hex(event_code(ev) as u64);
        sys_write(b"\n");
    }

    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
