# Writing programs and library OSes for Plinth

Plinth runs your code in ring 3 over a tiny syscall interface. This guide
walks through writing a program, running it, and -- the part that makes
Plinth an exokernel -- putting your own memory policy in a library OS
instead of the kernel.

The contract every program builds against is [ABI.md](ABI.md); this guide
is the hands-on walkthrough alongside it.

## The seam

```
  your program          what you write
  -----------           --------------
  libos (policy)        a library OS: turns raw frames into an abstraction
  libplinth (mechanism) the thin syscall shim -- NOT a library OS
  =================  syscall/sysret
  plinth kernel         mechanism only: frames, capabilities, address spaces
```

The kernel multiplexes hardware securely and refuses to define what an
abstraction *is*. `libplinth` is just the raw syscalls. Everything that
looks like an allocator, a heap, or a memory manager is a **library OS** --
ordinary unprivileged code your program chooses and links. Two programs on
the same kernel can answer "what is memory management" differently.

## 1. A minimal program

Start from `template-user/` -- copy the whole crate directory and rename
it `<yourname>-user`:

```text
template-user/
  Cargo.toml          package + libplinth dependency, panic = "abort"
  .cargo/config.toml  build-std (core) for the bare-metal target
  build.rs            passes the linker script and -no-pie
  linker.ld           load address + page-aligned segments
  src/main.rs         _start + panic handler
```

The program itself is small:

```rust
#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_write};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    sys_write(b"hello from my program\n");
    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(255)
}
```

What each piece is for:

- **`#![no_std]` / `#![no_main]`** -- there is no Rust runtime, no libc, no
  `main`. Your entry point is `_start`.
- **`_start`** -- the kernel enters here in ring 3 with a fresh stack and
  no arguments (no `argc`/`argv`). It can never return, so it ends in
  `sys_exit`. `#[no_mangle]` keeps the symbol named `_start`, which the
  linker names as the ELF entry.
- **panic handler** -- mandatory in `#![no_std]`. With nothing to unwind
  into, exit with a marker code.
- **`libplinth`** -- the syscall wrappers (`sys_write`, `sys_frame_alloc`,
  `sys_frame_map`, ...) plus the C memory intrinsics the compiler expects.

### Why the build setup looks like that

Plinth's loader accepts a **static, non-PIE `ET_EXEC`** with page-aligned,
W^X segments (see ABI.md). Two settings make the toolchain produce exactly
that, and they are the only non-obvious part of the build:

- **`build.rs` passes `-no-pie`.** The `x86_64-unknown-none` target emits a
  position-independent executable (`ET_DYN`) by default; the loader rejects
  that. `-no-pie` makes it a true `ET_EXEC`.
- **`linker.ld` page-aligns each section group** (`. = ALIGN(0x1000)`
  before `.rodata` and `.data`). The linker otherwise packs `.text`,
  `.rodata`, and `.data` into shared pages, which breaks per-page W^X. The
  load address (`. = 0x400000`) must stay inside the image window.

Do not set these via `RUSTFLAGS` or `.cargo` config `rustflags`: that
recompiles `core` through build-std. Keep `-no-pie` in `build.rs`, where it
applies only to your binary.

Build it:

```text
cd <yourname>-user
cargo build --release
# -> target/x86_64-unknown-none/release/<yourname>-user  (a static ET_EXEC)
```

## 2. Running your program

Plinth does not load programs from disk yet (that is Phase 2 -- see
[ROADMAP.md](ROADMAP.md)). For now a program runs by being **embedded in
the kernel image** and launched at boot. Wiring a new program in touches
three lists:

1. **`xtask/src/main.rs`** -- add your short name to `USER_CRATES` so xtask
   builds the crate.
2. **`kernel/build.rs`** -- add the same short name to `USER_BINARIES` so
   the crate's ELF path is exposed to the kernel as `<NAME>_BIN`.
3. **`kernel/src/main.rs`** -- add it to the `DEMOS` table:
   `("<name>-demo", include_bytes!(env!("<NAME>_BIN")))`. The boot loop
   runs each entry with `process::run`.

Then:

```text
cargo xtask run     # build everything and boot in QEMU
cargo xtask smoke   # boot and assert expected_boot_log.txt, in order
```

(If you assert your program's output, add its lines to
`expected_boot_log.txt`. The matcher is substring-based and in order.)

To launch a program as a **spawned child** instead -- in its own address
space, receiving a transferred capability -- add it to the kernel's
`SPAWNABLE` table and call `sys_spawn(child_id, slot)` from the parent. See
`spawner-user/` and `grantee-user/` for the pattern.

## 3. Using a library OS

`libplinth` is deliberately not a library OS -- it gives you raw frames,
not a heap. Memory policy lives in a `libos`. The shipped `libos/` crate
provides two: `BumpAlloc` and `FreeListAlloc`.

`bump-user/` shows the pattern -- a program that links a library OS and a
shared workload:

```rust
#![no_std]
#![no_main]

use libos::BumpAlloc;
use libplinth::sys_exit;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut policy = BumpAlloc::new();
    demo_app::run(&mut policy);   // the workload is generic over the policy
    sys_exit(0)
}
```

`list-user/` is the *same* `demo_app::run` over `FreeListAlloc`. Same
kernel, same syscalls, different memory management -- chosen in
unprivileged code. That contrast is the whole point of the project.

## 4. Writing your own library OS

A library OS is just a crate that turns the raw capability syscalls into
an abstraction. The minimum it does:

- call `sys_frame_alloc` to obtain a frame capability,
- call `sys_frame_map(slot, vaddr)` to place it at an address *it* chooses
  inside the map window,
- and impose whatever policy it wants on top -- bump vs. free-list reuse,
  alignment, sizing, when to ask the kernel for another frame.

Read `libos/src/lib.rs` for two complete examples and `demo-app/src/lib.rs`
for a workload written against a policy trait rather than a concrete
allocator. The kernel enforces only the mechanism (you can map a frame
only through a capability you hold, at an aligned address in the window);
*how* you manage memory above that is entirely yours.

## Where to look next

- [ABI.md](ABI.md) -- the syscall interface, executable format, and entry
  state, frozen as v1.
- `hello-user/` -- exercises the whole syscall surface end to end.
- `lazy-user/` -- registers a ring-3 page-fault handler (self-paging).
- `spawner-user/` + `grantee-user/` -- spawn and capability transfer.
