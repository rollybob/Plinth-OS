# Roadmap

Plinth started as the smallest codebase that makes the exokernel argument
concrete. The goal now is to grow it into a free exokernel OS that other
people can actually build on, without losing the property that makes it
worth reading: a kernel that is mechanism, with policy in unprivileged
library OSes.

The work is in two phases. Phase 1 keeps Plinth's deterministic,
read-in-one-sitting character; Phase 2 deliberately trades some of it for
the machinery a usable system needs. Phase 1 comes first.

## Where Plinth is today

An exokernel that boots under QEMU and runs unprivileged programs over a
`syscall` fast path and an `int 0x80` gate for blocking calls: physical
frames and CPU time as capabilities, per-process address spaces,
application-level page-fault handling (self-paging), and `spawn` with
capability transfer into an isolated child. A 100 Hz timer preemptively
multiplexes the CPU across several processes (round-robin); synchronous IPC
connects them; a virtio-blk disk multiplexed by a `BlockRange` capability
backs two filesystems over the same block ring ABI -- a read-only boot
archive (load-from-disk) and a read-write filesystem (create/read/delete) --
and the i8042 keyboard and PS/2 mouse deliver raw events behind `EventSource`
capabilities. Block I/O and input are both async
completion rings (io_uring-shaped shared-memory queues) -- block reads and
writes as one-shot requests, input as multishot subscriptions -- with a
reference `no_std` async executor in `libos` driving many requests in flight
and event streams at once.
Interrupts run through the Local APIC + I/O APIC (MSI-X for the disk, a
per-CPU LAPIC timer), and the kernel boots and schedules on every CPU the
ACPI MADT reports, serialized by a single big kernel lock; scheduling uses
per-core run queues with bounded work stealing (an idle core steals a ready
process from a busy one). The UEFI GOP linear framebuffer is multiplexed by a
`Framebuffer` capability: a graphics library OS (`libgfx`) maps it and does all
the drawing -- pixels, an 8x8 font, text -- in unprivileged code, and the screen
can be split into disjoint horizontal bands handed to separate graphics libOSes,
each confined to its rows by paging. No network yet, and the single lock is
intentionally left whole -- a benchmark showed it only contends near 100%
kernel residency, so splitting it earns nothing real workloads would feel.
See the [README](README.md) for the full demo.

## Phase 1 -- an adoptable reference

Make it possible for someone else to write and run their own program and
library OS against a stable interface, while the kernel stays deterministic
and small.

- [x] **Versioned syscall ABI** -- the interface is a documented contract
  ([ABI.md](ABI.md)), frozen as v1.
- [x] **In-kernel ELF loader** -- the kernel loads a static `ET_EXEC` ELF
  with per-segment W^X, instead of a flat blob. Bring your own program.
- [x] **Templates and a guide** -- a skeleton program crate and a
  walkthrough of writing programs and library OSes ([GUIDE.md](GUIDE.md)).
- [x] **Adoption scaffolding** -- this roadmap, contribution norms
  ([CONTRIBUTING.md](CONTRIBUTING.md)), and a [changelog](CHANGELOG.md).

## Phase 2 -- a usable general-purpose exokernel

Everything here follows from adding a timer, and each step is weighed
against the cost to determinism rather than taken for granted.

- [x] **Timer + preemptive scheduling.** A 100 Hz PIT preempts ring-3 code;
  the kernel saves the full context, switches address space and kernel stack,
  and round-robins independent processes (`kernel/src/scheduler.rs`). The
  kernel is non-preemptible (it reschedules only out of ring 3). Testing moved
  off the exact boot trace: per-process ordering plus no-leak invariants for
  the interleaving demo, and `pick_next` as unit tests.
- [x] **Inter-process communication.** Synchronous capability-named endpoints
  (`kernel/src/ipc.rs`): `send`/`recv` rendezvous, capability transfer through
  messages (zero-copy frame handoff), and `call`/`reply` RPC with a one-shot
  reply capability. `spawn` is reconciled with the scheduler -- it launches an
  independent scheduled process and the parent waits with `recv` (the join).
