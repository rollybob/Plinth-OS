//! Plinth kernel entry point.
//!
//! The kernel's job is deliberately small: own the hardware, multiplex it
//! securely, and push every policy decision up to unprivileged library
//! OSes. This file is the boot path; everything interesting happens above
//! the syscall boundary.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

// ACPI MADT discovery (Stage A1 of broader hardware) runs only on the userspace
// boot path; the test build stops before it, so silence dead-code noise there.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod acpi;
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
// The interrupt-controller seam (8259 PIC today). Its remap/unmask/eoi are
// driven only from the userspace boot path (the timer is armed there), so
// silence dead-code noise in the test build, like `timer`.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod irq;
// IPC endpoints are driven from the userspace boot path (and the no_mangle
// dispatcher); create_endpoint is unused in the test build, which stops
// before userspace.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod ipc;
// The event ring (its `EventRing` is unit-tested) plus the boot-path producer/
// consumer helpers, which are unused in the test build.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod input;
// The i8042 keyboard device (the first event source) runs only on the userspace
// boot path; its IRQ1 vector is installed in every build (interrupts::init).
#[cfg_attr(feature = "tests", allow(dead_code))]
mod keyboard;
mod memory;
// PCI discovery runs only on the userspace boot path (Stage 1 storage
// bring-up); the test build stops before it.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod pci;
// The virtio-blk driver (Stage 1 storage) runs only on the userspace boot
// path, after PCI discovery.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod virtio_blk;
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
        // Binaries a process may launch with spawn, by id. id 0 = grantee (a
        // worker that sends its result), id 1 = faultchild (a worker that
        // faults before sending, to exercise death-time reaping).
        const SPAWNABLE: &[&[u8]] = &[
            include_bytes!(concat!(env!("OUT_DIR"), "/grantee-user")),
            include_bytes!(concat!(env!("OUT_DIR"), "/faultchild-user")),
        ];
        process::set_phys_offset(phys_offset);
        process::set_spawnable(SPAWNABLE);

        // Discover the CPU + interrupt-controller topology from ACPI (Stage A1
        // of broader hardware): parse the MADT for the Local APIC base, the I/O
        // APIC(s), the CPU/AP APIC ids, and the ISA->GSI interrupt source
        // overrides. Pure discovery -- the 8259 PIC still drives interrupts
        // (irq::init below); the LAPIC + I/O APIC bring-up that consumes this map
        // is Stage A2. Reads firmware tables through the phys-offset window.
        acpi::init(&mut serial, boot_info.rsdp_addr.into_option(), phys_offset);

        // Initialise the interrupt controller (remap the PIC off the exception
        // vectors, mask every line); devices unmask their own line as they arm.
        irq::init();

        // Arm the periodic timer. It fires only once a process is in ring 3
        // (where interrupts are enabled); Stage 1 just counts the ticks, it
        // does not yet switch processes.
        timer::arm(100);
        let _ = writeln!(serial, "plinth: timer armed (100 Hz)");

        // Bring up the i8042 keyboard (the first input event source) and unmask
        // IRQ1. The synthetic selftest proves the scancode -> ring -> reader
        // wiring without a real keypress; live keys flow once a process blocks
        // on the source (Stage 2). Input is raw scancodes -- the keymap is libOS
        // policy, so nothing here turns a scancode into a character.
        keyboard::init();
        let _ = writeln!(serial, "plinth: keyboard ready (i8042, IRQ1)");
        keyboard::selftest(&mut serial);

        // Stage 1 storage bring-up: discover the virtio-blk device over legacy
        // PCI config space, then bring up the modern device (map its BAR,
        // negotiate features, stand up one virtqueue) and prove it with a
        // single polled read of sector 0 verified against the known image.
        // This runs before any process is created, so the BAR's kernel-half
        // MMIO mapping propagates to every process address space.
        let (infos, ndev) = pci::init(&mut serial);
        for i in 0..ndev {
            let info = infos[i].as_ref().expect("dense up to ndev");
            if let Err(e) = virtio_blk::init(&mut serial, info, phys_offset, i) {
                let _ = writeln!(serial, "plinth: virtio-blk[{i}] init failed: {e}");
            }
        }
        // Prove each device reads back. Device 0 is the ramp/test disk (the
        // 1 MiB byte-ramp image -- verify the ramp); device 1, if present, is
        // the boot archive (verify it reads and is a distinct disk, without the
        // kernel knowing the archive format -- that is the FS libOS's job).
        if virtio_blk::ready(0) {
            virtio_blk::selftest_read(&mut serial, phys_offset, 0, true);
        }
        if virtio_blk::ready(1) {
            virtio_blk::selftest_read(&mut serial, phys_offset, 1, false);
        }
        // The boot selftests above ran polled (no process to block yet). From
        // here on, runtime block_read blocks and is woken by the completion IRQ:
        // install each device's INTx handler and unmask its line (Stage 4).
        virtio_blk::enable_completion_irqs();

        // The synchronous, one-at-a-time demos (run via process::run). spawn
        // is no longer synchronous, so the spawner demo now runs under the
        // scheduler instead (see the spawn demo below).
        const DEMOS: &[(&str, &[u8])] = &[
            ("hello", include_bytes!(concat!(env!("OUT_DIR"), "/hello-user"))),
            ("bump-demo", include_bytes!(concat!(env!("OUT_DIR"), "/bump-user"))),
            ("crash-demo", include_bytes!(concat!(env!("OUT_DIR"), "/crash-user"))),
            ("list-demo", include_bytes!(concat!(env!("OUT_DIR"), "/list-user"))),
            ("greedy-demo", include_bytes!(concat!(env!("OUT_DIR"), "/greedy-user"))),
            ("lazy-demo", include_bytes!(concat!(env!("OUT_DIR"), "/lazy-user"))),
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

        use capability::{Capability, CapObject, RIGHT_RECV, RIGHT_SEND};

        // Free-frame count, for the no-leak-at-quiescence checks below.
        let free_frames = || {
            frame_alloc::FRAME_ALLOC
                .lock()
                .as_ref()
                .map(|fa| fa.free_frames())
                .unwrap_or(0)
        };

        // Free-endpoint count: the analogue of free_frames for the endpoint
        // table. Bracketing an IPC demo with this proves the endpoint slot is
        // reclaimed once every referencing capability is gone (Stage B), the
        // way the frame baseline proves frames are.
        let free_endpoints = ipc::free_endpoint_count;

        // Preemptive scheduler demo (Phase 2): launch independent CPU-bound
        // processes and round-robin them under the timer. Their lines
        // interleave in the log -- preemption made visible -- while each
        // process's own lines stay in program order. Frame counts bracket the
        // demo to show it leaks nothing once every process has exited.
        const SPIN_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/spin-user"));
        let before = free_frames();
        let _ = writeln!(serial, "plinth: {before} frames free before scheduler");
        scheduler::run("scheduler demo", &[SPIN_BIN, SPIN_BIN, SPIN_BIN], phys_offset, &[None, None, None]);
        let after = free_frames();
        let _ = writeln!(serial, "plinth: {after} frames free after scheduler");

        // IPC demo (Phase 2): a pinger and a ponger rendezvous over one
        // synchronous endpoint the kernel creates and grants to both. Their
        // ping/pong lines interleave; each process's own stay in program order.
        const PINGPONG_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/pingpong-user"));
        let before_ipc = free_frames();
        let _ = writeln!(serial, "plinth: {before_ipc} frames free before ipc");
        let _ = writeln!(serial, "plinth: {} endpoints free before ipc", free_endpoints());
        match ipc::create_endpoint() {
            Some(ep) => {
                let cap = Capability {
                    object: CapObject::Endpoint { id: ep },
                    rights: RIGHT_SEND | RIGHT_RECV,
                };
                scheduler::run(
                    "ipc demo",
                    &[PINGPONG_BIN, PINGPONG_BIN],
                    phys_offset,
                    &[Some(cap), Some(cap)],
                );
            }
            None => {
                let _ = writeln!(serial, "plinth: ipc demo: no endpoint available");
            }
        }
        let after_ipc = free_frames();
        let _ = writeln!(serial, "plinth: {after_ipc} frames free after ipc");
        let _ = writeln!(serial, "plinth: {} endpoints free after ipc", free_endpoints());

        // Capability-transfer / zero-copy demo: a producer fills a frame and
        // hands its capability to a consumer over IPC; the consumer maps the
        // same physical frame and reads the data. Ownership moves -- the
        // producer is unmapped -- so the frame is reclaimed exactly once.
        const SHARE_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/share-user"));
        let before_share = free_frames();
        let _ = writeln!(serial, "plinth: {before_share} frames free before share");
        let _ = writeln!(serial, "plinth: {} endpoints free before share", free_endpoints());
        match ipc::create_endpoint() {
            Some(ep) => {
                let cap = Capability {
                    object: CapObject::Endpoint { id: ep },
                    rights: RIGHT_SEND | RIGHT_RECV,
                };
                scheduler::run(
                    "share demo",
                    &[SHARE_BIN, SHARE_BIN],
                    phys_offset,
                    &[Some(cap), Some(cap)],
                );
            }
            None => {
                let _ = writeln!(serial, "plinth: share demo: no endpoint available");
            }
        }
        let after_share = free_frames();
        let _ = writeln!(serial, "plinth: {after_share} frames free after share");
        let _ = writeln!(serial, "plinth: {} endpoints free after share", free_endpoints());

        // RPC demo: a server and a client over one endpoint, with directional
        // rights -- the server holds RIGHT_RECV only, the client RIGHT_SEND
        // only. The client `call`s; the server `recv`s the request with a
        // one-shot reply capability and answers it (no send right needed -- the
        // reply cap is the authority).
        const RPC_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rpc-user"));
        let before_rpc = free_frames();
        let _ = writeln!(serial, "plinth: {before_rpc} frames free before rpc");
        let _ = writeln!(serial, "plinth: {} endpoints free before rpc", free_endpoints());
        match ipc::create_endpoint() {
            Some(ep) => {
                let recv_cap = Capability {
                    object: CapObject::Endpoint { id: ep },
                    rights: RIGHT_RECV,
                };
                let send_cap = Capability {
                    object: CapObject::Endpoint { id: ep },
                    rights: RIGHT_SEND,
                };
                scheduler::run(
                    "rpc demo",
                    &[RPC_BIN, RPC_BIN],
                    phys_offset,
                    &[Some(recv_cap), Some(send_cap)],
                );
            }
            None => {
                let _ = writeln!(serial, "plinth: rpc demo: no endpoint available");
            }
        }
        let after_rpc = free_frames();
        let _ = writeln!(serial, "plinth: {after_rpc} frames free after rpc");
        let _ = writeln!(serial, "plinth: {} endpoints free after rpc", free_endpoints());

        // Spawn + wait demo: the kernel launches a single parent process; the
        // parent `spawn`s a worker (an independent scheduled process), the
        // worker sends its result back over the channel spawn set up, and the
        // parent collects it with recv -- the join. This is spawn reconciled
        // with the scheduler: no synchronous nesting.
        const SPAWNER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/spawner-user"));
        let before_spawn = free_frames();
        let _ = writeln!(serial, "plinth: {before_spawn} frames free before spawn");
        let _ = writeln!(serial, "plinth: {} endpoints free before spawn", free_endpoints());
        scheduler::run("spawn demo", &[SPAWNER_BIN], phys_offset, &[None]);
        let after_spawn = free_frames();
        let _ = writeln!(serial, "plinth: {after_spawn} frames free after spawn");
        let _ = writeln!(serial, "plinth: {} endpoints free after spawn", free_endpoints());

        // Block-storage demo (Stage 2): the exokernel multiplexing surface. The
        // kernel grants a process a BlockRange capability naming a sub-range of
        // the disk; the process reads a sector through it (verifying the bytes
        // against the known image) and is denied a read one sector past its
        // range -- the multiplexing guarantee. Runs only if the device came up.
        // Frame counts bracket the demo: the process frame_allocs its I/O frame
        // and teardown reclaims it, so the count returns to baseline.
        if virtio_blk::ready(0) {
            const BLK_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/blk-user"));
            let before_blk = free_frames();
            let _ = writeln!(serial, "plinth: {before_blk} frames free before blk");
            // Grant device 0 (the ramp disk) sectors [1, 5): start=1 so an
            // offset-0 read is sector 1 (distinguishable from sector 0), count=4
            // so offset 4 is just past the grant -- the out-of-range probe the
            // demo makes.
            let range = Capability {
                object: CapObject::BlockRange { dev: 0, start: 1, count: 4 },
                rights: capability::RIGHT_READ,
            };
            scheduler::run("blk demo", &[BLK_BIN], phys_offset, &[Some(range)]);
            let after_blk = free_frames();
            let _ = writeln!(serial, "plinth: {after_blk} frames free after blk");
        }

        // Phase 2 storage, load-from-disk: the filesystem library-OS demo. The
        // kernel grants fsdemo one capability -- a BlockRange over the whole
        // archive device (device 1) -- and nothing else. fsdemo uses libfs to
        // parse the on-disk archive, find a program by name, read its ELF off
        // the disk, and launch it with spawn_from_buffer. The loaded program
        // (diskhello) is NOT embedded in the kernel; it lives only in the
        // archive, so its "running from disk" line proves the path end to end.
        // The kernel never parses the archive format -- it only multiplexes the
        // disk (the BlockRange) and validates the ELF libfs hands back. Frame
        // counts bracket the demo: fsdemo's scratch/image frames and
        // diskhello's image frames are all reclaimed at teardown.
        if let Some(cap) = virtio_blk::capacity(1) {
            const FSDEMO_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fsdemo-user"));
            let before_fs = free_frames();
            let _ = writeln!(serial, "plinth: {before_fs} frames free before fs");
            // The whole archive device, from sector 0 -- so a range-relative
            // sector is an archive sector (what the directory records).
            // Read-only: the boot archive is never written.
            let range = Capability {
                object: CapObject::BlockRange { dev: 1, start: 0, count: cap },
                rights: capability::RIGHT_READ,
            };
            scheduler::run("fs demo", &[FSDEMO_BIN], phys_offset, &[Some(range)]);
            let after_fs = free_frames();
            let _ = writeln!(serial, "plinth: {after_fs} frames free after fs");
        }

        // Phase 2 console input (Stage 2): event delivery through an EventSource
        // capability. The kernel grants evt-user a read capability on input
        // source 0 (the keyboard) and nothing else; the process reads one event
        // through it (a raw scancode -- characters are libOS policy) and is
        // rejected when it reads through a non-source capability. Its read finds
        // the ring empty and blocks, so the kernel idles waiting for input; a
        // synthetic scancode is delivered to wake it deterministically (a real
        // keypress would otherwise). Frame counts bracket the demo (no leak).
        {
            const EVT_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/evt-user"));
            let before_evt = free_frames();
            let _ = writeln!(serial, "plinth: {before_evt} frames free before evt");
            let source = Capability {
                object: CapObject::EventSource { id: 0 },
                rights: capability::RIGHT_READ,
            };
            // 'A' make code (0x1E), delivered the moment the reader blocks.
            input::arm_synthetic(&[0x1E]);
            scheduler::run("evt demo", &[EVT_BIN], phys_offset, &[Some(source)]);
            let after_evt = free_frames();
            let _ = writeln!(serial, "plinth: {after_evt} frames free after evt");
        }

        // Phase 2 console input (Stage 3): a line read through a library OS. The
        // kernel grants kbd-user the same keyboard EventSource; the process uses
        // libinput (an unprivileged keymap + line reader) to turn raw scancodes
        // into a line and echo it -- so "input is output-only" is retired, with
        // the keymap as libOS policy and the kernel still shipping only raw
        // events. The scripted scancodes spell "Hi" + Enter (shift down, h, shift
        // up, i, enter), delivered one per block as the reader idles on input.
        {
            const KBD_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/kbd-user"));
            let before_kbd = free_frames();
            let _ = writeln!(serial, "plinth: {before_kbd} frames free before kbd");
            let source = Capability {
                object: CapObject::EventSource { id: 0 },
                rights: capability::RIGHT_READ,
            };
            // Set-1 scancodes for "Hi\n": LShift make, h, LShift break, i, Enter.
            input::arm_synthetic(&[0x2A, 0x23, 0xAA, 0x17, 0x1C]);
            scheduler::run("kbd demo", &[KBD_BIN], phys_offset, &[Some(source)]);
            let after_kbd = free_frames();
            let _ = writeln!(serial, "plinth: {after_kbd} frames free after kbd");
        }

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
