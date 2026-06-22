# Plinth

A toy exokernel in Rust -- the exokernel idea reduced to the smallest
codebase that can demonstrate it. The whole kernel still reads in a sitting.

A plinth is the bare slab a column stands on: it carries the load and
imposes nothing about what is built above it. That is the contract here.
The kernel owns physical memory, CPU time, and a handful of devices and
multiplexes them securely through capabilities, but refuses to define what
memory management, scheduling, a filesystem, or a keymap *is*. Applications
on the same kernel answer those questions differently, in unprivileged code,
and the boot log shows the difference.

It began as one boot proving a single contrast -- the same workload over two
allocators -- and has grown a preemptive scheduler, IPC, a disk, a keyboard,
and symmetric multiprocessing, without giving up the property that makes it
worth reading: a kernel that is mechanism, with policy in unprivileged library
OSes, whose single-core boot is still checked line-by-line in CI.

## The demo

This is one single-core boot, verified line-by-line in CI against
[expected_boot_log.txt](expected_boot_log.txt) -- the same assertion battery
reruns on 2-4 cores (see [Testing](#testing)). It runs in two acts: the core
exokernel moves, then the system machinery -- a scheduler, IPC, storage, and
input -- that turns the kernel into something you could build on. The machinery
was added without giving up the determinism that makes the single-core boot
checkable.

```text
plinth: kernel entry
plinth: frame allocator ready
plinth: GDT + TSS loaded
plinth: IDT loaded
plinth: syscall interface ready
plinth: acpi: 1 cpu(s), 1 ioapic(s)
plinth: timer armed
plinth: keyboard ready (i8042, IRQ1)
plinth: keyboard selftest ok
plinth: scanning PCI bus
plinth: virtio-blk found at 00:03.0
plinth: virtio-blk found at 00:04.0
plinth: virtio-blk[0] ready (queue 0, size 64, capacity 2048 sectors)
plinth: virtio-blk[0] msix vector 0x30
plinth: virtio-blk[1] msix vector 0x31
plinth: virtio-blk[0] sector 0 read ok (ramp verified)
plinth: virtio-blk[1] sector 0 read ok (distinct disk)
plinth: running hello
hello: ring 3
hello: frame mapped and writable
hello: done
plinth: hello exited (code 0)
plinth: running bump-demo
demo: policy = bump
demo: a = 0x10000000
demo: b = 0x10000600
demo: freed a
demo: c = 0x10000c00
demo: c got a new address
demo: kernel frames used: 2
plinth: bump-demo exited (code 0)
plinth: running crash-demo
crash: about to dereference null
plinth: [user fault] #PF
plinth: terminating user process
plinth: crash-demo faulted
plinth: running list-demo
demo: policy = freelist
demo: a = 0x10000000
demo: b = 0x10000600
demo: freed a
demo: c = 0x10000000
demo: c reused a freed block
demo: kernel frames used: 1
plinth: list-demo exited (code 0)
plinth: running greedy-demo
greedy: spending CPU budget
greedy: charged 256, remaining = 768
greedy: charged 256, remaining = 512
greedy: charged 256, remaining = 256
greedy: charged 256, remaining = 0
plinth: [out of budget] terminating user process
plinth: greedy-demo out of budget
plinth: running lazy-demo
lazy: registering fault handler
lazy: serviced fault at 0x18000000
lazy: serviced fault at 0x18001000
lazy: serviced fault at 0x18002000
lazy: serviced fault at 0x18003000
lazy: all pages materialized on demand
plinth: lazy-demo exited (code 0)
plinth: scheduler demo: 3 processes
plinth: scheduler demo: all done
plinth: ipc demo: 2 processes
plinth: ipc demo: all done
plinth: share demo: 2 processes
plinth: share demo: all done
plinth: rpc demo: 2 processes
plinth: rpc demo: all done
plinth: spawn demo: 1 processes
plinth: spawn demo: all done
plinth: blk demo: 1 processes
blk: read ok b0=1 b1=2 b5=6
blk: out-of-range rejected
plinth: blk demo: all done
plinth: fs demo: 1 processes
fsdemo: loading 'diskhello' from the boot archive
diskhello: running from disk
fsdemo: diskhello returned 777
plinth: fs demo: all done
plinth: evt demo: 1 processes
evt: non-source rejected
evt: got scancode 0x1e
plinth: evt demo: all done
plinth: kbd demo: 1 processes
kbd: type a line
kbd: read 'Hi'
plinth: kbd demo: all done
plinth: boot ok
```

The single-process demos print verbatim, in order. The Phase 2 demos that run
several processes under the scheduler print only their bracketing lines here,
because the interleaving between processes is deliberately nondeterministic;
CI checks each process's *own* lines stay in program order
(`check_per_process_order`), and that the free-frame count returns to its
baseline around every demo (`check_frames_baseline`) -- the no-leak invariant,
asserted across the whole boot rather than printed.

### Act one -- the core exokernel moves

**Same app, different OS.** bump-demo and list-demo are the *identical*
workload (`demo-app/`) -- allocate three 1536-byte blocks, free the first,
allocate again -- linked against two different library OSes (`libos/`). The
bump policy never reuses memory: the third allocation lands at a new address
and costs a second kernel frame. The free-list policy recycles: the third
allocation comes back at the freed block's address on a single kernel frame.
Same kernel, same syscalls, different memory management -- because memory
management is application code here, not kernel code.

**A crash is an event, not a catastrophe.** crash-demo dereferences null. The
kernel logs the ring-3 page fault, terminates the process, reclaims it
without a leak, and the boot continues.

**CPU time is a capability too.** greedy-demo is minted a fixed CPU budget at
spawn and spends it with `cpu_charge`, watching the balance fall to zero.
When it charges past zero it has tried to consume a resource it no longer
holds, so the kernel terminates it exactly as it did the crash. The kernel
enforces the bound; *how* to spend the budget is the library OS's call.

**Userspace handles its own page faults.** lazy-demo registers a ring-3 fault
handler and then touches unmapped pages. Each first touch faults -- the *same*
`#PF` that kills crash-demo -- but here the kernel hands the fault back to the
process, which maps a frame with the ordinary `frame_alloc`/`frame_map`
syscalls and returns; the faulting instruction is retried and succeeds.
Demand paging where the *application*, not the kernel, decides what backs an
address: the opposite outcome to crash-demo from the identical hardware event,
chosen entirely in unprivileged code.

### Act two -- the system machinery

**The CPU is multiplexed, preemptively.** scheduler-demo launches three
independent CPU-bound processes; a 100 Hz timer preempts them and the kernel
round-robins between them, so their lines interleave in the log. The kernel
saves the full interrupted context, switches address space and per-process
kernel stack, and resumes the next process. The single-process determinism of
act one is gone here on purpose -- and the testing strategy changed with it,
from an exact trace to per-process ordering plus no-leak invariants.

**Capabilities are transferable authority, across isolation boundaries.** The
IPC demos rendezvous over synchronous endpoints: ping/pong exchange messages
(ipc-demo); a producer fills a frame and hands its *capability* to a consumer,
which maps the same physical frame and reads the data back, with no copy
(share-demo); a client and server do request/response RPC over a one-shot
reply capability, the server needing no send right of its own (rpc-demo). A
capability is not a local handle -- it is authority that moves between mutually
isolated address spaces, which is what makes it more than an access-control
entry. `spawn` is reconciled with all this (spawn-demo): it launches a child
as an independent scheduled process and returns a handle the parent `recv`s --
the join -- rather than nesting synchronously.

**The disk is multiplexed like memory.** blk-demo is granted a `BlockRange`
capability naming a run of disk sectors; it reads a sector through it and is
*denied* a read one sector past the range -- the same secure-binding guarantee
frames get, now over storage. The read is interrupt-driven and blocking: the
process suspends, the CPU runs other work while the DMA is in flight, and the
disk's completion interrupt wakes it. fs-demo goes further: an unprivileged
filesystem library OS (`libfs`) parses a read-only boot archive, finds
`diskhello` by name, reads its ELF off the disk, and launches it with
`spawn_from_buffer`. diskhello exists *only* on the disk -- it is not embedded
in the kernel -- so its "running from disk" line proves the load path end to
end.

**Input is raw events; the keymap is policy.** evt-demo is granted an
`EventSource` capability on the keyboard, is denied reading through a
non-source capability, and blocks on `event_recv` until a keystroke wakes it
(the kernel idles on input rather than treating the block as a deadlock). The
kernel ships raw Set-1 scancodes and nothing more. kbd-demo turns those
scancodes into a line with `libinput`, an unprivileged library OS holding the
keymap, shift handling, and line editor -- so the echoed `Hi` is policy the
kernel never sees.

## Why exokernels

A conventional kernel bundles mechanism (multiplexing hardware safely) with
policy (what a process, file, or heap is). The exokernel argument -- Engler,
Kaashoek, and O'Toole, SOSP '95 -- is that the bundle is the problem: the
kernel should securely expose raw resources, and every abstraction should
live in unprivileged *library OSes* that applications choose, replace, or
rewrite.

Plinth implements the minimum machinery that makes the argument concrete:

- **Secure bindings**: physical frames, disk sectors, and input devices are
  all granted as capabilities -- kernel-held records of (resource, rights),
  referred to by slot index. Userspace names a resource only through a
  capability it actually holds, and the kernel checks the right at every use.
- **Application-level resource management**: `frame_map` takes a *user-chosen*
  virtual address. The kernel validates the capability, the alignment, and the
  window -- placement policy belongs to the process.
- **Visible cost model**: each library OS reports how many frames it pulled
  from the kernel. Policy differences show up as numbers.
- **One mechanism, many resource types**: CPU time is a capability (a budget
  the holder spends down), an endpoint is a capability (an IPC channel), a
  `BlockRange` is a capability (a bounded run of disk sectors), an
  `EventSource` is a capability (one input device). "Secure bindings" is a
  general mechanism, not a frame-only trick -- and some resources (you cannot
  enforce a CPU bound, or deliver a device interrupt, from userspace)
  genuinely earn a place in the kernel's small interface.
- **Application-level fault handling**: a process can register a ring-3
  page-fault handler. A fault in its lazy region is delivered to that handler,
  which resolves it with ordinary syscalls and resumes the faulting
  instruction -- self-paging, the exokernel's signature move. The kernel's
  mechanism is delivery and resume; the policy is the application's.
- **Policy lives in library OSes, not the kernel**: allocation (`libos`), the
  on-disk filesystem format (`libfs`), and the keyboard keymap and line editor
  (`libinput`) are all unprivileged code linked into the programs that want
  them. The kernel ships frames, sectors, and raw scancodes; what those *mean*
  is decided above the kernel.

## Architecture

```text
       several processes, scheduled, each in its own address space
  +-----------+ +-----------+ +-----------+ +-----------+ +-----------+
  | bump-user | | list-user | | producer  | | fsdemo    | | kbd-user  |
  | demo-app  | | demo-app  | | consumer  | | libfs     | | libinput  |
  | BumpAlloc | | FreeList  | | (IPC)     | | (disk FS) | | (keymap)  |
  +-----+-----+ +-----+-----+ +-----+-----+ +-----+-----+ +-----+-----+
        |    libplinth: syscall shim + int 0x80 gate shim    |
  ======+======+============+=============+============+======+======
  syscall/sysret  |              int 0x80 gate              |
   write exit      | send recv call reply event_recv ring_wait
   frame_alloc/map/free cpu_charge fault_reg/return spawn spawn_from_buffer
   ring_register ring_submit
  +----------------------------------------------------------------+
  |                        plinth kernel                           |
  |  capabilities | frames | CPU budgets | endpoints | block ranges|
  |  event sources | async rings | scheduler + timer | addr spaces |
  |  fault upcall  | virtio-blk | i8042 keyboard | irq seam        |
  |  LAPIC + I/O APIC | MSI-X | SMP: AP trampoline + BKL + per-CPU |
  +----------------------------------------------------------------+
                       ring 0, one or more CPUs
```

Two entry mechanisms: the non-blocking calls use `syscall`/`sysretq`; the
blocking ones (IPC, `event_recv`, `ring_wait`) enter through an `int 0x80`
gate, because suspending and resuming a call needs the full resumable trap
frame the fast path does not save. Block I/O is the async-ring ABI:
`ring_register`/`ring_submit` are non-blocking doorbell calls on the fast path,
and a process parks in `ring_wait` only when it has nothing left to reap.

```text
kernel/      the exokernel (no_std, x86_64-unknown-none)
  frame_alloc.rs   bitmap physical frame allocator
  capability.rs    fixed-size capability tables (frames, CPU budgets,
                   endpoints, reply caps, block ranges, event sources)
  syscall.rs       the non-blocking calls, syscall/sysret entry
  ipc.rs           the int 0x80 gate: endpoints, send/recv/call/reply,
                   plus event_recv and ring_wait (blocking ops share it)
  rings.rs         async completion rings: SQ drain + completion demux,
                   ring_register/ring_submit/ring_wait
  scheduler.rs     preemptive round-robin, per-process kernel stacks,
                   core-agnostic claim-and-run across CPUs
  timer.rs         100 Hz preemption tick: per-CPU LAPIC timer (PIT fallback)
  irq.rs           interrupt-controller seam: LAPIC + I/O APIC, MSI-X
                   (8259 PIC kept as a fallback when no MADT is present)
  acpi.rs          MADT parser: CPU + interrupt-controller topology
  smp.rs           AP bring-up: INIT-SIPI-SIPI + the long-mode trampoline
  bkl.rs           the big kernel lock: one lock, held entry-to-exit
  percpu.rs        per-CPU state via IA32_GS_BASE (current process, stacks)
  interrupts.rs    IDT, ring-3-surviving exception handlers
  process.rs       process lifecycle + teardown
  usermode.rs      ring transition: iretq in, context switch out
  fault.rs         self-paging: #PF upcall to a ring-3 handler + resume
  memory.rs        per-process address spaces: clone, map, switch, destroy
  elf.rs           ELF loader: validate a static ET_EXEC, map PT_LOAD W^X
  pci.rs           legacy PCI config-space (0xCF8/0xCFC) enumeration
  virtio_blk.rs    virtio-blk (virtio-pci) driver: one virtqueue, DMA,
                   interrupt-driven completion
  keyboard.rs      i8042 keyboard: IRQ1 -> raw scancodes
  input.rs         event sources + bounded per-source event ring
  gdt.rs           GDT/TSS, sysret-compatible selector layout
  serial.rs        serial console
  tests/           in-kernel test suite (70 tests, run in QEMU)

libplinth/   user-side syscall + gate shim -- deliberately NOT a library OS
libos/       allocator library OSes (BumpAlloc, FreeListAlloc) + ring, a
             reference no_std async block-I/O executor over the ring ABI
libfs/       a read-only boot-archive parser -- the filesystem as a libOS
libinput/    a Set-1 keymap (with shift) and line reader -- input as a libOS
demo-app/    the shared allocator workload, generic over the memory policy

user programs (ring 3, each its own crate):
  hello-user/    syscall-surface integration test (runs first at boot)
  bump-user/ list-user/   demo-app over the two allocators
  crash-user/    deliberate null dereference (fault isolation)
  greedy-user/   deliberate CPU-budget overdraw
  lazy-user/     self-paging: a ring-3 fault handler maps pages on demand
  spin-user/     CPU-bound process for the preemptive-scheduler demo
  pingpong-user/ share-user/ rpc-user/   the IPC demos (rendezvous, frame
                 transfer, call/reply RPC)
  spawner-user/ grantee-user/   spawn a child and transfer it a capability
  blk-user/      read disk sectors through a bounded BlockRange
  asyncblk-user/ several overlapping reads via the libos async executor
  fsdemo-user/   load a program off disk with libfs + spawn_from_buffer
  diskhello-user/   lives only in the boot archive, never embedded
  evt-user/      read a raw keyboard event through an EventSource
  kbd-user/      read a line through libinput
  faultchild-user/  a child that faults, for liveness testing
  template-user/    minimal skeleton to copy for a new program (see GUIDE.md)
xtask/       build orchestration: user binaries, disk images, QEMU,
             smoke + test harnesses, asm clobber lint
```

## Design decisions

- **No kernel heap.** Capability tables, process state, endpoints, and event
  rings are fixed-size arrays. A toy kernel that needs malloc to express
  ownership has already smuggled in a policy.
- **Preemptive multitasking, but a non-preemptible kernel.** A 100 Hz timer
  preempts ring-3 code; the kernel saves the full interrupted context,
  switches address space and per-process kernel stack, and round-robins
  independent processes. But the kernel reschedules only on the way *out* to
  ring 3 -- it never preempts itself -- so kernel data structures are never
  reentered on a given core. On one CPU that meant no locks at all; under SMP a
  single big kernel lock (`bkl.rs`), held entry-to-exit, does the cross-core
  arbitration, so the non-preemptible-kernel property now reads *per core* and
  the lock serializes the rest. Preemption cost the exact-trace smoke test;
  per-process ordering and no-leak invariants replaced it.
- **Per-process address spaces.** Each process gets its own page tables: a
  private L4 whose user half is its own and whose kernel half is copied from
  the bootloader's L4, so the kernel runs correctly under any process's CR3.
  Creating a process clones the kernel half; destroying it frees the user
  half's page-table frames and the L4, so an address space leaks nothing --
  the free-frame count is flat across the whole boot. Isolation is what lets a
  capability be *transferred* into a genuinely separate domain over IPC or
  `spawn`.
- **Programs are real ELF images, loaded with W^X -- from memory or disk.**
  The kernel parses a static `ET_EXEC` ELF and maps each `PT_LOAD` segment at
  its own address with exactly the access it asks for: code executable and
  read-only, data writable and non-executable, never both on one page.
  Parsing is strict and allocation-free -- every field is bounds-checked
  against the file and rejected if malformed or out of the image window, so a
  bad binary fails to load rather than corrupting anything. The same validator
  runs whether the image is embedded at build time or read off the disk by a
  filesystem library OS and handed to `spawn_from_buffer` as untrusted bytes.
- **Two entry mechanisms, and each call past the core had to earn it.** The
  bar for the interface is high: if a feature can live in userspace, it does.
  Frame management is pure mechanism on the `syscall` fast path. Everything
  added since is either a case the mechanism itself cannot live in userspace --
  `cpu_charge` (a process cannot enforce a CPU bound against itself),
  `fault_reg`/`fault_return` (a process cannot deliver a hardware fault to
  itself), `spawn`/`spawn_from_buffer` (a process cannot create an isolated
  address space) -- or a blocking operation that needs the kernel to suspend
  and resume it, which is why IPC, `event_recv`, and `ring_wait` enter
  through the `int 0x80` gate rather than the fast path. Everything those calls
  *do* with the mechanism -- spend policy, paging policy, the filesystem
  format, the keymap, the async I/O executor -- is still application code.
- **Block I/O is async completion rings, and the executor is just policy.** A
  library OS submits capability-named requests into a shared-memory queue and
  reaps results from another, the kernel on the path only for a batched doorbell
  (`ring_submit`) and the completion IRQ -- never a kernel entry per read, and
  many reads in flight at once. The kernel ships only the *mechanism* (it stays
  the sole writer of physical DMA addresses, so the device is multiplexed with
  no IOMMU); the `no_std` async executor that turns a completion into a woken
  future lives in `libos`, replaceable like every other policy.
- **Status is split from payload on every blocking call.** IPC, `event_recv`,
  and the ring's completions return a *status* word separate from their payload
  (the message/event in `RSI`, the disk data in the DMA'd frame). A
  peer-controlled or device-controlled value can never be mistaken for an
  error -- not even `u64::MAX` -- including the `IPC_PEER_DIED` status that
  frees a process blocked on a counterpart that died.
- **One interrupt-controller seam.** The keyboard IRQ and the virtio-blk
  completion IRQ both route through a single `irq` module, built as the one
  place an interrupt-controller swap has to touch -- and that swap has since
  happened. `irq` now drives the Local APIC + I/O APIC, discovered from the
  ACPI MADT (`acpi.rs`), with MSI-X for virtio completions and a per-CPU LAPIC
  timer for the preemption tick; it keeps the 8259 PIC and the PIT as fallbacks
  when no MADT is present. The rest of the kernel still asks only for "the IRQ
  for this line" and "EOI this line" without knowing which controller answers --
  the seam paid off exactly as intended.
- **Symmetric multiprocessing, under a single big kernel lock.** Plinth boots
  every CPU the MADT lists: the BSP wakes each application processor with
  INIT-SIPI-SIPI through a real-mode trampoline (`smp.rs`) that walks it to long
  mode and into Rust. Per-CPU state (the current process, the kernel stacks)
  lives behind `IA32_GS_BASE` (`percpu.rs`); a single big kernel lock serializes
  every shared structure; a reschedule IPI wakes a halted core when work
  appears. A process is pinned to the core that first schedules it -- no
  cross-core migration yet -- which is the first step toward per-core ownership,
  not a stopgap. Scaling the lock (per-CPU run queues, work stealing) is
  deliberately deferred until contention data asks for it; a uniprocessor
  (`-smp 1`) lane is kept as the deterministic regression net. The direction is
  share-nothing, not faster locks.
- **Self-paging is signal delivery, kept honest.** A `#PF` in a registered
  lazy region is delivered to a ring-3 handler by saving the full faulting
  register context, `iretq`-ing into the handler on its own stack, and a
  sigreturn-style `fault_return` that restores the context and retries the
  instruction. One fault is in flight per process at a time, so one saved trap
  frame suffices; a fault *inside* a handler is unhandleable and terminates the
  process. The handler entry and stack are user-supplied but only ever entered
  at CPL 3 -- a bad value faults in ring 3, it never reaches into the kernel.

## Build and run

Requirements: Rust nightly (pinned via `rust-toolchain.toml`, needs
`rust-src`) and `qemu-system-x86_64` on PATH.

```text
cargo xtask run     # build everything, boot in QEMU
cargo xtask smoke   # boot captured, assert expected_boot_log.txt in order
cargo xtask test    # in-kernel test suite under QEMU
cargo xtask check   # lint libplinth asm blocks for syscall clobbers
cargo xtask run-gdb # boot paused, GDB server on :1234
```

First build downloads OVMF firmware (cached in `target/ovmf/`) and compiles
the bootloader; expect a few minutes. Slow machine or CI?
`PLINTH_QEMU_TIMEOUT=180` extends the QEMU watchdog. `cargo xtask run` then
boots into the live system -- type into the QEMU window and kbd-demo echoes
your line through `libinput`.

## Writing your own programs

Plinth runs your code in ring 3 over a stable syscall interface.
[ABI.md](ABI.md) is the contract -- syscalls, the IPC and device gate, the
executable format, and entry state, versioned as **v2.4** -- and
[GUIDE.md](GUIDE.md) is the walkthrough: copy `template-user/` to start a
program, and see how memory policy goes in a library OS rather than the
kernel. Where the project is headed is in [ROADMAP.md](ROADMAP.md); how to
contribute is in [CONTRIBUTING.md](CONTRIBUTING.md).

## Testing

Three layers, all in CI on every push:

1. `cargo xtask test` -- 65 in-kernel unit tests (frame allocator, capability
   table, CPU-budget charging, the ELF loader/validator, the scheduler's
   `pick_next` policy, the IPC wait queue, and the input event ring) executed
   inside QEMU, reported over a serial protocol (`[PASS]`/`[FAIL]`/`[SUITE]`)
   that xtask parses. Pure library-OS logic that does not need the kernel --
   the `libfs` archive parser and the `libinput` keymap -- is host-unit-tested
   as well.
2. `cargo xtask smoke` -- full boot with captured serial output. The
   single-process demos are asserted line-by-line in order
   (`expected_boot_log.txt`); the multi-process Phase 2 demos are checked for
   per-process ordering (interleaving-robust) and that free frames return to
   baseline around each demo, since the cross-process interleaving is
   deliberately nondeterministic. `PLINTH_ICOUNT` can pin the interleaving
   reproducibly for debugging; the kernel never depends on it.
   `cargo xtask smoke-smp` reruns the same interleaving-robust assertions on 2,
   3, and 4 cores -- the SMP regression lane, since multicore output is no
   longer a fixed transcript the single-core boot can stand in for.
3. `cargo xtask check` -- static lint: every syscall `asm!` block in libplinth
   must declare the full clobber set the kernel ABI implies.

## Current limitations

These are where Plinth is today, not where it stops. The syscall interface is
a documented, versioned contract ([ABI.md](ABI.md), v2.4), so you can write
your own programs and library OSes against it; growing Plinth toward a
genuinely usable general-purpose exokernel is the ongoing direction
([ROADMAP.md](ROADMAP.md)).

- **SMP, but not yet scaled.** Plinth boots and schedules on multiple cores,
  but all kernel entry serializes on a single big kernel lock, so adding cores
  does not yet add kernel throughput. Per-CPU run queues and work stealing -- the
  step that makes the lock stop being a bottleneck -- are the next roadmap item,
  deferred on purpose until there is contention data to justify the complexity.
  Real-machine device support (leaving QEMU's defaults) is the milestone after
  that.
- **`write` is uncapability-gated console output** for demo legibility -- the
  one call that is not behind a capability. A single `write` is atomic with
  respect to other processes, so interleaved demos never tear a line.
- **CPU budgets are still spent cooperatively.** The scheduler preempts for
  *fairness* (time-slicing under the timer), but the CPU-time *capability* is
  debited by `cpu_charge` at points the process chooses; a process that spins
  without charging is time-sliced like any other but never billed. The demo is
  the capability model for CPU time -- mint, spend, enforce-at-charge, reclaim
  -- layered over preemptive scheduling, not a deadline or priority system.
- **Self-paging is demand-zero only.** One fault in flight per process, a
  single fixed lazy window, and the handler maps fresh zeroed frames. It does
  not page from disk -- the disk has its own explicit async-ring read path --
  so it demonstrates the upcall and the resume, not a full unified
  virtual-memory system.
- **Storage is read-only, one filesystem.** A virtio-blk driver and a
  read-only boot archive parsed by `libfs`; there is no `block_write` in the
  ABI yet, and the archive format is the only filesystem. No network.
- **Input is one keyboard, raw events only.** The kernel ships raw Set-1
  scancodes from the i8042 device; one reader per source. Keymaps, layouts,
  and line editing are library-OS policy (`libinput`); fanning input out to
  many consumers would itself be a library OS over the primitive.
- **IPC endpoints are kernel-granted.** A process does not yet create its own
  endpoints: the kernel mints one per `spawn` (the result channel) and may
  grant one at launch. A process-facing endpoint-create call is not part of
  the ABI yet.
- **Known engineering gap, documented in the code:** a kernel-mode `#PF` on a
  user pointer would take the fatal path (mitigated -- syscalls validate user
  pointers against the page tables before dereferencing). Address spaces, by
  contrast, are fully reclaimed: a process's page-table frames are freed when
  it exits, so nothing leaks across the boot.

## Field notes

Hard-won, possibly useful to other no_std kernel people:

- **LLVM's loop-to-memcpy pass vs. your memcpy.** Defining `memcpy` as a naive
  byte loop at opt-level 3 + LTO lets LLVM recognise the loop and replace it
  with a call to `memcpy` -- itself. The resulting recursion overflows the user
  stack. The fix (libplinth/src/lib.rs) is volatile accesses, which cannot be
  folded into a library call. Relatedly: `==` on `&[u8]` in a no_std binary
  lowers to a `memcmp` that can resolve to a null weak symbol -- an
  instruction-fetch fault at RIP=0 -- unless strong intrinsics are linked.
- **ovmf-prebuilt is pinned to =0.2.8.** The 0.2.9-bundled edk2 build hangs
  with zero serial output (not even BdsDxe lines) under qemu-system-x86_64
  with q35 + pflash. If you bump the pin, re-test boot before trusting anything
  else.
- **Pin your nightly by date.** `channel = "nightly"` means a different
  compiler on every fresh machine. This repo built green locally for days
  while CI failed instantly: the runner pulled that morning's nightly, on
  which bootloader 0.11's BIOS-stage builds die with E0463.
  `rust-toolchain.toml` now names the exact dated nightly the suite is verified
  against.
- **syscall asm clobbers are a lint, not a comment.** The kernel's dispatcher
  may clobber every caller-saved register; one missing declaration means the
  compiler caches a value in a register the kernel destroys, and the failure
  appears far from the cause. `cargo xtask check` machine-checks the contract.
- **A blocking call needs a different door than a fast one.** The `syscall`
  fast path saves only `rcx`/`r11`; it cannot suspend a caller and resume it
  later with its registers intact. Every Plinth call that blocks -- IPC,
  `event_recv`, `ring_wait` -- goes through the `int 0x80` gate instead,
  whose interrupt entry saves the full resumable trap frame. Several arrived as
  `syscall` calls and had to move once they learned to block.

## License

MIT. See [LICENSE](LICENSE).
