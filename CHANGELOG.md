# Changelog

All notable changes to Plinth are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project aims to
follow semantic versioning. ABI v1 (see [ABI.md](ABI.md)) is frozen:
released versions do not break it.

## [Unreleased]

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
