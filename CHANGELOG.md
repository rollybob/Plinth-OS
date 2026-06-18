# Changelog

All notable changes to Plinth are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project aims to
follow semantic versioning. The ABI (see [ABI.md](ABI.md)) is versioned; the
current contract is **v2**. v2 adds IPC and revised `spawn`, breaking v1 --
the one incompatible ABI change so far; later additions will not break v2
within a major series.

## [Unreleased]

### Added
- Block storage (Phase 2 close), in three pieces. The kernel brings up a
  **virtio-blk modern (virtio-pci) device** (`kernel/src/pci.rs`,
  `kernel/src/virtio_blk.rs`): PCI enumeration over legacy config space
  (0xCF8/0xCFC), a mapped MMIO BAR, modern feature negotiation, one split
  virtqueue, and a bounded polled block read. The disk is multiplexed through a
  new **`BlockRange` capability** (`READ`/`WRITE`) naming a run of 512-byte
  sectors; a **`block_read` syscall** reads sectors -- named relative to the
  range, so a holder can never reach blocks outside its grant -- into a
  caller-owned frame the device DMAs into, returning a status word (the data is
  in the frame, so no read-back value can be mistaken for an error). A
  `blk-user` demo reads a sector through a granted sub-range and is denied a read
  past it (the multiplexing guarantee); `xtask smoke` verifies the read-back
  bytes against a deterministic image and that the I/O frame returns to baseline.
  The null physical frame is now reserved (it can never be a valid allocation,
  and it keeps a DMA ring off guest-physical 0).
- Synchronous IPC: capability-named endpoints (`kernel/src/ipc.rs`).
  `send`/`recv` are a bufferless rendezvous; a message can transfer a
  capability (a zero-copy frame handoff -- the receiver maps the same physical
  frame); `call`/`reply` is request/response RPC, where the server answers via
  a one-shot reply capability and needs no send right of its own. Blocking
  operations enter through a software-interrupt gate so they reuse the
  scheduler's context-switch path. An IPC operation returns a status (`RAX`)
  separately from its message payload (`RSI`), so a peer-controlled word can
  never be mistaken for an error.
- `spawn` is reconciled with the scheduler: it launches the child as an
  independent scheduled process and returns a handle (a receive capability on
  a result channel); the parent waits by `recv`-ing it. This removed the old
  synchronous spawn nesting (per-depth syscall stacks and depth limit).
- IPC / scheduler liveness hardening: per-endpoint capability reference counts
  reclaim an endpoint table slot once no capability can reach it (fixing the
  bounded-table leak where every `spawn` minted an endpoint that was never
  freed), and a process blocked on a peer that dies is now woken with
  `IPC_PEER_DIED` instead of hanging -- via a death-time reaping pass plus a
  block-time liveness check (so a counterpart that dies either before or while
  you wait is handled). `spawn_and_wait` surfaces a crashed child as a status,
  not a hang. The wait queue was extracted into a pure, unit-tested structure.

### Changed
- **ABI v2** (see [ABI.md](ABI.md)): `spawn` no longer returns the child's exit
  code synchronously -- it returns a wait handle and the child reports results
  over IPC. The IPC operations and the `Endpoint`/`Reply` capability kinds are
  also new in v2. This breaks ABI v1.

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
