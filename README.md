# Plinth

A toy exokernel in Rust -- the exokernel idea reduced to the smallest
codebase that can demonstrate it. The whole kernel reads in one sitting.

A plinth is the bare slab a column stands on: it carries the load and
imposes nothing about what is built above it. That is the contract here.
The kernel owns physical memory and multiplexes it securely through
capabilities, but refuses to define what memory management *is*. Two
applications on the same kernel answer that question differently, in
unprivileged code, and the boot log shows the difference.

## The demo

This is one boot, verified line-by-line in CI:

```text
plinth: kernel entry
plinth: frame allocator ready (61583 frames free)
plinth: GDT + TSS loaded
plinth: IDT loaded
plinth: syscall interface ready
plinth: running hello (635 bytes)
hello: ring 3
hello: frame mapped and writable
hello: done
plinth: hello exited (code 0)
plinth: 61583 frames free
plinth: running bump-demo (4061 bytes)
demo: policy = bump
demo: a = 0x10000000
demo: b = 0x10000600
demo: freed a
demo: c = 0x10000c00
demo: c got a new address
demo: kernel frames used: 2
plinth: bump-demo exited (code 0)
plinth: 61583 frames free
plinth: running crash-demo (147 bytes)
crash: about to dereference null
plinth: [user fault] #PF page fault rip=0x40001a err=0x6 addr=0x0
plinth: terminating user process
plinth: crash-demo faulted
plinth: 61583 frames free
plinth: running list-demo (5822 bytes)
demo: policy = freelist
demo: a = 0x10000000
demo: b = 0x10000600
demo: freed a
demo: c = 0x10000000
demo: c reused a freed block
demo: kernel frames used: 1
plinth: list-demo exited (code 0)
plinth: 61583 frames free
plinth: running greedy-demo (1593 bytes)
greedy: spending CPU budget
greedy: charged 256, remaining = 768
greedy: charged 256, remaining = 512
greedy: charged 256, remaining = 256
greedy: charged 256, remaining = 0
plinth: [out of budget] terminating user process
plinth: greedy-demo out of budget
plinth: 61583 frames free
plinth: running lazy-demo (5469 bytes)
lazy: registering fault handler
lazy: serviced fault at 0x18000000
lazy: serviced fault at 0x18001000
lazy: serviced fault at 0x18002000
lazy: serviced fault at 0x18003000
lazy: all pages materialized on demand
plinth: lazy-demo exited (code 0)
plinth: 61583 frames free
plinth: running spawner-demo (1646 bytes)
spawner: allocated a frame, granting it to a child
grantee: used granted frame at 0x10004000
spawner: child returned 42
plinth: spawner-demo exited (code 0)
plinth: 61583 frames free
plinth: boot ok
```

Six things are happening:

**Same app, different OS.** bump-demo and list-demo are the *identical*
workload (`demo-app/`) -- allocate three 1536-byte blocks, free the
first, allocate again -- linked against two different library OSes
(`libos/`). The bump policy never reuses memory: the third allocation
lands at a new address and costs a second kernel frame. The free-list
policy recycles: the third allocation comes back at the freed block's
address on a single kernel frame. Same kernel, same syscalls, different
memory management -- because memory management is application code here,
not kernel code.

**A crash is an event, not a catastrophe.** crash-demo dereferences
null between the two demos. The kernel logs the ring-3 page fault,
terminates the process, and runs the next one.

**Nothing leaks.** The free-frame count after every teardown is
identical -- including after the crash, and including bump-demo, whose
allocator never frees anything. (The absolute number tracks the kernel's
own footprint and shifts as the kernel grows; only its flatness across the
boot is the point.) Policies can be lazy; the kernel's capability
accounting is not.

**CPU time is a capability too.** greedy-demo is minted a fixed CPU
budget at spawn and spends it with `cpu_charge`, watching the balance
fall to zero. When it charges past zero it has tried to consume a
resource it no longer holds, so the kernel terminates it exactly as it
did the crash -- same teardown, no leak. The kernel enforces the bound;
*how* to spend the budget is the library OS's call.

**Userspace handles its own page faults.** lazy-demo registers a ring-3
fault handler and then touches unmapped pages. Each first touch faults --
the *same* `#PF` that kills crash-demo -- but here the kernel hands the
fault back to the process, which maps a frame with the ordinary
`frame_alloc`/`frame_map` syscalls and returns; the faulting instruction
is retried and succeeds. Demand paging where the *application*, not the
kernel, decides what backs an address. The opposite outcome to crash-demo
from the identical hardware event, chosen entirely in unprivileged code.

