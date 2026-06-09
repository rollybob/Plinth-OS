//! Plinth kernel entry point.
//!
//! The kernel's job is deliberately small: own the hardware, multiplex it
//! securely, and push every policy decision up to unprivileged library
//! OSes. This file is the boot path; everything interesting happens above
//! the syscall boundary.

#![no_std]
#![no_main]

mod serial;

use bootloader_api::{
    config::{BootloaderConfig, Mapping},
    entry_point,
    info::MemoryRegionKind,
    BootInfo,
};
use core::fmt::Write;

const BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut c = BootloaderConfig::new_default();
    // The frame allocator hands physical frames to userspace, so all of
    // physical memory must be reachable from kernel virtual addresses.
    c.mappings.physical_memory = Some(Mapping::Dynamic);
    c
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    let mut serial = serial::init();
    let _ = writeln!(serial, "plinth: kernel entry");

    let total = boot_info.memory_regions.len();
    let usable = boot_info
        .memory_regions
        .iter()
        .filter(|r| r.kind == MemoryRegionKind::Usable)
        .count();
    let _ = writeln!(serial, "plinth: {total} memory regions ({usable} usable)");

    let _ = writeln!(serial, "plinth: boot ok");
    qemu_exit(ExitCode::Success)
}

#[derive(Clone, Copy)]
#[repr(u32)]
enum ExitCode {
    /// QEMU exits with status (0 << 1) | 1 = 1.
    Success = 0,
    /// QEMU exits with status (1 << 1) | 1 = 3.
    Panic = 1,
}

/// Exit QEMU via the isa-debug-exit device (configured by xtask at port
/// 0xF4). On hardware without the device the write is ignored and we halt.
fn qemu_exit(code: ExitCode) -> ! {
    use x86_64::instructions::port::Port;
    // SAFETY: 0xF4 is the isa-debug-exit device in our QEMU configuration;
    // the write has no effect other than terminating the VM.
    unsafe {
        let mut port = Port::new(0xF4);
        port.write(code as u32);
    }
    loop {
        x86_64::instructions::hlt();
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // Take a fresh handle rather than sharing state with the boot path: the
    // panic may have fired at any point, including mid-write.
    let mut serial = serial::init();
    let _ = writeln!(serial, "plinth: PANIC: {info}");
    qemu_exit(ExitCode::Panic)
}
