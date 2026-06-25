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
// The single big kernel lock (Stage B2, D4) is acquired/released only from
// the userspace-driving dispatch bodies; the test build stops before any
// of those run.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod bkl;
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
// The PS/2 mouse device (the second event source, Design/mouse_input.md) runs
// only on the userspace boot path; its IRQ12 vector is installed in every
// build (interrupts::init). Its packet decode (`Packet`/`decode_axis`) is
// pure logic exercised by the test suite, like the keyboard's `Event` coding.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod mouse;
// Per-CPU data (Stage B2, D6) is set up and read only from the userspace
// boot path (BSP) and AP bring-up; the test build never reaches either.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod percpu;
// PCI discovery runs only on the userspace boot path (Stage 1 storage
// bring-up); the test build stops before it.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod pci;
// The virtio-blk driver (Stage 1 storage) runs only on the userspace boot
// path, after PCI discovery.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod virtio_blk;
// Async completion rings are reached only from the userspace syscall path; the
// test build stops before userspace, so silence the dead-code noise there.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod rings;
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
// AP bring-up (broader hardware, Stage B1) runs only on the userspace boot
// path, after the LAPIC is up; the test build never reaches it. frame_alloc
// reads its TRAMPOLINE_PHYS constant regardless of build (the reservation
// must hold even in a build that never starts an AP), so this is the one
// dead-code-allowed module the test build still partially depends on.
#[cfg_attr(feature = "tests", allow(dead_code))]
mod smp;
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

    let selectors = gdt::init(percpu::BSP_CORE_ID);
    let _ = writeln!(serial, "plinth: GDT + TSS loaded");

    interrupts::init();
    let _ = writeln!(serial, "plinth: IDT loaded");

    // Per-CPU data (Stage B2.2, D6): point this core's GS_BASE at its own
    // slot before arming syscall_entry, which is gs:-relative.
    percpu::init(percpu::BSP_CORE_ID, syscall::stack_top(percpu::BSP_CORE_ID));
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
        // faults before sending, to exercise death-time reaping), id 2 =
        // stealwork (a CPU-bound worker the work-stealing S4 demo spawns en
        // masse onto one core).
        const SPAWNABLE: &[&[u8]] = &[
            include_bytes!(concat!(env!("OUT_DIR"), "/grantee-user")),
            include_bytes!(concat!(env!("OUT_DIR"), "/faultchild-user")),
            include_bytes!(concat!(env!("OUT_DIR"), "/stealwork-user")),
        ];
        process::set_phys_offset(phys_offset);
        process::set_spawnable(SPAWNABLE);

        // Discover the CPU + interrupt-controller topology from ACPI (broader
        // hardware, Stage A1): parse the MADT for the Local APIC base, the I/O
        // APIC(s), the CPU/AP APIC ids, and the ISA->GSI interrupt source
        // overrides. Reads firmware tables through the phys-offset window; returns
        // the topology the interrupt controller consumes below.
        let topology = acpi::init(&mut serial, boot_info.rsdp_addr.into_option(), phys_offset);

        // Initialise the interrupt controller (broader hardware, Stage A2). With
        // an ACPI topology this brings up the LAPIC + I/O APIC and retires the
        // 8259 PIC, routing each line through the I/O APIC (incl. the IRQ0->GSI2
        // PIT remap); without one it falls back to the PIC. Either way the PIC is
        // remapped off the exception vectors and masked, and devices unmask their
        // own line as they arm. The seam is invisible above this call -- the boot
        // trace is unchanged whether the PIC or the APIC delivers.
        irq::init(topology.as_ref());

        // Wake every other CPU the MADT reported (broader hardware, Stage
        // B1). Needs the LAPIC up (just above) to send IPIs. Phase 1: each AP
        // proves it can be woken at all and halts in real mode -- it does not
        // yet touch any structure the BSP's demos below use, so nothing here
        // changes the single-CPU concurrency story (that is Stage B2).
        smp::start_aps(&mut serial, topology.as_ref(), phys_offset);

        // Arm the periodic timer. It fires only once a process is in ring 3
        // (where interrupts are enabled); Stage 1 just counts the ticks, it
        // does not yet switch processes.
        timer::arm(100);
        let _ = writeln!(serial, "plinth: timer armed (100 Hz)");

        // Bring up the i8042 keyboard (the first input event source) and unmask
        // IRQ1. Scancodes flow through `input::record`, which routes them to any
        // ring subscribed to the source (event_rings.md); the evt/kbd demos
        // below prove that producer -> subscription -> reader path end to end.
        // Input is raw scancodes -- the keymap is libOS policy, so nothing here
        // turns a scancode into a character.
        keyboard::init();
        let _ = writeln!(serial, "plinth: keyboard ready (i8042, IRQ1)");

        // Bring up the i8042's second port (the mouse, source 1,
        // Design/mouse_input.md) and unmask IRQ12, if a device answers. A
        // missing mouse is logged, not a boot fault (S4) -- the rest of boot
        // proceeds either way, and the mouse demo below grants the
        // EventSource only if `mouse::present()`.
        mouse::init();
        if mouse::present() {
            let _ = writeln!(serial, "plinth: mouse ready (i8042 port 2, IRQ12)");
        } else {
            let _ = writeln!(serial, "plinth: no mouse detected (i8042 port 2)");
        }

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

        // Work-stealing demo (SMP scaling S4, Design/smp_scaling.md section 6).
        // The parent spawns three CPU-bound workers back to back; spawn homes
        // every child to the spawning core, so all four processes pile onto one
        // core's run queue while the other cores sit idle. The two facts this
        // proves: (1) every worker still completes -- the parent joins all
        // three (recv = wait), and each prints its own "stealwork[id] done";
        // (2) at least one process actually moved to a different core's array --
        // the scheduler's steal counter, read as a before/after delta around
        // the run, is the one signal only stealing produces. Under -smp 1 there
        // is no other core, so the delta is 0 and the demo still completes on
        // the single core (the smoke check requires a steal only when >= 2
        // cores are online). Bracketed by the frame/endpoint baselines like the
        // other scheduler demos -- the per-spawn result channels leak nothing.
        const STEALER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/stealer-user"));
        let online_cores =
            (0..percpu::MAX_CORES).filter(|&c| irq::is_core_online(c)).count();
        let before_steal = free_frames();
        let _ = writeln!(serial, "plinth: {before_steal} frames free before steal");
        let _ = writeln!(serial, "plinth: {} endpoints free before steal", free_endpoints());
        let steals_before = scheduler::steals();
        scheduler::run("steal demo", &[STEALER_BIN], phys_offset, &[None]);
        let steals_done = scheduler::steals() - steals_before;
        let _ = writeln!(
            serial,
            "plinth: steal demo: {steals_done} steals across {online_cores} cores"
        );
        let after_steal = free_frames();
        let _ = writeln!(serial, "plinth: {after_steal} frames free after steal");
        let _ = writeln!(serial, "plinth: {} endpoints free after steal", free_endpoints());

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

        // Async block demo (Stage 3): depth made observable. The kernel grants a
        // BlockRange over device 0 sectors [0, 4); asyncblk issues four reads
        // that overlap on the device through the libos reference executor (a
        // futures executor over the completion rings) and asserts each landed in
        // its own frame -- the many-in-flight path the single-shot block_read
        // could not express. Frame counts bracket the demo: its four I/O frames
        // and two ring frames are all reclaimed at teardown.
        if virtio_blk::ready(0) {
            const ASYNCBLK_BIN: &[u8] =
                include_bytes!(concat!(env!("OUT_DIR"), "/asyncblk-user"));
            let before_async = free_frames();
            let _ = writeln!(serial, "plinth: {before_async} frames free before asyncblk");
            // Sectors [0, 4): the demo reads relative sectors 0..3, one per
            // overlapping request. Read-only, like the other block grants.
            let range = Capability {
                object: CapObject::BlockRange { dev: 0, start: 0, count: 4 },
                rights: capability::RIGHT_READ,
            };
            scheduler::run("asyncblk demo", &[ASYNCBLK_BIN], phys_offset, &[Some(range)]);
            let after_async = free_frames();
            let _ = writeln!(serial, "plinth: {after_async} frames free after asyncblk");
        }

        // Block write path (Design/block_write.md): the write half of the ring
        // ABI. The kernel grants blkwrite-user a BlockRange over device 0
        // sectors [8, 12) -- clear of every other demo's range on this device --
        // minted with RIGHT_READ | RIGHT_WRITE, since the demo round-trips
        // through the SAME range (write a pattern, then read the same sectors
        // back to verify it landed): post_write's RIGHT_WRITE check and
        // post_read's RIGHT_READ check (rings.rs) both gate this one cap. The
        // process writes a fixed pattern, reads the range back into a separate
        // frame, and asserts the bytes match what it wrote (not the disk's
        // original ramp content) -- proving the write actually reached the
        // device. Frame counts bracket the demo (no leak).
        //
        // A second BlockRange (slot 2) over sectors [12, 16) -- also clear of
        // every other demo's range -- is minted RIGHT_READ only. The demo
        // attempts a write through it and asserts the kernel rejects with
        // BLK_E_RIGHTS: the negative-case mirror of blk-user's out-of-range
        // probe, closing the gap block_write.md's follow-up note flagged (no
        // demo asserted a write through a RIGHT_READ-only grant is rejected).
        if virtio_blk::ready(0) {
            const BLKWRITE_BIN: &[u8] =
                include_bytes!(concat!(env!("OUT_DIR"), "/blkwrite-user"));
            let before_blkwrite = free_frames();
            let _ = writeln!(serial, "plinth: {before_blkwrite} frames free before blkwrite");
            let range = Capability {
                object: CapObject::BlockRange { dev: 0, start: 8, count: 4 },
                rights: capability::RIGHT_READ | capability::RIGHT_WRITE,
            };
            let rdonly_range = Capability {
                object: CapObject::BlockRange { dev: 0, start: 12, count: 4 },
                rights: capability::RIGHT_READ,
            };
            scheduler::run(
                "blkwrite demo",
                &[BLKWRITE_BIN],
                phys_offset,
                &[Some(range), Some(rdonly_range)],
            );
            let after_blkwrite = free_frames();
            let _ = writeln!(serial, "plinth: {after_blkwrite} frames free after blkwrite");
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

        // Phase 2 console input (Stage 3): a multishot event STREAM. The kernel
        // grants evtstream-user the same keyboard EventSource; the process opens
        // one multishot subscription through the libos async executor and reaps a
        // SEQUENCE of events from it (one RING_OP_EVENT_SUB, many completions
        // demuxed by the subscription cookie), asserting each scancode arrives
        // once and in order -- the many-event path the single-shot event_recv
        // shim cannot express. A scripted scancode sequence is injected to drive
        // it deterministically (a real keyboard would otherwise). Frame counts
        // bracket the demo (no leak: its ring frames and the subscription slot are
        // all reclaimed at teardown).
        {
            const EVTSTREAM_BIN: &[u8] =
                include_bytes!(concat!(env!("OUT_DIR"), "/evtstream-user"));
            let before_es = free_frames();
            let _ = writeln!(serial, "plinth: {before_es} frames free before evtstream");
            let source = Capability {
                object: CapObject::EventSource { id: 0 },
                rights: capability::RIGHT_READ,
            };
            // Set-1 make codes for 'a','b','c','d' -- must match evtstream-user's
            // SEQUENCE. Delivered one per block as the reader idles on input.
            input::arm_synthetic(&[0x1E, 0x30, 0x2E, 0x20]);
            scheduler::run("evtstream demo", &[EVTSTREAM_BIN], phys_offset, &[Some(source)]);
            let after_es = free_frames();
            let _ = writeln!(serial, "plinth: {after_es} frames free after evtstream");
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

        // Phase 2 console input (Stage 4): the unified loop -- ONE ring multiplexes
        // block I/O and input. The kernel grants unified-user two capabilities, a
        // BlockRange over device 0 (slot 1) and the keyboard EventSource (slot 2);
        // the process registers a single ring, issues a block read AND a multishot
        // keyboard subscription on it, and drives both to completion in one
        // block_on/ring_wait loop -- the end-goal event-loop shape a real OS is
        // built on. The disk completion arrives via the virtio MSI-X IRQ; the
        // scripted scancodes ('x','y','z' make codes) drive the input side, one per
        // block as the reader idles on input (the same idle the kbd demo uses, but
        // now with a disk read in flight on the same ring). Frame counts bracket
        // the demo (no leak: the ring frames, the I/O frame, and the subscription
        // slot are all reclaimed at teardown).
        if virtio_blk::ready(0) {
            const UNIFIED_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/unified-user"));
            let before_uni = free_frames();
            let _ = writeln!(serial, "plinth: {before_uni} frames free before unified");
            // Grants mint in order after the CPU budget: BlockRange -> slot 1,
            // EventSource -> slot 2 (the demo's BLOCK_SLOT and EVENT_SLOT).
            let range = Capability {
                object: CapObject::BlockRange { dev: 0, start: 0, count: 1 },
                rights: capability::RIGHT_READ,
            };
            let source = Capability {
                object: CapObject::EventSource { id: 0 },
                rights: capability::RIGHT_READ,
            };
            // Set-1 make codes for 'x','y','z' -- must match unified-user's SEQUENCE.
            input::arm_synthetic(&[0x2D, 0x15, 0x2C]);
            scheduler::run("unified demo", &[UNIFIED_BIN], phys_offset, &[Some(range), Some(source)]);
            let after_uni = free_frames();
            let _ = writeln!(serial, "plinth: {after_uni} frames free after unified");
        }

        // Mouse input (Design/mouse_input.md S2): a second EventSource. The
        // kernel grants mouse-user a read capability on input source 1 (the
        // mouse) and nothing else; the process opens a multishot subscription
        // (the same libos stream adapter the keyboard's evtstream-user uses)
        // and reaps a scripted packet SEQUENCE, asserting each packet's
        // dx/dy/buttons decode correctly and in order, then cancels. Only run
        // if the i8042's second port answered at bring-up (mouse::present()):
        // a missing mouse is not a boot fault, but there is then no source 1
        // to subscribe to. Frame counts bracket the demo (no leak).
        if mouse::present() {
            const MOUSE_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mouse-user"));
            let before_mouse = free_frames();
            let _ = writeln!(serial, "plinth: {before_mouse} frames free before mouse");
            let source = Capability {
                object: CapObject::EventSource { id: input::SOURCE_MOUSE as u8 },
                rights: capability::RIGHT_READ,
            };
            // Must match mouse-user's SEQUENCE: (dx, dy, buttons) per packet.
            input::arm_synthetic_mouse(&[(10, -5, 0x00), (-20, 15, 0x01), (3, 3, 0x02)]);
            scheduler::run("mouse demo", &[MOUSE_BIN], phys_offset, &[Some(source)]);
            let after_mouse = free_frames();
            let _ = writeln!(serial, "plinth: {after_mouse} frames free after mouse");
        }

        // Read-write filesystem (Design/readwrite_fs.md S6): the librwfs
        // library OS over the block write path. The kernel grants rwfs-user a
        // BlockRange over device 0 sectors [32, 96) -- clear of every other
        // demo's range on this device -- minted RIGHT_READ | RIGHT_WRITE (a
        // round-tripping cap needs both, same lesson blkwrite-user's build
        // caught). The process formats the range fresh every run (S5),
        // creates two files, reads each back, deletes one, creates a third
        // sized to need exactly the freed run, and asserts it landed at the
        // exact sector the deleted file held -- proving the bitmap allocator
        // actually reclaims freed space rather than just hiding it -- then
        // re-verifies the surviving file is untouched. Frame counts bracket
        // the demo (no leak).
        if virtio_blk::ready(0) {
            const RWFS_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rwfs-user"));
            let before_rwfs = free_frames();
            let _ = writeln!(serial, "plinth: {before_rwfs} frames free before rwfs");
            let range = Capability {
                object: CapObject::BlockRange { dev: 0, start: 32, count: 64 },
                rights: capability::RIGHT_READ | capability::RIGHT_WRITE,
            };
            scheduler::run("rwfs demo", &[RWFS_BIN], phys_offset, &[Some(range)]);
            let after_rwfs = free_frames();
            let _ = writeln!(serial, "plinth: {after_rwfs} frames free after rwfs");
        }

        // BKL contention micro-benchmark (broader-hardware "SMP -- scaling"
        // decision: is splitting the lock, roadmap item B3, even justified?).
        // Saturate every core with the cheapest kernel-entry hammer there is
        // (bench-user: a tight cpu_charge(0) loop) and measure how often the
        // single big kernel lock is actually contended. Reset the counters
        // first so the report reflects only the hammer, not the demo traffic
        // above. Compiled in only by the `bench` feature (`cargo xtask bench`);
        // the production kernel never builds or runs this.
        #[cfg(feature = "bench")]
        {
            const BENCH_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bench-user"));
            // The scheduler table holds MAX_PROCESSES (4); run exactly that, so
            // -smp 4 puts one hammer on every core (maximal contention) and
            // -smp 2/3 oversubscribes.
            const BENCH_PROCS: usize = scheduler::MAX_PROCESSES;
            bkl::bench_reset();
            let _ = writeln!(serial, "plinth: bkl bench: {BENCH_PROCS} procs x cpu_charge(0) hammer");
            let bins: [&[u8]; BENCH_PROCS] = [BENCH_BIN; BENCH_PROCS];
            let caps: [Option<Capability>; BENCH_PROCS] = [None; BENCH_PROCS];
            scheduler::run("bkl bench", &bins, phys_offset, &caps);
            bkl::bench_report(&mut serial);
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