**Capabilities cross isolated address spaces.** spawner-demo allocates a
frame and spawns a child in a *separate* address space, transferring it the
frame capability. The child runs in its own page tables, maps the frame at
an address of its choosing, uses it, and returns a result the parent
collects. The child never allocated that frame and was never handed its
contents -- it can touch the frame only because the capability was moved
into its table. Delegation of authority between mutually isolated
protection domains, which is what makes a capability more than a handle.

## Why exokernels

A conventional kernel bundles mechanism (multiplexing hardware safely)
with policy (what a process, file, or heap is). The exokernel argument
-- Engler, Kaashoek, and O'Toole, SOSP '95 -- is that the bundle is the
problem: the kernel should securely expose raw resources, and every
abstraction should live in unprivileged *library OSes* that applications
choose, replace, or rewrite.

Plinth implements the minimum machinery that makes the argument
concrete:

- **Secure bindings**: physical frames are granted as capabilities --
  kernel-held records of (resource, rights), referred to by slot index.
  Userspace names a frame only through a capability it actually holds.
- **Application-level resource management**: `frame_map` takes a
  *user-chosen* virtual address. The kernel validates the capability,
  the alignment, and the window -- placement policy belongs to the
  process.
- **Visible cost model**: each library OS reports how many frames it
  pulled from the kernel. Policy differences show up as numbers.
- **More than one resource type**: CPU time is also a capability -- a
  budget the holder spends down rather than a thing it owns. It shows
  that "secure bindings" is a general mechanism, not a frame-only trick,
  and that some resources (you cannot enforce a CPU bound from
  userspace) genuinely earn a place in the kernel's small interface.
- **Application-level fault handling**: a process can register a ring-3
  page-fault handler. A fault in its lazy region is delivered to that
  handler, which resolves it with ordinary syscalls and resumes the
  faulting instruction -- self-paging, the exokernel's signature move.
  The kernel's mechanism is delivery and resume; the policy (what backs
  the address) is the application's.
- **Capabilities are transferable authority**: each process runs in its
  own address space, and `spawn` launches a child with a capability moved
  out of the parent's table into the child's. A capability is not just a
  local handle -- it is authority that can be delegated across an isolation
  boundary, which is the property that makes capability systems more than
  access-control lists.

## Architecture

```text
              ring 3 -- each in its own address space
  +--------------+   +--------------+   +--------------+
  |  bump-user   |   |  crash-user  |   |  list-user   |
  |  demo-app    |   |              |   |  demo-app    |
  |  BumpAlloc   |   |  (no libOS)  |   | FreeListAlloc|
  +------+-------+   +------+-------+   +------+-------+
         |       libplinth: syscall shim       |
  =======+=================+===================+=======  syscall/sysret
         | write exit frame_alloc frame_map frame_free
         | cpu_charge fault_reg fault_return spawn
  +----------------------------------------------------+
  |                   plinth kernel                     |
  |  capabilities | frames | fault upcall | per-proc AS |
  +----------------------------------------------------+
                            ring 0
```

```text
kernel/      the exokernel (no_std, x86_64-unknown-none)
  frame_alloc.rs   bitmap physical frame allocator
  capability.rs    fixed-size capability tables (frames + CPU budgets)
  syscall.rs       the nine syscalls, syscall/sysret entry, spawn nesting
  process.rs       synchronous process lifecycle + teardown
  usermode.rs      ring transition: iretq in, longjmp out
  interrupts.rs    ring-3-surviving exception handlers
  fault.rs         self-paging: #PF upcall to a ring-3 handler + resume
  gdt.rs           GDT/TSS, sysret-compatible selector layout
  memory.rs        per-process address spaces: clone, map, switch, destroy
  elf.rs           ELF loader: validate a static ET_EXEC, map PT_LOAD W^X
  tests/           in-kernel test suite (32 tests, run in QEMU)
libplinth/   user-side syscall shim -- deliberately NOT a library OS
libos/       two library OSes: BumpAlloc and FreeListAlloc
demo-app/    the shared workload, generic over the memory policy
hello-user/  syscall-surface integration test (runs first at boot)
crash-user/  deliberate null dereference
greedy-user/ deliberate CPU-budget overdraw
lazy-user/   self-paging: a ring-3 fault handler maps pages on demand
spawner-user/ allocates a frame and spawns a child, granting the cap
grantee-user/ spawned child: uses a frame it only holds by transfer
template-user/ minimal skeleton to copy for a new program (see GUIDE.md)
xtask/       build orchestration: user binaries, disk image, QEMU,
             smoke + test harnesses, asm clobber lint
```

