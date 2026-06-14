//! The capability-transfer demo (parent half).
//!
//! Allocate a frame, then spawn a child in its own isolated address space,
//! transferring the frame capability to it. The child runs to completion
//! and returns a result the parent collects. The parent never hands the
//! child any data -- only the capability -- and the child proves it has
//! access by using a frame it never allocated.

#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_frame_alloc, sys_spawn, sys_write, write_dec, SYS_ERR};

/// Spawnable child id (see the kernel's SPAWNABLE table).
const GRANTEE_ID: u64 = 0;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let slot = sys_frame_alloc();
    if slot == SYS_ERR {
        sys_exit(101);
    }
    sys_write(b"spawner: allocated a frame, granting it to a child\n");

    // Hand the frame capability to the child; block until it finishes.
    let code = sys_spawn(GRANTEE_ID, slot);

    sys_write(b"spawner: child returned ");
    write_dec(code);
    sys_write(b"\n");
    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
