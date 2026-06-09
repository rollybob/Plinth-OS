# Plinth

A toy exokernel in Rust, built to demonstrate the exokernel idea in the
smallest codebase that makes it visible.

A plinth is the bare slab a column stands on: it carries the load and
imposes nothing about what is built above it. That is the exokernel
contract. The kernel owns the hardware and multiplexes it securely --
physical memory frames, CPU time, communication channels -- but refuses
to define what a process's world looks like. No files, no malloc, no
process model beyond isolation. Every abstraction traditionally baked
into a kernel is instead implemented by an unprivileged *library OS*
linked into each application, and two applications on the same kernel
can make entirely different choices.

The design follows the exokernel literature (Engler, Kaashoek, and
O'Toole's 1995 SOSP paper "Exokernel: An Operating System Architecture
for Application-Level Resource Management"), scaled down to something
readable in an afternoon.

## Status

Early. Current state:

- [x] UEFI boot (bootloader 0.11 + OVMF), serial console, clean QEMU exit
- [x] Smoke-test harness (`cargo xtask smoke`) asserting on boot output
- [x] Physical frame allocator (bitmap) and capability tables; frame
      ownership is modeled as a mint/lookup/revoke cycle, kernel-side
- [x] In-kernel test suite running under QEMU (`cargo xtask test`)
- [ ] Ring-3 processes and a ~6-call syscall surface exposing frames
      to userspace through those capabilities
- [ ] Two demo library OSes with different memory-management policies
      on the same kernel
- [ ] Fault-isolation demo: one process crashes, the others keep running
- [ ] CI running the full suite in QEMU

## Requirements

- Rust nightly (pinned via `rust-toolchain.toml`; needs `rust-src` for
  `build-std`)
- `qemu-system-x86_64` on PATH

## Build and run

```
cargo xtask run     # build kernel + UEFI disk image, boot in QEMU
cargo xtask smoke   # boot with captured output, verify expected_boot_log.txt
cargo xtask test    # build with --features tests, run the in-kernel suite
cargo xtask run-gdb # boot paused, GDB server on :1234
```

The first build downloads OVMF firmware into `target/ovmf/` (cached
afterwards) and compiles the bootloader, which takes a few minutes.

## Layout

```
kernel/   the exokernel itself (no_std, x86_64-unknown-none)
xtask/    build orchestration: disk image, QEMU invocation, smoke harness
```
