# Changelog

All notable changes to Plinth are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project aims to
follow semantic versioning. The ABI (see [ABI.md](ABI.md)) is versioned; the
current contract is **v2.6**. v2 added IPC and revised `spawn`, breaking v1 --
the one incompatible ABI change so far; v2.1 added `spawn_from_buffer` (the
load-from-disk path), v2.2 added console input (`event_recv` + `EventSource`),
both additive over v2; v2.3 moved `block_read` to the `int 0x80` gate so it can
block (hidden behind the `libplinth` wrapper); v2.4 replaces `block_read`
entirely with async completion rings (`ring_register`/`ring_submit`/`ring_wait`),
additive but for the retirement of the now-unused `block_read` op; v2.5 ports
input onto the same rings as multishot subscriptions (`RING_OP_EVENT_SUB`/
`RING_OP_CANCEL` SQ ops), retiring the `event_recv` gate op behind a shim -- one
ring, one `ring_wait`, now multiplexes block reads and input; v2.6 adds the
write half of the block ring ABI (`RING_OP_WRITE`), purely additive.

## [Unreleased]

### Added
- Broader hardware -- SMP scaling (per-core run queues + bounded work stealing).
  The scheduler replaces the single flat process table scan with a **per-core
  run queue** (`CORE_QUEUE`, `kernel/src/scheduler.rs`): a process is homed to a
  core at spawn time, and `pick_next` scans only the calling core's own queue.
  An idle core **steals** one ready process from a busy core's queue, lifting
  the pin from the SMP boot milestone -- migration is now a steal, and the D5
  shootdown-safety invariants were re-audited against the grown kernel. The
  single big kernel lock is deliberately left whole (lock-splitting earns
  nothing until a workload contends it near 100% kernel residency). A new
  **`steal demo`** (`stealer-user`/`stealwork-user`) forces an imbalance and
  asserts both that every worker completes and that a cross-core steal actually
  fired (a kernel steal counter); `cargo xtask smoke-smp` exercises it on 2-4
  cores. Scheduling scales per-core; kernel-entry throughput is unchanged. The
  ABI is untouched.
- Broader hardware -- APIC (Stages A1-A3). Interrupt delivery moved off the 8259
  PIC. A hand-rolled, bounded ACPI **MADT parser** (`kernel/src/acpi.rs`,
  modeled on the PCI enumerator -- no ACPI crate, no AML) discovers the CPU and
  interrupt-controller topology, and the `irq` seam now drives the **Local APIC
  + I/O APIC**, with the 8259 kept as a fallback when no MADT is present.
  virtio-blk completions moved to **MSI-X** (retiring the level-triggered INTx
  deassert dance) and the preemption tick to the **per-CPU LAPIC timer** (the
  PIT kept as a fallback). All still uniprocessor and byte-identical under
  `PLINTH_ICOUNT` -- the seam swapped underneath with nothing above it changing.
- Broader hardware -- SMP (Stages B1-B2). The kernel now boots and schedules on
  every CPU the MADT lists. Each application processor is woken with
  INIT-SIPI-SIPI and carried real -> protected -> long mode into Rust by a
  trampoline (`kernel/src/smp.rs`); per-CPU state (current process, kernel
  stacks) lives behind `IA32_GS_BASE` (`kernel/src/percpu.rs`; `gdt.rs` and
  `syscall.rs` rebuilt per-core); a **single big kernel lock**
  (`kernel/src/bkl.rs`, held entry-to-exit) serializes every shared kernel
  static, and a reschedule IPI wakes a halted core when work appears. A process
  is pinned to the core that first schedules it (no cross-core migration yet).
  The non-preemptible-kernel discipline narrows from "single CPU" to "per core,
  while holding the lock"; the ABI is unchanged -- SMP is entirely
  kernel-internal. A new `cargo xtask smoke-smp` reruns the smoke assertion
  battery on 2-4 cores (wired into CI); the `-smp 1` lane keeps the
  deterministic line-by-line contract as a regression net.
- Interrupt-driven blocking block I/O (storage Stage 4). `block_read` no longer
  busy-polls the device with interrupts off: the issuing process now goes
  **Blocked** and the CPU runs other processes (or idles) until the disk's
  completion interrupt wakes it -- the kernel-internal twin of the input wake
  primitive, reusing `block_current`/`wake_with`. Each virtio-blk device's INTx
  line (read from PCI config space) is routed through the same `irq` seam the
  keyboard uses, and its `ISR` register is read to ack the level-triggered line.
  Boot-time selftests stay polled (no process exists to block yet); the scheduler
  treats a process blocked on disk as a legitimate idle, not a deadlock. Output
  is byte-identical and verified under the `PLINTH_ICOUNT` determinism tripwire.
