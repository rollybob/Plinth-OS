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

A uniprocessor exokernel that boots under QEMU and runs unprivileged
programs over nine syscalls: physical frames and CPU time as capabilities,
per-process address spaces, application-level page-fault handling
(self-paging), and `spawn` with capability transfer into an isolated
child. A 100 Hz timer preemptively multiplexes the CPU across several
processes (round-robin); there is no disk or network yet. See the
[README](README.md) for the full demo.

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
- [x] **Console input.** The i8042 keyboard's IRQ feeds raw scancodes into a
  bounded event ring behind an interrupt-controller seam; an `EventSource`
  capability multiplexes the device, and a blocking `event_recv` (on the IPC
  gate) delivers events. A process blocked on input is not a deadlock -- the
  kernel idles waiting for a keystroke. Keymaps and line editing are a library
  OS (`libinput`); the kernel ships only raw events.
- [ ] **Broader hardware.** SMP and real-machine device support, each taken
  on its own merits.

## Stability

The ABI is versioned in [ABI.md](ABI.md); the current contract is **v2**.
v2 added IPC and revised `spawn`, the one incompatible change from v1 (made
while Phase 2 is still pre-release). Within a major series, new capabilities
are added without breaking existing programs. Anything not in ABI.md is an
implementation detail and may move.
