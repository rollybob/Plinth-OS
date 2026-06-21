//! Capabilities: the kernel's only notion of "who may touch what".
//!
//! An exokernel exposes raw resources, so access control cannot live in
//! the abstractions (there are none). Instead every grant is explicit:
//! a capability is an unforgeable (kernel-held) record that some process
//! may perform some operations on some resource. Userspace refers to its
//! capabilities by slot index; the records themselves never leave the
//! kernel. This is the "secure bindings" half of the exokernel contract.
//!
//! Tables are fixed-size arrays -- no kernel heap, by design. A toy
//! kernel that needs malloc to express ownership has already smuggled
//! in a policy.

pub const RIGHT_READ: u8 = 1 << 0;
pub const RIGHT_WRITE: u8 = 1 << 1;
pub const RIGHT_MAP: u8 = 1 << 2;
/// The right to spend a CpuTime budget via cpu_charge. Disjoint from the
/// frame rights on purpose: a Frame capability never carries RIGHT_CONSUME
/// and a CpuTime capability never carries RIGHT_MAP, so the rights check
/// alone keeps the two syscall families from touching the wrong object.
pub const RIGHT_CONSUME: u8 = 1 << 3;
/// The right to send on / receive from an Endpoint. Directional, so a
/// capability can grant one half of a channel without the other -- a client
/// gets RIGHT_SEND, a server RIGHT_RECV, on the same endpoint.
pub const RIGHT_SEND: u8 = 1 << 4;
pub const RIGHT_RECV: u8 = 1 << 5;

