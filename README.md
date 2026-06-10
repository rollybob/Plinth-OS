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
plinth: frame allocator ready (61695 frames free)
plinth: GDT + TSS loaded
plinth: IDT loaded
plinth: syscall interface ready
plinth: running hello (872 bytes)
hello: ring 3
hello: frame mapped and writable
hello: done
plinth: hello exited (code 0)
plinth: 61692 frames free
plinth: running bump-demo (4296 bytes)
demo: policy = bump
demo: a = 0x10000000
demo: b = 0x10000600
demo: freed a
demo: c = 0x10000c00
demo: c got a new address
demo: kernel frames used: 2
plinth: bump-demo exited (code 0)
plinth: 61692 frames free
plinth: running crash-demo (384 bytes)
crash: about to dereference null
plinth: [user fault] #PF page fault rip=0x40001a err=0x6 addr=0x0
plinth: terminating user process
plinth: crash-demo faulted
plinth: 61692 frames free
plinth: running list-demo (6064 bytes)
demo: policy = freelist
demo: a = 0x10000000
demo: b = 0x10000600
demo: freed a
demo: c = 0x10000000
demo: c reused a freed block
demo: kernel frames used: 1
plinth: list-demo exited (code 0)
plinth: 61692 frames free
plinth: boot ok
```

Three things are happening:

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
allocator never frees anything. Policies can be lazy; the kernel's
capability accounting is not.

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

## Architecture

```text
                            ring 3
  +--------------+   +--------------+   +--------------+
  |  bump-user   |   |  crash-user  |   |  list-user   |
  |  demo-app    |   |              |   |  demo-app    |
  |  BumpAlloc   |   |  (no libOS)  |   | FreeListAlloc|
  +------+-------+   +------+-------+   +------+-------+
         |       libplinth: syscall shim       |
  =======+=================+===================+=======  syscall/sysret
         |  write  exit  frame_alloc  frame_map  frame_free
  +----------------------------------------------------+
  |                   plinth kernel                     |
  |  capabilities  |  frame allocator  |  fault wall    |
  +----------------------------------------------------+
                            ring 0
```

```text
kernel/      the exokernel (no_std, x86_64-unknown-none)
  frame_alloc.rs   bitmap physical frame allocator
  capability.rs    fixed-size capability tables (no kernel heap)
  syscall.rs       the five syscalls, syscall/sysret entry
  process.rs       synchronous process lifecycle + teardown
  usermode.rs      ring transition: iretq in, longjmp out
  interrupts.rs    ring-3-surviving exception handlers
  gdt.rs           GDT/TSS, sysret-compatible selector layout
  memory.rs        user page mappings over the bootloader's tables
  tests/           in-kernel test suite (10 tests, run in QEMU)
libplinth/   user-side syscall shim -- deliberately NOT a library OS
libos/       two library OSes: BumpAlloc and FreeListAlloc
demo-app/    the shared workload, generic over the memory policy
hello-user/  syscall-surface integration test (runs first at boot)
crash-user/  deliberate null dereference
xtask/       build orchestration: user binaries, disk image, QEMU,
             smoke + test harnesses, asm clobber lint
```

## Design decisions

- **No kernel heap.** Capability tables and process state are fixed-size
  arrays. A toy kernel that needs malloc to express ownership has
  already smuggled in a policy.
- **Synchronous processes.** One process at a time, run to completion.
  `enter_user` saves kernel context and iretq's to ring 3; the exit
  syscall or a fault longjmps back. No scheduler, no timer, no APIC --
  and fully deterministic serial output, which is what makes the
  line-by-line smoke test possible.
- **Uniprocessor, on purpose.** SMP would triple the kernel and teach
  nothing about exokernels.
- **Five syscalls.** If a sixth is needed, something probably belongs in
  userspace instead.

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

## Testing

Three layers, all in CI on every push:

1. `cargo xtask test` -- 10 in-kernel unit tests (frame allocator,
   capability table) executed inside QEMU, reported over a serial
   protocol (`[PASS]`/`[FAIL]`/`[SUITE]`) that xtask parses.
2. `cargo xtask smoke` -- full boot with captured serial output,
   asserting every line of the demo narrative above, in order.
3. `cargo xtask check` -- static lint: every syscall `asm!` block in
   libplinth must declare the full clobber set the kernel ABI implies.

## Limitations (deliberate)

Single CPU, single process at a time, no preemption, no disk, no
network, frames are the only resource type, and `write` is uncapability-
gated console output for demo legibility. Each of these is a scoping
decision, not a roadmap: the project demonstrates the exokernel
architecture, and stops.

Known engineering gaps, documented in the code: a kernel-mode #PF on a
user pointer would take the fatal path (mitigated -- syscalls validate
user pointers against the page tables before dereferencing); page-table
frames for user regions are allocated once and never reclaimed.

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
- **syscall asm clobbers are a lint, not a comment.** The kernel's C
  dispatcher may clobber every caller-saved register; one missing
  declaration means the compiler caches a value in a register the
  kernel destroys, and the failure appears far from the cause.
  `cargo xtask check` machine-checks the contract.

## License

MIT. See [LICENSE](LICENSE).
