//! Console line demo (Stage 3): read a line of input through the keyboard
//! EventSource and echo it. The kernel delivered raw scancodes; libinput (an
//! unprivileged library OS) turned them into characters and assembled the line.
//! "Input is output-only" is retired.

#![no_std]
#![no_main]

use libinput::read_line;
use libplinth::{sys_exit, sys_write, EVENT_SOURCE_SLOT};

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_write(b"kbd: type a line\n");

    let mut buf = [0u8; 64];
    let n = read_line(EVENT_SOURCE_SLOT, &mut buf);

    sys_write(b"kbd: read '");
    sys_write(&buf[..n]);
    sys_write(b"'\n");

    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
