//! The fault-isolation demo: dereference null, on purpose.
//!
//! The kernel's #PF handler should log the fault, kill this process,
//! and carry on running the next one. If the second write below ever
//! appears on the console, isolation is broken.

#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_write};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    sys_write(b"crash: about to dereference null\n");

    // Volatile write through a null pointer: page 0 is unmapped, so this
    // raises #PF from ring 3 immediately.
    unsafe {
        core::ptr::null_mut::<u64>().write_volatile(0xdead_beef);
    }

    sys_write(b"crash: still alive -- fault isolation FAILED\n");
    sys_exit(2)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