- Async completion rings for block I/O (ABI v2.4). Block I/O moves off a kernel
  entry per read onto shared-memory **submission/completion queues** the library
  OS shares with the kernel (io_uring-shaped). A new `kernel/src/rings.rs` adds a
  bound-ring capability (`CapObject::Ring`) and three calls: **`ring_register`**
  and **`ring_submit`** on the `syscall` fast path (nr 12/13), **`ring_wait`** on
  the `int 0x80` gate (op 6). The libOS writes logical, capability-named requests
  (a `BlockRange` slot, a frame slot, a sector offset -- never a physical
  address); `ring_submit` rings a doorbell and the kernel drains the queue in the
  submitter's context, running the same two cap-checks, translating each request
  into a virtqueue descriptor chain, and posting completions back from the MSI-X
  IRQ. The kernel stays the only writer of physical descriptor addresses, so the
  device is multiplexed across tenants with no IOMMU. Many reads can be in flight
  at once (depth `floor(qsize/3)`); the completion handler demuxes each back to
  its request by the descriptor head the device echoes. The internals were proved
  first as host-side unit tests of the demux, then under smoke one-in-flight.
- Reference async executor + many-in-flight demo. `libos` gains `ring`, a minimal
  `no_std` futures executor over the ring ABI (a read is a `Future`; the reactor
  matches completions to futures by an opaque cookie; `block_on` is the one place
  it blocks) -- library-OS *policy* over the kernel's *mechanism*, replaceable. A
  new `asyncblk-user` demo issues several reads that overlap on the device and
  asserts each landed in its own frame, the depth the single-shot `block_read`
  could not express. Completion order is the device's, so it is asserted, never
  transcript-matched.
- Input on the completion rings -- multishot event subscriptions (ABI v2.5).
  Input is *producer-initiated* (a keystroke answers no request), so it rides the
  ring as a **multishot subscription**, not a one-shot request: a new
  `RING_OP_EVENT_SUB` SQ op names an `EventSource` + a `user_data` cookie and arms
  a standing subscription, every event then posting a CQ completion (the packed
  event in `status`, the cookie in `user_data`) until `RING_OP_CANCEL`. The
  subscription-routing + CQ-full-backpressure core (`rings.rs` `Subscriptions`) is
  pure logic over a fixed pool, proved first as host unit tests
  (`tests/event_rings.rs`: source/cookie routing, drop-newest + sticky count,
  cancel, release, pool limits). One ring drained by one `ring_wait` now
  multiplexes block reads and input -- the unified event loop a real OS is built
  on. `libos`'s `ring` gains an event-stream adapter (a multishot stream over the
  same reactor, alongside the one-shot read future), and an `evtstream-user` demo
  subscribes and reaps a scripted scancode sequence, asserting each event arrives
  once and in order (assertion-based, never transcript-matched), then cancels.
  CQ-full backpressure -- drop the newest event + a drop flag the reader observes
  -- is the one rule the block path never needed (its CQ is sized to in-flight
  depth). The unified payoff: `libos` adds `join2` (a heterogeneous two-future
  join) and `EventStream::collect`, and a `unified-user` demo registers ONE ring,
  issues a block read AND a keyboard subscription on it, and drives both to
  completion in a single `block_on`/`ring_wait` loop -- the event loop a real OS
  is built on, with the kernel demuxing disk completions and key events back to
  their futures by `user_data` in one CQ.
- A second `EventSource` -- the PS/2 mouse on IRQ12 (`kernel/src/mouse.rs`),
  raw `dx`/`dy`/button packets as one packed `EVENT_MOUSE_MOVE` event -- proved
  the event-rings mechanism generalizes past one device with zero changes to
  `rings.rs` or the CQ-full backpressure logic. A `mouse-user` demo subscribes
  and reaps a scripted packet sequence, asserting each decodes correctly and in
  order, then cancels.
- Block writes -- the write half of the ring ABI (ABI v2.6). A new
  `RING_OP_WRITE` SQ op mirrors `RING_OP_READ`'s shape exactly, but with the
  two cap-checks' direction reversed: the `BlockRange` must carry `RIGHT_WRITE`
  (not `RIGHT_READ`), and the I/O frame must carry `RIGHT_READ` (not
  `RIGHT_WRITE`) -- the kernel reads the frame's existing contents to hand to
  the device, rather than writing into it. `virtio_blk::post_request` gained a
  `Direction` parameter selecting `VIRTIO_BLK_T_IN`/`VIRTIO_BLK_T_OUT` and
  whether the data descriptor is device-writable; `drain_completions` and the
  in-flight demux are unchanged (direction-agnostic). `libos`'s `ring` gains a
  `write` future mirroring `read`. A new `blkwrite-user` demo writes a fixed
  pattern to a granted range, reads the same range back into a separate frame,
  and asserts the bytes match what was written -- not the disk's original ramp
  content -- proving the write reached the device. It also holds a second,
  `RIGHT_READ`-only `BlockRange` and asserts a write through it is rejected
  with `BLK_E_RIGHTS` -- the negative case for the rights-direction check.
