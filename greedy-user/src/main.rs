//! The CPU-budget demo: spend CPU until the kernel cuts us off.
//!
//! The resource-bound twin of crash-user. Every process is minted a
//! CPU-time capability at spawn (slot CPU_CAP_SLOT); cpu_charge debits it.
//! This process charges in a loop and never voluntarily stops -- so when
//! its budget runs out the kernel terminates it and reclaims everything,
//! and the boot log continues to the next demo. If the "still alive" line
//! below ever appears, budget enforcement is broken.

#![no_std]
#![no_main]

use libplinth::{sys_cpu_charge, sys_exit, sys_write, write_dec, CPU_CAP_SLOT};

/// Ticks spent per round. Chosen with the kernel's INITIAL_CPU_BUDGET so
/// the budget steps cleanly to zero before the overdraw (1024 / 256 = 4).
const CHARGE: u64 = 256;

#[link_section = ".text.entry"]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    sys_write(b"greedy: spending CPU budget\n");

    loop {
        // On overdraw the kernel does not return here -- it terminates us.
        let remaining = sys_cpu_charge(CPU_CAP_SLOT, CHARGE);
        sys_write(b"greedy: charged 256, remaining = ");
        write_dec(remaining);
        sys_write(b"\n");
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
