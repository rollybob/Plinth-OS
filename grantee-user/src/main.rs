//! The capability-transfer demo (child half).
//!
//! Spawned by spawner-user, which transferred it a frame capability. This
//! process never called frame_alloc -- the only reason it can map and use a
//! frame is that the capability was handed to it, in its own table, at
//! GRANT_SLOT. It proves the access works, then returns a result to the
//! parent as its exit code.

#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_frame_map, sys_write, write_hex, GRANT_SLOT, MAP_BASE};

/// Where this process chooses to map the granted frame.
const MAP_VA: u64 = MAP_BASE + 0x4000;

const PATTERN: u64 = 0xfeed_face_0000_0042;

#[link_section = ".text.entry"]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Map the inherited capability at an address of our choosing.
    if sys_frame_map(GRANT_SLOT, MAP_VA) != 0 {
        sys_exit(102);
    }
    // SAFETY: the granted frame is now mapped read-write for this process.
    unsafe {
        let p = MAP_VA as *mut u64;
        p.write_volatile(PATTERN);
        if p.read_volatile() != PATTERN {
            sys_exit(103);
        }
    }
    sys_write(b"grantee: used granted frame at ");
    write_hex(MAP_VA);
    sys_write(b"\n");

    // The result the parent collects from spawn().
    sys_exit(42)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
