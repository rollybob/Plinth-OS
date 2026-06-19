# Plinth ABI v2.3

This is the contract between a Plinth program and the kernel: the call
interfaces, the capability model, the executable format, and the state a
process starts in. It is what you build against if you write your own program
or library OS.

`libplinth` is the reference implementation of the program side of this
contract (a thin shim, deliberately *not* a library OS). You do not have to
use it; anything that honors the ABI below runs.

## What changed since v1

v2 adds inter-process communication and concurrency, and revises one v1 call:

- **New: synchronous IPC** -- endpoints, `send`/`recv`, capability transfer
  through messages, and `call`/`reply` RPC (the "IPC interface" section). These
  enter through a software-interrupt gate, not `syscall`. An IPC operation
  returns a **status** separately from its **payload** (status in `RAX`, payload
  in `RSI`), so a peer-controlled message word can never be mistaken for an
  error -- including the `IPC_PEER_DIED` status that frees a process blocked on
  a dead counterpart.
- **New: capability kinds** -- `Endpoint` and `Reply`, with `SEND`/`RECV`
  rights; and `BlockRange`, naming a run of disk sectors, with `READ`/`WRITE`.
- **New: block storage** -- a `block_read` syscall reads disk sectors, named
  *relative* to a `BlockRange` capability, into a frame the device DMAs into.
  Like IPC, it returns a **status** word (the data lands in the frame), so no
  read-back value can be mistaken for an error.
- **Changed: `spawn`** no longer runs a child synchronously and returns its
  exit code. It launches the child as an independent, concurrently scheduled
  process and returns a *wait handle*; the child reports results over IPC. This
  is the one incompatible change from v1.

### v2.1 (additive)

- **New: `spawn_from_buffer`** launches a child from an ELF image the caller
  holds in its own mapped memory, rather than from the kernel's embedded set.
  This is the load-from-disk path: a filesystem library OS reads a program off a
  block device and launches it. The buffer is untrusted input and flows through
  the same ELF validator as every embedded binary. Embedded `spawn`-by-id stays
  as the built-in bootstrap loader. Purely additive -- no v2 call changed.
- **`BlockRange` now names a device.** A range is `(dev, start, count)`: the
  device index is part of the capability, so a holder cannot reach another
  device's blocks any more than another range's. `block_read`'s arguments are
  unchanged (the device rides in the capability, not the call).

### v2.2 (additive)

- **New: console input** -- an `EventSource` capability (`READ`) names an input
  device, and `event_recv` reads the next raw event from it, blocking until one
  arrives. Like the IPC ops it enters through the software-interrupt gate (a
  blocking read needs the same resumable trap frame) and returns a **status**
  (`RAX`) separate from the **event** (`RSI`). The kernel ships raw scancodes;
  turning them into characters is library-OS policy. Purely additive.

### v2.3 (block_read moves to the gate)

- **`block_read` is now a blocking call on the `int 0x80` gate (op 5), not
  syscall nr 10.** It always returned a status word and named the same
  capabilities; what changed is that the read now BLOCKS until the device
  completes the I/O, and the issuing process is suspended so others run
  meanwhile (the disk completion interrupt wakes it). A blocking call needs the
  resumable trap frame the gate saves, so `block_read` joined the IPC ops and
  `event_recv` there -- exactly the same move, for exactly the same reason.
  Syscall nr 10 is retired (unused). The arguments, the relative-sector
  addressing, and the `BLK_OK`/`BLK_E_*` status are all unchanged; only the
  entry mechanism (and the now-asynchronous wait) differ. A caller that used
  `libplinth::sys_block_read` is unaffected -- the wrapper hides the change.

## Syscall interface

The non-blocking calls use the `syscall`/`sysretq` instructions:

- The syscall number goes in `RAX`; arguments in `RDI`, `RSI`, `RDX` (every
  syscall takes at most three); the return value comes back in `RAX`.
- The `syscall` instruction clobbers `RCX` and `R11`; the kernel's
  dispatcher may clobber the caller-saved registers `R8`-`R10` and the
  argument registers. A caller must treat all of `RDI`, `RSI`, `RDX`,
  `RCX`, `R8`, `R9`, `R10`, `R11` as clobbered.
- The error sentinel is `SYS_ERR = 0xFFFF_FFFF_FFFF_FFFF` (`u64::MAX`).

