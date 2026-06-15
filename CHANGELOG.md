# Changelog

All notable changes to Plinth are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project aims to
follow semantic versioning. ABI v1 (see [ABI.md](ABI.md)) is frozen:
released versions do not break it.

## [Unreleased]

### Added
- Synchronous IPC: capability-named endpoints (`kernel/src/ipc.rs`).
  `send`/`recv` are a bufferless rendezvous; a message can transfer a
  capability (a zero-copy frame handoff -- the receiver maps the same physical
  frame); `call`/`reply` is request/response RPC, where the server answers via
  a one-shot reply capability and needs no send right of its own. Blocking
  operations enter through a software-interrupt gate so they reuse the
  scheduler's context-switch path.
- `spawn` is reconciled with the scheduler: it launches the child as an
  independent scheduled process and returns a handle (a receive capability on
  a result channel); the parent waits by `recv`-ing it. This removed the old
  synchronous spawn nesting (per-depth syscall stacks and depth limit).

### Changed
- `spawn` no longer returns the child's exit code synchronously; it returns a
  wait handle and the child reports results over IPC. (An ABI v2 note is owed
  before release; ABI v1 still documents the old behavior.)

### Added (scheduler, earlier this cycle)
- Preemptive round-robin scheduler (`kernel/src/scheduler.rs`): a 100 Hz PIT
  timer preempts ring-3 code, the kernel saves the full interrupted context,
  switches address space and per-process kernel stack, and resumes another
  process. The kernel is non-preemptible (the timer reschedules only out of
  ring 3), so kernel data structures are never reentered. Crosses the project
  from one-process-at-a-time to real CPU multiplexing.
- A `spin-user` demo: the boot path launches three independent CPU-bound
  processes under the scheduler; their output interleaves in the log
  (preemption made visible) while each process's own lines stay in program
  order.
- `xtask smoke` now also checks per-process ordering (interleaving-robust,
  replacing an exact-trace assertion) and that free frames return to baseline
  after the scheduler demo (no leak at quiescence). Scheduler `pick_next`
  policy is covered by 6 new unit tests.
- Opt-in `PLINTH_ICOUNT` makes preemptive interleaving reproducible across
  runs for debugging; the kernel never depends on it.

## [2.1.0] - 2026-06-14

### Added
- In-kernel ELF loader (`kernel/src/elf.rs`): the kernel loads a static,
  non-PIE `ET_EXEC` ELF, mapping each `PT_LOAD` segment at its own address
  with per-page W^X. The parser is strict, allocation-free, and
  bounds-checks every field; a malformed image fails to load rather than
  corrupting anything. 19 unit tests cover the validator.
- [ABI.md](ABI.md): the syscall interface, executable format, and process
  entry state, documented and frozen as ABI v1.
- [GUIDE.md](GUIDE.md) and a `template-user/` skeleton crate: how to write
  and run your own programs and library OSes.
- [ROADMAP.md](ROADMAP.md), this changelog, and [CONTRIBUTING.md](CONTRIBUTING.md).

### Changed
- Programs are loaded from real ELF images instead of build-time flat
  binaries; the boot log announces the loaded image size.
- The user stack moved into its own address window, disjoint from the image
  and frame-map windows.
- User crates link as static non-PIE `ET_EXEC` with page-aligned segments
  (`-no-pie` plus a page-aligning linker script).

### Removed
- The flat-binary extraction step in `xtask` (and its `object` dependency).
- The `.text.entry` linker hack: the kernel honors `e_entry`, so `_start`
  no longer needs to be forced to the start of the image.

## [2.0.0] - 2026-06-13

### Added
- CPU time as a capability: a per-process budget spent with `cpu_charge`,
  enforced by the kernel (overdraw terminates and reclaims the process).
- Self-paging: a process can register a ring-3 page-fault handler and
  resolve faults in its lazy window with ordinary syscalls.
- Per-process address spaces: each process runs in its own page tables.
- `spawn`: launch a child in an isolated address space with a capability
  transferred into it; synchronous, depth-bounded, with the child's exit
  code returned to the parent.

## [1.0.0] - 2026-06-10

### Added
- Initial public release: a uniprocessor exokernel that boots under QEMU
  with a serial console, a bitmap frame allocator, capability tables, and a
  small syscall surface.
- The core demonstration: one workload over two library OSes (bump vs.
  free-list allocation), plus a fault-isolation demo (a process faults; the
  kernel logs it, reclaims it, and continues).
- `xtask` build/run/test harness with a `[PASS]`/`[SUITE]` serial protocol
  and headless QEMU CI.