- [x] **Storage and a filesystem.** An in-kernel virtio-blk driver (PCI
  enumeration, mapped MMIO, one virtqueue) multiplexed by a `BlockRange`
  capability; a read-only boot archive parsed by an unprivileged `libfs`; and
  `spawn_from_buffer`, so a program is loaded off disk and run rather than
  embedded at build time. Block reads are interrupt-driven and blocking -- the
  CPU runs other processes while the disk DMA is in flight, woken by the
  completion IRQ through the same interrupt-controller seam the keyboard uses.
- [x] **Async block I/O (completion rings).** Block I/O moved off a kernel
  entry per read onto io_uring-shaped shared-memory submission/completion
  queues. A library OS submits capability-named requests (never physical
  addresses) and reaps results from memory; the kernel is on the path only for
  a batched doorbell (`ring_submit`) and the completion IRQ, with many reads in
  flight at once. The kernel ships only the mechanism -- it stays the sole
  writer of physical DMA descriptors, so the device is multiplexed across
  tenants with no IOMMU -- and a reference `no_std` async executor in `libos`
  turns completions into woken futures (replaceable policy). `block_read` is
  retired in favour of the ring ABI (v2.4); the throughput lever for a
  kernel-light workload is taking the kernel off the I/O fast path, which is why
  this, not per-CPU run-queue splitting, was the next step after the SMP boot
  model. A `RING_OP_WRITE` op (v2.6) added the write half: the same entry shape
  with the two cap-checks' direction reversed (`BlockRange` via `RIGHT_WRITE`,
  the I/O frame via `RIGHT_READ`), proving the ring mechanism needed no change
  to carry the opposite direction (Design/block_write.md).
- [x] **Console input.** The i8042 keyboard's IRQ feeds raw scancodes behind an
  interrupt-controller seam; an `EventSource` capability multiplexes the device.
  Input rides the **same completion rings as block I/O**: a keystroke answers no
  request, so it is a *multishot subscription* (`RING_OP_EVENT_SUB`, then a
  stream of completions until cancel), and one `ring_wait` loop multiplexes disk
  and input (v2.5, retiring the standalone `event_recv` gate op behind a shim). A
  process blocked on input is not a deadlock -- the kernel idles waiting for a
  keystroke. The `libos` async executor turns the stream into an event stream
  alongside the read future; keymaps and line editing are a library OS
  (`libinput`); the kernel ships only raw events. A second `EventSource` --
  the i8042 mouse on IRQ12, raw dx/dy/button packets -- proved the mechanism
  generalizes past one device with zero ABI or ring changes
  (Design/mouse_input.md).
- [x] **A read-write filesystem.** `librwfs` (Design/readwrite_fs.md), a
  second library OS over the same block ring ABI, with **zero kernel or ABI
  change** -- the write path already proved the mechanism generalized; this
  is the policy built on top. A bitmap allocator and a fixed-maximum-entry
  mutable directory are pure logic, host-tested like the read-only archive's
  parser; a superblock + bitmap + directory metadata region is formatted
  fresh every run and rewritten as one unit after every create/delete. The
  read-only archive is unchanged and remains the boot/initramfs format --
  this is a second, separate format for runtime-mutable files, the same
  "additive, not a rewrite" shape `filesystem.md` used to defer a hypothetical
  FAT libOS. A `rwfs-user` demo proves the bitmap actually reclaims freed
  space, not just hides it: a file created after a delete lands at the exact
  sector the deleted file held.
