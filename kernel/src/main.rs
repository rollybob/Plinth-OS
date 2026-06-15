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
// The ELF loader's parser is exercised by the test suite, but its mapping
// helpers are only reached from the userspace boot path; silence their
// dead-code noise in the test build.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod elf;
// The #PF path (self-paging upcall) is only exercised from userspace, which
// the test build never reaches; silence its dead-code noise there.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod fault;
mod frame_alloc;
mod gdt;
mod interrupts;
mod memory;
// process/usermode are driven from the normal boot path only; the test
// build stops before userspace, so silence their dead-code noise there.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod process;
// The scheduler's pure pick_next is exercised by the test suite; the rest of
// it (launch/switch/teardown) is only reached from the userspace boot path,
// so silence that dead-code noise in the test build.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod scheduler;
mod serial;
mod syscall;
// The timer's IRQ vector is installed in every build (interrupts::init),
// but it is armed and read only on the userspace boot path.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod timer;
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
        // Built by xtask from the *-user crates as static ET_EXEC ELFs,
        // embedded here and parsed by the kernel's ELF loader at run time.
        // The sequence is the demo: hello proves the syscall surface,
        // then the same workload runs under two different library OSes
        // with a deliberate crash between them -- the kernel logs the
        // fault, reclaims the process, and keeps going.
        // Binaries a process may launch with spawn, by id. id 0 = grantee.
        const SPAWNABLE: &[&[u8]] = &[include_bytes!(env!("GRANTEE_BIN"))];
        process::set_phys_offset(phys_offset);
        process::set_spawnable(SPAWNABLE);

        // Arm the periodic timer. It fires only once a process is in ring 3
        // (where interrupts are enabled); Stage 1 just counts the ticks, it
        // does not yet switch processes.
        timer::arm(100);
        let _ = writeln!(serial, "plinth: timer armed (100 Hz)");

        const DEMOS: &[(&str, &[u8])] = &[
            ("hello", include_bytes!(env!("HELLO_BIN"))),
            ("bump-demo", include_bytes!(env!("BUMP_BIN"))),
            ("crash-demo", include_bytes!(env!("CRASH_BIN"))),
            ("list-demo", include_bytes!(env!("LIST_BIN"))),
            ("greedy-demo", include_bytes!(env!("GREEDY_BIN"))),
            ("lazy-demo", include_bytes!(env!("LAZY_BIN"))),
            ("spawner-demo", include_bytes!(env!("SPAWNER_BIN"))),
        ];

        for (name, binary) in DEMOS {
            let _ = writeln!(serial, "plinth: running {name} ({} bytes)", process::image_size(binary));
            match process::run(binary, phys_offset) {
                Ok(process::Outcome::Exited(code)) => {
                    let _ = writeln!(serial, "plinth: {name} exited (code {code})");
                }
                Ok(process::Outcome::Faulted) => {
                    let _ = writeln!(serial, "plinth: {name} faulted");
                }
                Ok(process::Outcome::OutOfBudget) => {
                    let _ = writeln!(serial, "plinth: {name} out of budget");
                }
                Err(e) => {
                    let _ = writeln!(serial, "plinth: failed to run {name}: {e}");
                    qemu_exit(ExitCode::Failure)
                }
            }
            // Same number after every teardown (the one-time page-table
            // frames aside): no process leaks, not even the crashed one.
            if let Some(fa) = frame_alloc::FRAME_ALLOC.lock().as_ref() {
                let _ = writeln!(serial, "plinth: {} frames free", fa.free_frames());
            }
        }

        // Preemptive scheduler demo (Phase 2): launch independent CPU-bound
        // processes and round-robin them under the timer. Their lines
        // interleave in the log -- preemption made visible -- while each
        // process's own lines stay in program order. Frame counts bracket the
        // demo to show it leaks nothing once every process has exited.
        const SPIN_BIN: &[u8] = include_bytes!(env!("SPIN_BIN"));
        let before = frame_alloc::FRAME_ALLOC
            .lock()
            .as_ref()
            .map(|fa| fa.free_frames())
            .unwrap_or(0);
        let _ = writeln!(serial, "plinth: {before} frames free before scheduler");
        scheduler::run(&[SPIN_BIN, SPIN_BIN, SPIN_BIN], phys_offset);
        let after = frame_alloc::FRAME_ALLOC
            .lock()
            .as_ref()
            .map(|fa| fa.free_frames())
            .unwrap_or(0);
        let _ = writeln!(serial, "plinth: {after} frames free after scheduler");

        // The tick count is proof the timer fired during ring-3 execution.
        // It is nondeterministic under wall-clock timing (it varies with how
        // long the demos took) and deterministic only under -icount; nothing
        // asserts the number, only that "boot ok" was reached.
        let _ = writeln!(serial, "plinth: boot ok ({} ticks)", timer::ticks());
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