## Design decisions

- **No kernel heap.** Capability tables and process state are fixed-size
  arrays. A toy kernel that needs malloc to express ownership has
  already smuggled in a policy.
- **Synchronous processes.** One process runs at a time, to completion.
  `enter_user` saves kernel context and iretq's to ring 3; the exit
  syscall or a fault longjmps back. No scheduler, no timer, no APIC --
  and fully deterministic serial output, which is what makes the
  line-by-line smoke test possible. `spawn` nests this model (a parent
  blocks while its child runs) without breaking determinism.
- **Per-process address spaces.** Each process gets its own page tables: a
  private L4 whose user half (PML4[0]) is its own and whose kernel half is
  copied from the bootloader's L4, so the kernel runs correctly under any
  process's CR3. Creating a process clones the kernel half; destroying it
  frees the user half's page-table frames and the L4, so an address space
  leaks nothing -- the free-frame count is flat across the whole boot,
  including the spawn, which builds and tears down two. Isolation is what
  lets `spawn` transfer a capability into a genuinely separate domain.
- **Programs are real ELF images, loaded with W^X.** The kernel parses a
  static `ET_EXEC` ELF and maps each `PT_LOAD` segment at its own address
  with exactly the access it asks for: code executable and read-only, data
  writable and non-executable, never both on one page. Parsing is strict
  and allocation-free -- every field is bounds-checked against the file and
  rejected if it is malformed or would place a segment outside the image
  window, so a bad binary fails to load rather than corrupting anything.
  This is the same code path an on-disk program would take; today the
  images are still embedded at build time. (Earlier versions mapped a flat
  blob writable-and-executable; real per-segment protection replaced that.)
- **Uniprocessor, on purpose.** SMP would triple the kernel and teach
  nothing about exokernels.
- **Nine syscalls, and each one past the core five had to earn it.** The
  bar for adding to the interface is high: if a feature can live in
  userspace, it does. Frame management is five calls of pure mechanism.
  The other four are cases where the *mechanism itself* cannot live in
  userspace: `cpu_charge` (a process cannot enforce a CPU bound against
  itself), `fault_reg`/`fault_return` (a process cannot deliver a hardware
  fault to itself, or return from ring 0 to a faulting instruction), and
  `spawn` (a process cannot create an isolated address space or move a
  capability into another protection domain). Everything those calls *do*
  with the mechanism -- spend policy, paging policy, what to run and what
  to delegate -- is still application code.
- **Spawn is synchronous nesting, with the kernel-stack discipline made
  real.** A child runs one level down, in its own address space, on its
  own kernel syscall stack -- because the parent's syscall is suspended
  mid-flight while the child runs, and a shared kernel stack would let the
  child's syscalls clobber the parent's frame. That "every suspendable
  context needs its own kernel stack" is the foundational truth a single-
  process kernel gets to ignore; `spawn` stops ignoring it. Nesting is
  depth-bounded by a fixed array (no heap), and there is no scheduler --
  just depth-first call and return.
- **Self-paging is signal delivery, kept honest.** A #PF in a registered
  lazy region is delivered to a ring-3 handler by saving the full faulting
  register context, `iretq`-ing into the handler on its own stack, and a
  sigreturn-style `fault_return` that restores the context and retries the
  instruction. One fault is in flight at a time (synchronous, single
  process), so one saved trap frame suffices; a fault *inside* a handler
  is unhandleable and terminates the process. The handler entry and stack
  are user-supplied but only ever entered at CPL 3 -- a bad value faults
  in ring 3, it never reaches into the kernel.

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

First build downloads OVMF firmware (cached in `target/ovmf/`) and
compiles the bootloader; expect a few minutes. Slow machine or CI?
`PLINTH_QEMU_TIMEOUT=180` extends the QEMU watchdog.

## Writing your own programs

