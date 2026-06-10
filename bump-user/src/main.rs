//! The shared demo workload, linked against the bump library OS.

#![no_std]
#![no_main]

use libos::BumpAlloc;
use libplinth::sys_exit;

#[link_section = ".text.entry"]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut policy = BumpAlloc::new();
    demo_app::run(&mut policy);
    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
