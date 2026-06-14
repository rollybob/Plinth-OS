# Plinth ABI v1

This is the stable contract between a Plinth program and the kernel: the
syscall interface, the executable format, and the state a process starts
in. It is what you build against if you write your own program or library
OS. Version 1 is frozen -- new capabilities will be added without breaking
what is documented here.

`libplinth` is the reference implementation of the program side of this
contract (a thin syscall shim, deliberately *not* a library OS). You do not
have to use it; anything that honors the ABI below runs.

## Syscall interface

Calls use the `syscall`/`sysretq` instructions:

- The syscall number goes in `RAX`; arguments in `RDI`, `RSI`, `RDX`; the
  return value comes back in `RAX`.
- The `syscall` instruction clobbers `RCX` and `R11`; the kernel's
  dispatcher may clobber the caller-saved registers `R8`-`R10` and the
  argument registers. A caller must treat all of `RDI`, `RSI`, `RDX`,
  `RCX`, `R8`, `R9`, `R10`, `R11` as clobbered.
- The error sentinel is `SYS_ERR = 0xFFFF_FFFF_FFFF_FFFF` (`u64::MAX`).

| Nr | Name         | Args (RDI, RSI)   | Returns                          |
|----|--------------|-------------------|----------------------------------|
| 1  | write        | ptr, len          | bytes written, or `SYS_ERR`      |
| 2  | exit         | code              | does not return                  |
| 3  | frame_alloc  | --                | capability slot, or `SYS_ERR`    |
| 4  | frame_map    | slot, vaddr       | 0, or `SYS_ERR`                  |
| 5  | frame_free   | slot              | 0, or `SYS_ERR`                  |
| 6  | cpu_charge   | slot, amount      | remaining budget, or terminates  |
| 7  | fault_reg    | entry, stack_top  | 0, or `SYS_ERR`                  |
| 8  | fault_return | --                | resumes the faulting instruction |
| 9  | spawn        | child_id, slot    | child exit code, or `SYS_ERR`    |

Notes:

- **write** copies `len` bytes from a user buffer to the console. Every
  page touched must be mapped and user-accessible, or the call returns
  `SYS_ERR`; `len` is capped at 4096.
- **frame_alloc** allocates one physical frame and mints a capability for
  it in the calling process's table, returning the slot.
- **frame_map** maps the frame named by `slot` at a *user-chosen*
  page-aligned virtual address inside the map window (below). The kernel
  validates the capability, alignment, and window; placement is the
  program's choice. This is the core exokernel move.
- **frame_free** unmaps (if mapped), revokes, and frees the frame at
  `slot`. Aimed at a non-frame slot it fails without disturbing it.
- **cpu_charge** debits `amount` ticks from the CPU-time capability at
  `slot` and returns the remaining budget. Charging more than remains is
  consuming a resource you no longer hold: the kernel terminates the
  process (there is no recoverable error for overdraw). Enforcement is
  cooperative -- a process that never charges is never billed.
- **fault_reg** / **fault_return** are the self-paging pair: register a
  ring-3 page-fault handler (`entry`, running on `stack_top`), and return
  from it to retry the faulting instruction. A not-present fault in the
  lazy window is delivered to the handler instead of terminating the
  process. Both arguments must be non-zero. A fault inside the handler is
  unhandleable and terminates the process.
- **spawn** runs an embedded child program to completion in its own
  isolated address space, transferring the capability at `slot` out of the
  caller's table into the child's (where it lands at `GRANT_SLOT`). Returns
  the child's exit code, or `SYS_ERR` if it faulted, overran its budget, or
  could not be started. Spawn is synchronous (the parent blocks) and
  depth-bounded.

### Well-known initial capabilities

A process's capability slots are indices into a kernel-held table. Two
slots are well-known, the way file descriptor 0 is on Unix:

- `CPU_CAP_SLOT = 0` -- the CPU-time budget minted for every process at
  spawn. Pass it to `cpu_charge`.
- `GRANT_SLOT = 1` -- present only in a spawned child: the capability the
  parent transferred in via `spawn`.

### Virtual-address windows

| Window      | Range                       | Purpose                          |
|-------------|-----------------------------|----------------------------------|
| Image       | `0x0040_0000`-`0x0F00_0000` | where PT_LOAD segments must live |
| Stack       | top at `0x0FF0_0000`        | kernel-provided; grows down      |
| Map         | `0x1000_0000`-`0x2000_0000` | `frame_map` target addresses     |
| Lazy        | `0x1800_0000`-`0x1900_0000` | self-paged faults (within Map)   |

`frame_map` accepts any page-aligned address in the Map window. A
not-present access in the Lazy sub-window, once a handler is registered, is
delivered to that handler rather than killing the process.

### Exit codes

A program's exit code is a 32-bit value (`exit(code)` keeps the low 32
bits). The kernel reserves sentinels above the 32-bit range to report a
faulted or out-of-budget termination; those are surfaced as `SYS_ERR` to a
spawning parent, never as a normal exit code.

## Executable format

A Plinth program is a **static, non-PIE `ET_EXEC` ELF64** for x86-64:

- Little-endian, `EM_X86_64`, `ET_EXEC`. A PIE (`ET_DYN`) is rejected --
  link with `-no-pie`.
- The kernel maps `PT_LOAD` segments verbatim at their `p_vaddr` and
  applies **no relocations**. `PT_INTERP` (dynamic linking) is rejected;
  other non-`PT_LOAD` headers are ignored.
- Each `PT_LOAD` segment must be **page-aligned** and lie entirely within
  the image window. Align section groups in your linker script so the
  linker emits separate, page-aligned segments.
- **W^X is enforced per page**: a segment may be writable or executable,
  not both. Text and read-only data must not share a page with writable
  data. Each segment is mapped with exactly the permissions in its
  `p_flags` (executable iff `PF_X`, writable iff `PF_W`).
- The image, plus the stack, must fit in a fixed page budget (currently 64
  pages total); oversize images are rejected.

The reference `*-user` crates do this with a `build.rs` that passes
`-no-pie` and a `linker.ld` that page-aligns `.text`, `.rodata`, and
`.data` (see any `*-user/` crate as a template).

## Process entry state

- Control enters at the ELF's `e_entry`, in ring 3.
- `RSP` points at the top of a fresh, zeroed, non-executable stack.
- **No arguments are passed** -- there is no `argc`/`argv`/`envp` and no
  auxiliary vector. A program receives its inputs through syscalls and the
  capabilities it holds.
- The process starts holding its CPU-time capability at `CPU_CAP_SLOT`,
  plus one transferred capability at `GRANT_SLOT` if it was spawned with a
  grant. All other slots are empty.
