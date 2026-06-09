//! Plinth kernel entry point.
//!
//! The kernel's job is deliberately small: own the hardware, multiplex it
//! securely, and push every policy decision up to unprivileged library
//! OSes. This file is the boot path; everything interesting happens above
//! the syscall boundary.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

mod capability;
mod frame_alloc;
mod gdt;
mod interrupts;
mod memory;
// process/usermode are driven from the normal boot path only; the test
// build stops before userspace, so silence their dead-code noise there.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod process;
mod serial;
mod syscall;
#[cfg(feature = "tests")]
mod tests;
#[cfg_attr(feature = "tests", allow(dead_code))]
mod usermode;

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

    let phys_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("bootloader did not map physical memory");

    let frames = frame_alloc::FrameAlloc::new(&boot_info.memory_regions, phys_offset);
    let _ = writeln!(
        serial,
        "plinth: frame allocator ready ({} frames free)",
        frames.free_frames()
    );
    *frame_alloc::FRAME_ALLOC.lock() = Some(frames);

    memory::init(phys_offset);

    let selectors = gdt::init();
    let _ = writeln!(serial, "plinth: GDT + TSS loaded");

    interrupts::init();
    let _ = writeln!(serial, "plinth: IDT loaded");

    syscall::init(&selectors);
    let _ = writeln!(serial, "plinth: syscall interface ready");

    // Test build: run the suite and exit immediately -- never continue to
    // userspace. The exit code tells xtask whether QEMU died unexpectedly,
    // but pass/fail is judged from the [SUITE] serial line.
    #[cfg(feature = "tests")]
    {
        let mut guard = frame_alloc::FRAME_ALLOC.lock();
        let mut ctx = tests::TestCtx {
            frames: guard.as_mut().expect("allocator installed above"),
        };
        let ok = tests::run_all(&mut ctx);
        qemu_exit(if ok { ExitCode::Success } else { ExitCode::Failure })
    }

    #[cfg(not(feature = "tests"))]
    {
        // Built by xtask from hello-user/, linked at process::USER_CODE_VA.
        const HELLO_BINARY: &[u8] = include_bytes!(env!("HELLO_BIN"));

        let _ = writeln!(serial, "plinth: running hello ({} bytes)", HELLO_BINARY.len());
        match process::run(HELLO_BINARY, phys_offset) {
            Ok(process::Outcome::Exited(code)) => {
                let _ = writeln!(serial, "plinth: hello exited (code {code})");
            }
            Ok(process::Outcome::Faulted) => {
                let _ = writeln!(serial, "plinth: hello faulted");
            }
            Err(e) => {
                let _ = writeln!(serial, "plinth: failed to run hello: {e}");
                qemu_exit(ExitCode::Failure)
            }
        }

        let _ = writeln!(serial, "plinth: boot ok");
        qemu_exit(ExitCode::Success)
    }
}

#[derive(Clone, Copy)]
#[repr(u32)]
enum ExitCode {
    /// QEMU exits with status (0 << 1) | 1 = 1.
    Success = 0,
    /// QEMU exits with status (1 << 1) | 1 = 3. Used for panics and a
    /// failed test suite.
    Failure = 1,
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
    qemu_exit(ExitCode::Failure)
}
