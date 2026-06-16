//! Spawn + crash-reaping demo (faulting worker half).
//!
//! Spawned by spawner-user as a second worker. Unlike grantee-user -- which
//! sends its result and exits cleanly -- this worker FAULTS before it ever
//! sends, standing in for any child that dies mid-task. Its parent is blocked
//! in `recv` on the result channel the kernel set up at spawn; the kernel's
//! death-time reaping must wake that `recv` with `IPC_PEER_DIED` instead of
//! leaving the parent blocked forever. The endpoint is then reclaimed once both
//! sides' capabilities are gone -- a crash leaks nothing.

#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_write};

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_write(b"faultchild: faulting before send\n");

    // Volatile write through a null pointer: page 0 is unmapped and no fault
    // handler is registered, so the kernel terminates this process here --
    // before it can send on the result channel. The parent's recv must then
    // observe IPC_PEER_DIED, not a value.
    // SAFETY: deliberately invalid; faulting is the entire purpose.
    unsafe {
        core::ptr::null_mut::<u64>().write_volatile(0xdead_beef);
    }

    // Unreachable: the fault already terminated us. A safety net only.
    sys_write(b"faultchild: still alive -- reaping FAILED\n");
    sys_exit(2)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
