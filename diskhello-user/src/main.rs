//! The program loaded from disk (loadee half of the load-from-disk demo).
//!
//! This binary is never embedded in the kernel; it lives only in the boot
//! archive. The FS libOS (fsdemo-user) reads it off the archive device and
//! launches it with spawn_from_buffer, which hands it a send capability to the
//! result channel at ENDPOINT_SLOT -- exactly as a spawn-by-id child gets one.
//! It prints a line so the serial log shows it actually ran in ring 3, then
//! sends a recognizable result back to its launcher and exits.

#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_send, sys_write, ENDPOINT_SLOT};

/// Reported back to the launcher; the smoke test checks fsdemo echoes it.
const RESULT: u64 = 777;

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_write(b"diskhello: running from disk\n");
    sys_send(ENDPOINT_SLOT, RESULT);
    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