- A read-write filesystem library OS, `librwfs` (Design/readwrite_fs.md),
  built entirely on the block write path with **zero kernel or ABI change**
  -- every decision in it is library-OS policy over `block_read`/`block_write`,
  the exokernel argument applied one layer up from the read-only archive. A
  bitmap allocator (`bitmap.rs`, first-fit over a byte slice) and a
  fixed-maximum-entry mutable directory (`directory.rs`, the archive's
  zeroed-name-means-free convention, but now reusable) are both pure logic,
  host-tested like `libfs::archive`. `format.rs` wires them to real sectors
  via `libos::ring` (async-native from the start, no polled shim): the
  on-disk layout is a superblock + bitmap + directory metadata region
  (rewritten as one unit after every mutation) followed by a single-extent,
  fixed-size-at-creation data area, formatted fresh every run. A new
  `rwfs-user` demo creates two files, reads each back, deletes one, creates a
  third sized to need exactly the freed run, and asserts it landed at the
  deleted file's exact former sector -- proving the bitmap reclaims freed
  space rather than just hiding it -- then re-verifies the surviving file is
  untouched by the cycle.

### Changed
- **`block_read` moved from syscall nr 10 to the `int 0x80` gate (op 5), ABI
  v2.3.** A blocking call needs the resumable trap frame the gate saves (the same
  reason the IPC ops and `event_recv` live there); the `syscall` fast path cannot
  suspend and resume a call. The arguments, relative-sector addressing, and
  `BLK_OK`/`BLK_E_*` status are unchanged, and the `libplinth::sys_block_read`
  wrapper hides the move -- only the entry mechanism and the now-asynchronous
  wait differ. Syscall nr 10 is retired.
- **`block_read` retired entirely (ABI v2.4).** With block I/O now the ring ABI,
  the kernel `block_read` op (`int 0x80` op 5) and its in-kernel routing are
  gone; `libplinth::sys_block_read` is reimplemented as a single-in-flight shim
  over `ring_register`/`ring_submit`/`ring_wait`, so its signature and `BLK_*`
  status are unchanged and every existing caller (`blk-user`, `fsdemo-user`,
  `libfs`) is untouched at the source level. Op 5 is left unused.
- **`event_recv` retired entirely (ABI v2.5).** With input now the ring ABI, the
  kernel `event_recv` op (`int 0x80` op 4), the in-kernel per-source `EventRing`
  staging buffer, and the boot keyboard selftest that depended on it are gone;
  `input::record` reroutes through `rings::deliver_event` to subscriptions, and
  `libplinth::sys_event_recv` is reimplemented as a single-subscription shim over
  the ring (subscribe once, reap one event per call), so its signature and
  `EVENT_OK`/`EVENT_ERR` status are unchanged and every existing caller
  (`evt-user`, `kbd-user`, `libinput`) is untouched at the source level. Op 4 is
  left unused. The subscriber's CQ is now the only event buffer, so an event
  arriving before any subscription exists is dropped rather than briefly staged
  (immaterial for input).
- Console input, first stages (`event_recv` + an `EventSource` capability). The
  kernel takes the i8042 keyboard's IRQ behind a new interrupt-controller seam
  (`irq`, the one module an APIC port later swaps), queues raw scancodes in a
  bounded per-source event ring, and multiplexes the device through an
  **`EventSource` capability** (`READ`). A new **`event_recv`** call -- on the
  same `int 0x80` gate as IPC, since a blocking read needs a resumable trap
  frame -- returns the next event (raw scancode in a register), blocking until
  one arrives; a process blocked on input is no longer mistaken for a deadlock,
  so the kernel idles waiting for a keystroke. Events are raw: keymaps and
  characters are library-OS policy. An `evt-user` demo reads an event through
  its granted source and is denied reading through a non-source capability.
  Interpretation lives in a new **`libinput`** library OS -- a Set-1 keymap with
  shift and a line reader over `event_recv` (the keymap is pure and
  host-tested, like libfs's archive parser). A `kbd-user` demo reads a line and
  echoes it, so "input is output-only" is retired with the keymap as
  unprivileged policy and the kernel still shipping only raw events.
- Load-from-disk (Phase 2 close, final piece). A read-only **boot archive**
  (superblock + directory of `(name, first_sector, byte_len)` + sector-aligned
  ELF blobs) is assembled by xtask and attached as a **second virtio-blk
  device**; `BlockRange` now names `(dev, start, count)`, so a range is bound to
  one device. A new **`libfs`** library OS parses the archive (a pure,
  host-unit-tested parser -- the filesystem as unprivileged policy) and, given a
  `BlockRange` over the archive disk, reads a named program's ELF off the disk
  into frames and launches it via a new **`spawn_from_buffer`** syscall (ABI
  v2.1, additive). The kernel runs the disk-supplied ELF through the same
  validator as embedded binaries, audited for untrusted input (bounds and
  overflow checks on every header and segment field). An `fsdemo` library OS
  loads `diskhello` -- a program that exists *only* in the archive, not embedded
  in the kernel -- and collects its result, proving the path end to end;
  embedded `spawn`-by-id stays as the bootstrap loader. `xtask smoke` verifies
  the loaded program ran and that frames return to baseline.
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
