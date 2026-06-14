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
child. One process runs at a time, to completion; there is no timer, disk,
or network. See the [README](README.md) for the full demo.

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
- [ ] **Adoption scaffolding** -- this roadmap, contribution norms, and a
  changelog (in progress).

## Phase 2 -- a usable general-purpose exokernel

Everything here follows from adding a timer, and each step is weighed
against the cost to determinism rather than taken for granted.

- [ ] **Timer + preemptive scheduling.** The foundational step, and the one
  that changes how the kernel is tested: the current line-by-line
  deterministic boot log gives way to assertion-based tests. Run more than
  one process, with the kernel deciding when each runs.
- [ ] **Inter-process communication.** Once processes are concurrent, they
  need a way to talk.
- [ ] **Storage and a filesystem.** A block device driver and a minimal
  filesystem -- the path to loading programs from disk rather than
  embedding them at build time.
- [ ] **Console input.** Today `write` is output only.
- [ ] **Broader hardware.** SMP and real-machine device support, each taken
  on its own merits.

## Stability

ABI v1 is frozen: the syscalls, error conventions, executable format, and
process entry state documented in [ABI.md](ABI.md) will not change
incompatibly. New capabilities are added without breaking existing
programs. Anything not in ABI.md is an implementation detail and may move.
