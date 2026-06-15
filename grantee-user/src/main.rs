//! Spawn + wait demo (worker half).
//!
//! Launched by spawner-user via `spawn`. The kernel gives every spawned child
//! a send capability to its parent's result channel, at ENDPOINT_SLOT. This
//! worker computes a result and sends it back; the parent is waiting on the
//! matching receive end. The worker then exits -- the result travels over IPC,
//! not through the exit code (spawn is no longer synchronous).

#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_send, sys_write, ENDPOINT_SLOT};

/// The result this worker reports back to its parent.
const RESULT: u64 = 42;

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_write(b"worker: computing result\n");
    // Send the result on the channel the kernel set up at spawn; the parent's
    // recv collects it.
    sys_send(ENDPOINT_SLOT, RESULT);
    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
