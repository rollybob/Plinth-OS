//! A minimal Plinth program: the smallest thing that runs in ring 3 and
//! exits cleanly. Copy this crate to start your own program -- see GUIDE.md.
//!
//! A Plinth program is `#![no_std]` / `#![no_main]`: there is no Rust
//! runtime and no libc. It links `libplinth` (the raw syscall shim) and,
//! optionally, a library OS for memory policy -- see the `libos`/`demo-app`
//! crates and `bump-user` for that pattern. It must define `_start` (the
//! ELF entry point) and a panic handler; nothing else is provided for it.

#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_write};

/// The entry point. The kernel enters here in ring 3 with a fresh stack and
/// no arguments, so `_start` takes none and never returns -- there is
/// nothing to return to, so it ends in `sys_exit`. `#[no_mangle]` keeps the
/// symbol named `_start`, which the linker script names as the ELF entry
/// (`ENTRY(_start)`); the kernel jumps to whatever `e_entry` resolves to.
#[no_mangle]
pub extern "C" fn _start() -> ! {
    sys_write(b"template: hello from a fresh Plinth program\n");
    sys_exit(0)
}

/// Every `#![no_std]` binary must define how a panic terminates. With no
/// runtime to unwind into, the only sensible action is to exit with a
/// marker code.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(255)
}