| Nr | Name         | Args (RDI, RSI, ...)  | Returns                          |
|----|--------------|-----------------------|----------------------------------|
| 1  | write        | ptr, len              | bytes written, or `SYS_ERR`      |
| 2  | exit         | code                  | does not return                  |
| 3  | frame_alloc  | --                    | capability slot, or `SYS_ERR`    |
| 4  | frame_map    | slot, vaddr           | 0, or `SYS_ERR`                  |
| 5  | frame_free   | slot                  | 0, or `SYS_ERR`                  |
| 6  | cpu_charge   | slot, amount          | remaining budget, or terminates  |
| 7  | fault_reg    | entry, stack_top      | 0, or `SYS_ERR`                  |
| 8  | fault_return | --                    | resumes the faulting instruction |
| 9  | spawn        | child_id, transfer    | wait handle, or `SYS_ERR`        |
| 11 | spawn_from_buffer | buf_va, len, transfer | wait handle, or `SYS_ERR`   |

(Nr 10, `block_read`, was retired in v2.3: it is now a blocking op on the
`int 0x80` gate -- see the IPC interface table below.)

Notes:

- **write** copies `len` bytes from a user buffer to the console. Every
  page touched must be mapped and user-accessible, or the call returns
  `SYS_ERR`; `len` is capped at 4096. A single `write` is delivered
  atomically with respect to other processes, so a line emitted in one call is
  never interleaved with another process's output.
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
  process (there is no recoverable error for overdraw).
- **fault_reg** / **fault_return** are the self-paging pair: register a
  ring-3 page-fault handler (`entry`, running on `stack_top`), and return
  from it to retry the faulting instruction. A not-present fault in the
  lazy window is delivered to the handler instead of terminating the
  process. Both arguments must be non-zero. A fault inside the handler is
  unhandleable and terminates the process.