Plinth runs your code in ring 3 over a stable syscall interface.
[ABI.md](ABI.md) is the contract -- syscalls, executable format, and entry
state, frozen as v1 -- and [GUIDE.md](GUIDE.md) is the walkthrough: copy
`template-user/` to start a program, and see how memory policy goes in a
library OS rather than the kernel. Where the project is headed is in
[ROADMAP.md](ROADMAP.md); how to contribute is in
[CONTRIBUTING.md](CONTRIBUTING.md).

## Testing

Three layers, all in CI on every push:

1. `cargo xtask test` -- 32 in-kernel unit tests (frame allocator,
   capability table, CPU-budget charging, and the ELF loader/validator,
   whose parser is a pure function over a byte slice) executed inside QEMU,
   reported
   over a serial protocol (`[PASS]`/`[FAIL]`/`[SUITE]`) that xtask parses.
2. `cargo xtask smoke` -- full boot with captured serial output,
   asserting every line of the demo narrative above, in order.
3. `cargo xtask check` -- static lint: every syscall `asm!` block in
   libplinth must declare the full clobber set the kernel ABI implies.

## Current limitations

Single CPU, single process at a time, no preemption, no disk, no
network, frames and CPU budgets are the only resource types, and `write`
is uncapability-gated console output for demo legibility. Programs are
real ELF binaries, but they are still embedded at build time -- there is
no disk to load them from yet.

These are where Plinth is today, not where it stops. The syscall
interface is now a documented, versioned contract (see [ABI.md](ABI.md)),
so you can write your own programs and library OSes against it; growing
Plinth toward a genuinely usable general-purpose exokernel is the ongoing
direction.

CPU-budget enforcement is **cooperative**: `cpu_charge` debits the
budget at points the process chooses, and a process that spins without
ever charging is never billed and never stopped. Preemptive enforcement
is exactly what a timer interrupt is for, and the timer is deliberately
out of scope (it would end the deterministic serial output the whole
test harness rests on). What the kernel demonstrates is the *capability
model* for CPU time -- mint, spend, enforce-at-charge, reclaim -- not a
preemptive scheduler.

Self-paging is scoped to match: one fault in flight at a time, a single
fixed lazy window, and demand-zero backing only (the handler maps fresh
frames -- there is no disk to page from). It demonstrates the upcall and
the resume, not a full virtual-memory system.

`spawn` is synchronous and depth-bounded: a parent blocks while its child
runs, nesting is capped by a fixed array of kernel stacks, and a child is
launched from an embedded set by id (not from arbitrary user-supplied
bytes). It demonstrates isolated process creation, capability transfer,
and protected call/return -- not a general process model (no concurrency,
no scheduling, no wait/reap or signals; those belong to a library OS).

Known engineering gap, documented in the code: a kernel-mode #PF on a
user pointer would take the fatal path (mitigated -- syscalls validate
user pointers against the page tables before dereferencing). Address
spaces, by contrast, are now fully reclaimed: a process's page-table
frames are freed when it exits, so nothing leaks across the boot.

## Field notes

Hard-won, possibly useful to other no_std kernel people:

- **LLVM's loop-to-memcpy pass vs. your memcpy.** Defining `memcpy` as a
  naive byte loop at opt-level 3 + LTO lets LLVM recognise the loop and
  replace it with a call to `memcpy` -- itself. The resulting recursion
  overflows the user stack. The fix (libplinth/src/lib.rs) is volatile
  accesses, which cannot be folded into a library call. Relatedly: `==`
  on `&[u8]` in a no_std binary lowers to a `memcmp` that can resolve to
  a null weak symbol -- an instruction-fetch fault at RIP=0 -- unless
  strong intrinsics are linked.
- **ovmf-prebuilt is pinned to =0.2.8.** The 0.2.9-bundled edk2 build
  hangs with zero serial output (not even BdsDxe lines) under
  qemu-system-x86_64 with q35 + pflash. If you bump the pin, re-test
  boot before trusting anything else.
- **Pin your nightly by date.** `channel = "nightly"` means a different
  compiler on every fresh machine. This repo built green locally for
  days while CI failed instantly: the runner pulled that morning's
  nightly, on which bootloader 0.11's BIOS-stage builds die with E0463.
  `rust-toolchain.toml` now names the exact dated nightly the suite is
  verified against.
- **syscall asm clobbers are a lint, not a comment.** The kernel's C
  dispatcher may clobber every caller-saved register; one missing
  declaration means the compiler caches a value in a register the
  kernel destroys, and the failure appears far from the cause.
  `cargo xtask check` machine-checks the contract.

## License

MIT. See [LICENSE](LICENSE).
