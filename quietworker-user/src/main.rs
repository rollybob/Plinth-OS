//! A worker that reports a result and says nothing.
//!
//! Identical in shape to grantee-user -- send on the channel the kernel set up
//! at spawn, then exit -- but it writes nothing to the console. That is the
//! whole point: `caprelease-user` spawns it in a loop long enough to overrun a
//! capability table, and a chatty child would put twenty-odd lines into
//! `expected_boot_log.txt` for no diagnostic value.
//!
//! grantee-user is deliberately left alone rather than quietened: its line is
//! part of the spawn demo's asserted output.

#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_send, ENDPOINT_SLOT};

/// The result this worker reports back. The value is not interesting; the
/// caller only checks that the join completed.
const RESULT: u64 = 7;

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_send(ENDPOINT_SLOT, RESULT);
    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