pub const MAX_CAPS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapObject {
    /// Ownership of one physical frame (frame-aligned address).
    Frame { addr: u64 },
    /// A budget of CPU "ticks" the holder may consume. Unlike a frame --
    /// which the holder owns until it is revoked -- this capability is
    /// depleted by use: cpu_charge debits the budget, and a holder that
    /// tries to spend past zero has overdrawn a resource it does not have.
    /// The kernel mints it at spawn; teardown reclaims the slot but the
    /// "resource" (CPU time) is not poolable, so nothing returns anywhere.
    CpuTime { budget: u64 },
    /// A synchronous IPC endpoint, named by index into the kernel endpoint
    /// table. The holder may send and/or receive (per its rights). The
    /// endpoint itself owns no poolable resource, so teardown just drops the
    /// slot -- like a CpuTime budget.
    Endpoint { id: usize },
    /// A one-shot reply capability minted into a server when it receives a
    /// `call`: it authorizes replying exactly once to the specific caller
    /// (named by its process-table slot), and is consumed on use. The caller
    /// is Blocked-awaiting-reply and cannot run or exit until replied, so the
    /// slot it names always denotes that same caller while the cap exists --
    /// no generation counter is needed. Owns no poolable resource.
    Reply { caller: usize },
    /// A contiguous run of disk blocks (512-byte virtio sectors): `count`
    /// sectors starting at sector `start`, on block device `dev`. This is the
    /// unit by which the kernel multiplexes block storage among library OSes --
    /// disjoint ranges to different holders, the same "secure bindings over a
    /// raw resource" move as frames. RIGHT_READ / RIGHT_WRITE gate the two I/O
    /// directions.
    ///
    /// `dev` is the index of the virtio-blk device the range lives on (devices
    /// are enumerated in PCI-slot order at boot; see `pci`/`virtio_blk`). A
    /// range names device *and* sectors, so a holder cannot reach another
    /// device's blocks any more than another range's: the device index is part
    /// of the multiplexing boundary, not a free syscall argument.
    ///
    /// Pure inline data: the range names no pooled kernel resource (unlike an
    /// Endpoint, which owns a table slot), so teardown just drops it -- no
    /// reference count. (When a read-write filesystem later hands out
    /// *allocated* ranges from a pool, that pool's reservation will need the
    /// endpoint-style refcount; the range capability itself stays inline. This
    /// is the agreed narrowing of hardening ruling D3b, 2026-06-17.)
    BlockRange { dev: u8, start: u64, count: u64 },
    /// An input event source (`id` selects the device: 0 = keyboard). `RIGHT_READ`
    /// gates reading its event stream via `event_recv`. The kernel multiplexes
    /// the physical device into per-source event rings and hands a source's
    /// capability to the library OS that owns input -- the same "secure binding
    /// over a raw resource" move as frames and `BlockRange`. A holder reads only
    /// the source it was granted; `id` is part of the multiplexing boundary.
    ///
    /// Pure inline data, like `BlockRange`: the ring is a fixed kernel static,
    /// not a pooled resource the capability owns, so teardown just drops it --
    /// no reference count (consistent with the D3b narrowing).
    EventSource { id: u8 },
    /// A bound async completion ring (`id` indexes the kernel `rings` table),
    /// minted by `ring_register` over a caller-owned SQ/CQ frame pair. The
    /// holder submits (`ring_submit`) and waits (`ring_wait`) on it; it is bound
    /// to the registering process (ring confinement), so unlike an Endpoint it is
    /// never transferred and needs no reference count -- a single owner. Owns a
    /// table slot (like an Endpoint), so teardown releases it via `rings::release`
    /// (the SQ/CQ frames are ordinary Frame capabilities, freed on their own).
    Ring { id: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    pub object: CapObject,
    pub rights: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapError {
    TableFull,
    /// Slot index beyond the table.
    BadSlot,
    /// Slot exists but holds nothing (never minted, or revoked).
    EmptySlot,
    /// Capability exists but lacks a required right.
    RightsDenied,
    /// Operation does not apply to this object kind (e.g. charging a Frame).
    WrongType,
    /// A CpuTime budget cannot cover the requested charge. The capability
    /// is left untouched; the caller decides what an overdraw means (the
    /// syscall layer terminates the offending process).
    Insufficient,
}

pub struct CapTable {
    slots: [Option<Capability>; MAX_CAPS],
}

impl CapTable {
    pub const fn new() -> CapTable {
        CapTable { slots: [None; MAX_CAPS] }
    }

    /// Install a capability in the first free slot; returns the slot index.
    pub fn mint(&mut self, object: CapObject, rights: u8) -> Result<usize, CapError> {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(Capability { object, rights });
                return Ok(i);
            }
        }
        Err(CapError::TableFull)
    }

    /// Iterate the live capabilities in this table (skipping empty slots), by
    /// value. Read-only -- the death-time IPC reaping uses it to find the
    /// endpoint and reply capabilities a dying process held.
    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        self.slots.iter().filter_map(|slot| *slot)
    }

    /// Fetch the capability at `slot`, requiring every right in `required`.
    pub fn lookup(&self, slot: usize, required: u8) -> Result<Capability, CapError> {
        let entry = *self.slots.get(slot).ok_or(CapError::BadSlot)?;
        let cap = entry.ok_or(CapError::EmptySlot)?;
        if cap.rights & required != required {
            return Err(CapError::RightsDenied);
        }
        Ok(cap)
    }

    /// Debit `amount` from the CpuTime capability at `slot`, requiring
    /// every right in `required` (RIGHT_CONSUME). Returns the remaining
    /// budget on success. Fails with WrongType if the slot holds anything
    /// but a CpuTime budget, and Insufficient if the budget cannot cover
    /// the charge -- in which case the budget is left exactly as it was.
    pub fn charge(&mut self, slot: usize, amount: u64, required: u8) -> Result<u64, CapError> {
        let cap = self
            .slots
            .get_mut(slot)
            .ok_or(CapError::BadSlot)?
            .as_mut()
            .ok_or(CapError::EmptySlot)?;
        if cap.rights & required != required {
            return Err(CapError::RightsDenied);
        }
        let CapObject::CpuTime { budget } = &mut cap.object else {
            return Err(CapError::WrongType);
        };
        let remaining = budget.checked_sub(amount).ok_or(CapError::Insufficient)?;
        *budget = remaining;
        Ok(remaining)
    }

    /// Remove and return the capability at `slot`. Revocation is
    /// unconditional: rights gate use, not removal.
    pub fn revoke(&mut self, slot: usize) -> Result<Capability, CapError> {
        self.slots
            .get_mut(slot)
            .ok_or(CapError::BadSlot)?
            .take()
            .ok_or(CapError::EmptySlot)
    }

    /// Remove every capability, handing each to `f`. Process teardown
    /// uses this to return capability-owned resources to their pools.
    /// (Teardown is unreachable in the test build, hence the cfg_attr.)
    #[cfg_attr(feature = "tests", allow(dead_code))]
    pub fn drain(&mut self, mut f: impl FnMut(Capability)) {
        for slot in self.slots.iter_mut() {
            if let Some(cap) = slot.take() {
                f(cap);
            }
        }
    }
}
