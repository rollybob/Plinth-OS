//! The shared demo workload, linked against the free-list library OS.
//! Identical application code to bump-user -- the only difference is
//! which OS personality manages its memory.

#![no_std]
#![no_main]

use libos::FreeListAlloc;
use libplinth::sys_exit;

#[link_section = ".text.entry"]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut policy = FreeListAlloc::new();
    demo_app::run(&mut policy);
    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
