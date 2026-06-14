# Contributing to Plinth

Contributions are welcome. Plinth is a small, deliberately legible kernel;
the bar for changes is that they keep it that way.

## Building and testing

Requirements: Rust nightly (pinned by `rust-toolchain.toml`, needs the
`rust-src` component) and `qemu-system-x86_64` on `PATH`. The first build
downloads and caches OVMF firmware under `target/ovmf/`.

```text
cargo xtask run     # build everything and boot in QEMU
cargo xtask smoke   # boot, assert expected_boot_log.txt line by line
cargo xtask test    # in-kernel unit test suite, run under QEMU
cargo xtask check   # lint: syscall asm! blocks declare the full clobber set
```

All four must be green before a change is ready. The three test layers are
not optional:

- If you change behavior the boot log shows, update `expected_boot_log.txt`
  (the matcher is substring-based and in order).
- If you add or change a syscall, keep the `asm!` clobber declarations in
  `libplinth` correct -- `cargo xtask check` enforces this.
- New kernel logic should come with a test where it can be tested without
  userspace (the ELF parser, for example, is a pure function with a full
  suite in `kernel/src/tests/`).

## Conventions

- `#![no_std]` throughout; no kernel heap.
- Explicit over clever. Complexity has to earn its place -- if a feature
  can live in a library OS instead of the kernel, it should.
- Every `unsafe` block carries a comment justifying why it is sound.
- **ASCII only. No emoji, and no Unicode unless it is genuinely necessary**
  -- in code, comments, log strings, and docs.
- Match the surrounding code's naming, comment density, and idiom.

## The ABI is a contract

ABI v1 ([ABI.md](ABI.md)) is frozen. Do not change the syscall numbers,
argument or error conventions, executable format, or process entry state in
a way that breaks existing programs. New syscalls and capabilities are
added; old ones are not repurposed. If a change would alter ABI.md, raise
it as a discussion first.

## Pull requests

- Keep changes focused; one concern per PR.
- Explain *why*, not just *what* -- the reasoning is the part worth
  reviewing.
- By contributing, you agree your work is licensed under the project's MIT
  license (see [LICENSE](LICENSE)).