- **spawn** launches the embedded program `child_id` (an index into the
  kernel's spawnable set) as an independent, concurrently scheduled process,
  and returns a *wait handle*: the slot of a receive capability on a fresh
  result channel the kernel creates for this spawn. The child receives the
  matching **send** capability at `ENDPOINT_SLOT`. `transfer` optionally moves
  one capability out of the caller's table into the child's (landing right
  after the child's endpoint capability); pass `NO_CAP` (`u64::MAX`) for none.
  `spawn` does not block -- the child runs alongside the caller. To wait for
  and collect the child's result, `recv` the handle (see IPC); that recv is
  the join. Returns `SYS_ERR` if the child could not be created.
- **spawn_from_buffer(buf_va, len, transfer)** is `spawn` for a binary the
  caller already holds: `len` bytes at `buf_va` in the caller's address space
  are the child's ELF image, instead of an embedded `child_id`. This is how a
  filesystem library OS launches a program it read off disk. `buf_va` must be
  page-aligned, the whole `[buf_va, buf_va+len)` range must lie in the Map
  window and be mapped, and `len` must not exceed the kernel's image ceiling
  (256 KiB) -- otherwise `SYS_ERR`. The bytes are untrusted: the kernel runs
  them through the same ELF validator as every embedded binary. The result
  channel, `ENDPOINT_SLOT` send capability, `transfer`, and the returned wait
  handle all behave exactly as for `spawn`. Embedded `spawn`-by-id is not
  retired -- it remains the built-in bootstrap loader.

## IPC interface

Endpoints are synchronous rendezvous points. The blocking IPC operations enter
through a **software-interrupt gate**, `int 0x80`, rather than `syscall` -- a
blocking call must save and restore the full register state so the kernel can
suspend and later resume it, which the `syscall` fast path does not do. The
convention mirrors the syscall one:

- The operation selector goes in `RAX`; arguments in `RDI`, `RSI`, `RDX`.
- Results come back as a **status in `RAX`** -- `IPC_OK = 0`,
  `IPC_PEER_DIED = 2`, or `IPC_ERR = 1` (bad slot or missing right) -- the
  **message payload in `RSI`** (`recv`/`call`), and the transferred-capability
  slot in `RDX` (`recv`). The payload and cap slot are meaningful only when the
  status is `IPC_OK`. Splitting status from the payload means no message word,
  not even `u64::MAX`, can be mistaken for an error or a dead peer.
- The handler returns via `iretq`, which restores every register except the
  result registers; for forward compatibility treat `RCX`, `R8`-`R11` (and
  `RDX` where it is not a documented result) as clobbered.
- `NO_CAP = u64::MAX` is the `RDX` "no capability was transferred" sentinel.

| Op (RAX) | Name  | Args (RDI, RSI, RDX, RCX) | Returns (RAX status, RSI, RDX)         |
|----------|-------|---------------------------|----------------------------------------|
| 0        | send  | ep_slot, msg, cap_slot    | status                                 |
| 1        | recv  | ep_slot                   | status; msg in RSI; cap slot/`NO_CAP` RDX |
| 2        | call  | ep_slot, req              | status; reply word in RSI              |
| 3        | reply | reply_slot, msg           | status                                 |
| 4        | event_recv | source_slot          | status; packed event in RSI            |
| 5        | block_read | range, frame, sec, count | status (`BLK_OK` / `BLK_E_*`)     |

`event_recv` (console input) and `block_read` (block storage) are not IPC, but
they share this gate because a blocking read needs the same resumable trap frame
the IPC ops do. `block_read` is the one gate op with a fourth argument, `count`
in `RCX`. They are documented under "Console input" and "Block storage" below.

Notes:

- An **endpoint** carries one machine word per message, plus optionally one
  transferred capability. Bulk data is meant to ride a shared frame whose
  capability you transfer, not the word.
- **send(ep_slot, msg, cap_slot)** requires `RIGHT_SEND` on the endpoint
  capability at `ep_slot`. It blocks until a receiver takes the message. If
  `cap_slot` is not `NO_CAP`, the capability there is transferred to the
  receiver: it is revoked from the sender (and, if it is a mapped frame,
  unmapped here -- the capability and the access leave together), and minted
  into the receiver, which learns its slot from `recv`. Returns the status in
  `RAX` (`IPC_OK`, or `IPC_ERR` for a bad slot or missing right).
- **recv(ep_slot)** requires `RIGHT_RECV`. It blocks until a sender arrives,
  then returns `IPC_OK` in `RAX`, the message word in `RSI`, and in `RDX` the
  slot where a transferred capability landed (or `NO_CAP` if none). A `recv`
  that picks up a `call` instead returns a one-shot **reply capability** in
  `RDX` -- use it with `reply`. A non-`IPC_OK` status (`IPC_PEER_DIED` if the
  only counterpart died, `IPC_ERR` for a bad slot/right) means no message: the
  `RSI`/`RDX` values are not valid.
- **call(ep_slot, req)** requires `RIGHT_SEND`. It sends a request and blocks
  for a reply, returning `IPC_OK` in `RAX` and the reply word in `RSI`. The
  kernel mints the receiving server a one-shot reply capability naming this
  caller; the caller stays blocked until the server `reply`s -- or is woken
  with `IPC_PEER_DIED` if the server dies holding the reply capability.
- **reply(reply_slot, msg)** wakes the caller named by the one-shot reply
  capability at `reply_slot` (which `recv` returned), delivering `msg` as the
  caller's `call` result, and consumes the capability. No endpoint right is
  needed -- holding the reply capability is the authority, so a receive-only
  server can still answer. Returns the status in `RAX` (`IPC_OK`, or `IPC_ERR`
  if the slot is not a live reply capability).

A program creates its own endpoints only indirectly so far: the kernel makes
one per `spawn` (the result channel) and may grant one at launch. A
process-facing endpoint-create call is not yet part of the ABI.

## Console input

Input is delivered as raw events from capability-named **event sources**. An
`EventSource` capability (`RIGHT_READ`) names one input device; `event_recv`
reads the next event from it. The call enters through the same `int 0x80` gate
as IPC (a blocking read needs the resumable trap frame) under op selector 4.

- **event_recv(source_slot)** requires `RIGHT_READ` on the `EventSource`
  capability at `source_slot`. It returns the next event from that source --
  immediately if one is queued, otherwise blocking until one arrives (the kernel
  idles waiting for input). Returns `EVENT_OK = 0` in `RAX` with the **packed
  event in `RSI`**, or `EVENT_ERR = 1` (bad slot, not an event source, or
  missing read right). The status/payload split means no event word can be
  mistaken for an error.
- The event is **raw**: `RSI` packs the kind in bits 0..8, a device code in
  8..24, and a value in 24..32. For the keyboard (the only source today, id 0),
  the kind is `EVENT_KEY`, the code is the raw Set-1 scancode byte, and the
  value is the make/break bit (1 = press). The kernel does no keymap
  translation -- turning scancodes into characters and handling layouts,
  modifiers, and line editing is library-OS policy.
- One reader per source today: the process the kernel grants the source
  capability to. Fanning input out to many consumers is itself a library OS
  over this primitive.

## Block storage

A `BlockRange` capability (`RIGHT_READ`) names a run of `(dev, start, count)`
512-byte sectors on a block device; `block_read` reads from it into a frame the
device DMAs into. The call enters through the same `int 0x80` gate as IPC under
op selector 5 (a blocking read needs the resumable trap frame), with the one
fourth argument the gate ever takes, `count`, in `RCX`.

- **block_read(range, frame, sector_off, count)** reads `count` 512-byte sectors
  -- starting `sector_off` sectors into the `BlockRange` capability at slot
  `range` -- into the frame named by slot `frame`. It **blocks** until the device
  completes the I/O (other processes run meanwhile; the disk completion interrupt
  wakes the caller). The device DMAs the data into the frame, so map that frame
  (`frame_map`) to read the bytes; the frame capability must carry `RIGHT_WRITE`
  (`frame_alloc` grants it). Sectors are named **relative** to the range -- the
  kernel adds the range's start -- so a holder can never address blocks outside
  its grant. The result is a *status* word in `RAX`, not a data value (the data
  is in the frame): `BLK_OK = 0` on success, or `BLK_E_BADARG = 1` (zero count or
  a count larger than one frame), `BLK_E_RANGE = 2` (the request falls outside
  the range -- the multiplexing guarantee), `BLK_E_RIGHTS = 3` (bad slot, wrong
  kind, or missing right), or `BLK_E_DEV = 4` (device error). A `block_write`
  counterpart is not yet part of the ABI -- the first filesystem is read-only.

## Capabilities

Every grant is an unforgeable, kernel-held record that the holder may perform
some operations on some resource. Userspace names its capabilities by slot
index into a per-process table; the records never leave the kernel. Kinds:

| Kind     | Resource                         | Rights                        |
|----------|----------------------------------|-------------------------------|
| Frame    | one physical frame               | `READ`, `WRITE`, `MAP`        |
| CpuTime  | a spendable CPU-tick budget      | `CONSUME`                     |
| Endpoint   | an IPC rendezvous channel        | `SEND`, `RECV`              |
| Reply      | one-shot authority to reply once | (none -- holding it suffices) |
| BlockRange | `count` sectors on a device      | `READ`, `WRITE`            |
| EventSource | one input device's event stream | `READ`                     |

Rights are checked at use, not at transfer. `Reply` capabilities are minted by
the kernel (on receiving a `call`) and consumed on use; you cannot create one. A
`BlockRange` names sectors `[start, start+count)` on a specific block device
`dev`; the holder addresses them relative to `start` (offset 0 is the first
sector of the grant), and the kernel refuses any access beyond `count` or onto
another device -- so disjoint ranges handed to different library OSes cannot
reach each other's blocks, and a range on one disk cannot read another. An
`EventSource` names one input device (id 0 = keyboard); a holder reads only the
source it was granted, never another.

### Well-known initial capabilities

A few slots are well-known, the way file descriptor 0 is on Unix:

- `CPU_CAP_SLOT = 0` -- the CPU-time budget minted for every process. Pass it
  to `cpu_charge`.
- `GRANT_SLOT = ENDPOINT_SLOT = BLOCK_SLOT = 1` -- the first capability the
  kernel grants a process after its CPU budget. For a spawned child this is the
  **send** capability on its parent's result channel (use it to report a
  result); for a process the kernel launches with an endpoint, it is that
  endpoint capability; for one launched with disk access, it is a `BlockRange`.
  A capability moved in via `spawn`'s `transfer` argument lands in the next slot
  after this one.

All other slots start empty.

## Virtual-address windows

| Window      | Range                       | Purpose                          |
|-------------|-----------------------------|----------------------------------|
| Image       | `0x0040_0000`-`0x0F00_0000` | where PT_LOAD segments must live |
| Stack       | top at `0x0FF0_0000`        | kernel-provided; grows down      |
| Map         | `0x1000_0000`-`0x2000_0000` | `frame_map` target addresses     |
| Lazy        | `0x1800_0000`-`0x1900_0000` | self-paged faults (within Map)   |

`frame_map` accepts any page-aligned address in the Map window. A
not-present access in the Lazy sub-window, once a handler is registered, is
delivered to that handler rather than killing the process.

## Exit codes

A program's exit code is a 32-bit value (`exit(code)` keeps the low 32
bits). The kernel reserves sentinels above the 32-bit range for its own
"faulted" / "out-of-budget" termination signals; those never appear as a
normal exit code. Note that since v2 a process's exit code is *not* delivered
to any other process -- `spawn` is asynchronous, and a child reports results
over IPC, not through its exit code.

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
- **No stack arguments are passed** -- there is no `argc`/`argv`/`envp` and no
  auxiliary vector. A program receives its inputs through calls and the
  capabilities it holds.
- The process starts holding its CPU-time capability at `CPU_CAP_SLOT`, plus
  whatever the kernel granted at `GRANT_SLOT`/`ENDPOINT_SLOT` (and, for a
  spawned child, any `transfer`ed capability in the next slot). All other
  slots are empty.

One register *is* defined at entry, for processes the kernel runs under its
scheduler -- which includes every process created by `spawn` and the kernel's
multi-instance program sets: **`RDI` holds the process's scheduler slot index**
(an integer), so several copies of one program can tell themselves apart
(`_start` reads it as its first C-ABI argument). For a single spawned child the
value is just its slot and is usually ignored. A portable program that needs a
stable identity should arrange to receive one over IPC rather than rely on this
number; a program that does not need it ignores `RDI`.
