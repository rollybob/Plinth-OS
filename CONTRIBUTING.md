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

All four must be green before a change is ready. CI
(`.github/workflows/ci.yml`) also runs the targeted boot lanes -- `smoke-amd`,
`smoke-smp`, `no-i8042`, `smoke-nostorage`, `smoke-nvme`, `smoke-fbcon-shell`,
`smoke-fbcon-panic`, `smoke-usb`, and `usb-key` (the README's Testing section
says what each covers) -- so run the ones your change touches. The test layers
are not optional:

- If you change behavior the boot log shows, update `expected_boot_log.txt`.
  The matcher is substring-based and in order, and it runs both ways: a new
  line that no expectation matches fails the smoke test too, unless it is
  covered by one of the `#!allow` patterns at the bottom of the file.
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

The current contract is ABI v2.12 ([ABI.md](ABI.md)); every version bump is
recorded there and in [CHANGELOG.md](CHANGELOG.md). Do not change the syscall
numbers, argument or error conventions, executable format, or process entry
state in a way that breaks existing programs. New syscalls and capabilities
are added; old ones are not repurposed. If a change would alter ABI.md, raise
it as a discussion first.

## Design-note references

Some code comments, commit messages, and `expected_boot_log.txt` annotations
cite design notes such as `real_hardware.md`, `usb_hid.md`, or `iommu.md`, and
milestone or issue labels such as `D3`, `Q5`, or `K-003`. These point to the
author's private design notes, not to files in this repository. The comment
around each reference is meant to stand on its own; if one does not, that is
a documentation bug worth reporting.

## Pull requests

- Keep changes focused; one concern per PR.
- Explain *why*, not just *what* -- the reasoning is the part worth
  reviewing.
- By contributing, you agree your work is licensed under the project's MIT
  license (see [LICENSE](LICENSE)).
