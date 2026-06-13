//! The self-paging demo: userspace handles its own page faults.
//!
//! Register a ring-3 fault handler, then touch unmapped pages in the lazy
//! window. Each first touch faults; the kernel upcalls the handler, which
//! maps a frame with the ordinary frame_alloc/frame_map syscalls and
//! returns; the faulting instruction is retried and succeeds. Same #PF as
//! crash-user -- opposite outcome, because this process supplied a policy.

#![no_std]
#![no_main]

use core::ptr::addr_of_mut;

use libplinth::{
    sys_exit, sys_fault_reg, sys_fault_return, sys_frame_alloc, sys_frame_map, sys_write, write_hex,
    LAZY_BASE, PAGE_SIZE, SYS_ERR,
};

/// Stack the fault handler runs on -- separate from the faulting context's
/// stack, and 16-aligned so the handler is entered the way _start is. The
/// field is storage only, referenced by address.
#[repr(align(16))]
struct FaultStack(#[allow(dead_code)] [u8; 4096]);
static mut FAULT_STACK: FaultStack = FaultStack([0; 4096]);

const TOUCH_PAGES: u64 = 4;

/// Entered (via the kernel's upcall) with the faulting address in arg0.
/// Maps that page on demand, then resumes the faulting instruction.
extern "C" fn fault_handler(fault_addr: u64) -> ! {
    let page = fault_addr & !(PAGE_SIZE - 1);
    let slot = sys_frame_alloc();
    if slot == SYS_ERR {
        sys_exit(101);
    }
    if sys_frame_map(slot, page) != 0 {
        sys_exit(102);
    }
    sys_write(b"lazy: serviced fault at ");
    write_hex(page);
    sys_write(b"\n");
    sys_fault_return()
}

#[link_section = ".text.entry"]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    sys_write(b"lazy: registering fault handler\n");

    let stack_top = addr_of_mut!(FAULT_STACK) as u64 + 4096;
    if sys_fault_reg(fault_handler as *const () as u64, stack_top) != 0 {
        sys_exit(100);
    }

    // None of these pages are mapped. Each write traps into the handler,
    // which materializes the page; then the write (and the readback) land.
    let mut i = 0;
    while i < TOUCH_PAGES {
        let va = LAZY_BASE + i * PAGE_SIZE;
        let expect = 0xA11C_0000 + i;
        // SAFETY: the first access faults; the handler maps va read-write
        // for this process before the instruction is retried.
        unsafe {
            let p = va as *mut u64;
            p.write_volatile(expect);
            if p.read_volatile() != expect {
                sys_exit(103);
            }
        }
        i += 1;
    }

    sys_write(b"lazy: all pages materialized on demand\n");
    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