- [x] **Visual userspace.** The UEFI GOP linear framebuffer the `bootloader`
  crate already maps -- no GPU driver -- multiplexed by a `Framebuffer`
  capability and an `fb_map` syscall (ABI v2.7). The kernel only discovers the
  framebuffer and hands it out; all drawing is library-OS policy in a clean-room
  `libgfx` (a pixel writer, an 8x8 bitmap font + `draw_text`, a deterministic
  frame hash), exactly as the kernel ships raw scancodes and owns no keymap. The
  pixel boundary stays testable by hashing a fixed sub-rectangle to serial (the
  smoke pins `-vga std` headless). The multiplexing payoff: the screen splits
  into disjoint horizontal **bands**, one granted to each of two concurrent
  graphics libOSes, confined to their rows by paging -- and a band holder that
  writes past its grant is `#PF`-terminated (the display analogue of disjoint
  `BlockRange`s). A `gfxtext-user` demo also echoes a keyboard line on-screen,
  joining the framebuffer and the keyboard `EventSource` in one libOS
  (Design/display.md).
- **Broader hardware.** SMP and real-machine device support, each taken on its
  own merits. Split into stages, because adding a second CPU ends the
  single-core invariant the no-lock kernel rested on -- a concurrency redesign,
  not a flag:
  - [x] **APIC.** The 8259 PIC retired for the Local APIC + I/O APIC, the
    interrupt topology discovered from the ACPI MADT (`kernel/src/acpi.rs`);
    virtio-blk completions moved to MSI-X and the preemption tick to the
    per-CPU LAPIC timer. Still uniprocessor and still deterministic -- the
    `irq` seam swapped underneath with nothing above it changing.
  - [x] **SMP -- boot and concurrency model.** Every application processor the
    MADT lists is brought up through a long-mode trampoline
    (`kernel/src/smp.rs`); per-CPU state lives behind `IA32_GS_BASE`
    (`kernel/src/percpu.rs`); a single big kernel lock (`kernel/src/bkl.rs`)
    serializes shared kernel state, and a reschedule IPI wakes idle cores.
    Processes pin to their scheduling core (no migration yet). A
    `cargo xtask smoke-smp` lane reruns the assertion battery on 2-4 cores; an
    `-smp 1` lane keeps the deterministic contract as a regression net.
  - [x] **SMP -- scaling.** Per-CPU run queues with bounded work stealing: each
    core owns a run queue (`CORE_QUEUE`, `kernel/src/scheduler.rs`), processes
    are homed at spawn time, and an idle core steals one Ready process from a
    busy core's queue (lifting the pin from the boot milestone -- migration is
    now a steal). Scoped to scheduling *ownership and locality*, not
    lock-splitting: a benchmark showed the single lock only contends near 100%
    kernel residency, a regime real workloads do not occupy -- so the throughput
    lever was taking the kernel off the I/O fast path (the async rings above),
    not splitting the lock, which is left for a workload that actually contends.
    This is the architectural step toward the share-nothing direction -- per-CPU
    (eventually per-NUMA) state coordinated by message passing. The `steal demo`
    forces an imbalance (one parent spawns workers that pile onto its core while
    others idle) and asserts both completion and that a cross-core steal fired;
    `cargo xtask smoke-smp` exercises it on 2-4 cores.
  - [ ] **Real-machine hardware.** Leaving QEMU's comfortable defaults: real
    ACPI quirks, MMIO cache attributes, PCIe ECAM, a NIC. Its own milestone,
    once the concurrency model is scaled.

## Stability

The ABI is versioned in [ABI.md](ABI.md); the current contract is **v2.7**.
v2 added IPC and revised `spawn`, the one incompatible change from v1 (made
while Phase 2 is still pre-release); v2.1 (`spawn_from_buffer`), v2.2
(console input), v2.3 (`block_read` moved to the blocking gate), v2.4
(async completion rings, retiring `block_read`), v2.5 (input as multishot
ring subscriptions, retiring `event_recv`), v2.6 (`RING_OP_WRITE`, the
write half of the block ring ABI), and v2.7 (the `Framebuffer` capability +
the `fb_map` syscall) are all additive over v2 but for the two
retired-and-shimmed ops. Within a major series, new capabilities are added
without breaking existing programs. Anything not in ABI.md is an implementation
detail and may move.
