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

pub const MAX_CAPS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapObject {
    /// Ownership of one physical frame (frame-aligned address).
    Frame { addr: u64 },
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

    /// Fetch the capability at `slot`, requiring every right in `required`.
    pub fn lookup(&self, slot: usize, required: u8) -> Result<Capability, CapError> {
        let entry = *self.slots.get(slot).ok_or(CapError::BadSlot)?;
        let cap = entry.ok_or(CapError::EmptySlot)?;
        if cap.rights & required != required {
            return Err(CapError::RightsDenied);
        }
        Ok(cap)
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
