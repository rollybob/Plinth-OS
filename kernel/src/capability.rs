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
    /// The linear framebuffer (Design/display.md): a memory-mapped pixel region
    /// the bootloader's UEFI GOP set up, named together with the geometry needed
    /// to draw into it. `RIGHT_MAP` gates `fb_map`, which maps the region into
    /// the holder's address space; `RIGHT_WRITE` marks it writable. The kernel
    /// multiplexes the raw region; all drawing (fonts, layout, compositing) is
    /// library-OS policy -- the same "secure binding over a raw resource" move as
    /// frames, `BlockRange`, and `EventSource`, applied to pixels.
    ///
    /// Geometry is carried inline (like `BlockRange`'s sectors): `phys_base` is
    /// the region's physical base, `stride` the pixels per row (>= width), and
    /// `format` the pixel layout (0=rgb, 1=bgr, 2=u8, 3=other). v1 grants the
    /// whole screen; a sub-region grant (Design/display.md Stage 4) is the same
    /// variant with a smaller rect and an offset `phys_base`, the display
    /// analogue of disjoint `BlockRange`s. Pure inline data naming firmware MMIO,
    /// not a pooled resource, so teardown just drops it -- no reference count,
    /// and the pixel frames are never returned to the allocator (consistent with
    /// the D3b narrowing).
    Framebuffer {
        phys_base: u64,
        width: u32,
        height: u32,
        stride: u32,
        bytes_per_pixel: u8,
        format: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    pub object: CapObject,
    pub rights: u8,
    /// The process that lent this capability, as a process-table slot, or
    /// `None` if the kernel minted it (Design/cap_reclaim.md D3).
    ///
    /// Set at transfer time so a holder's death can return an unre-mintable
    /// capability to its lender instead of destroying it. `None` means "nobody
    /// lent this" -- every kernel mint, and every capability that has come home
    /// to its own origin (the homecoming rule: a stale origin pointing at a
    /// process with no remaining claim is a lie the next death would act on).
    ///
    /// A slot, not a `ProcessId`, because this kernel has no such type -- but
    /// note the safety argument that makes `Reply { caller }`'s slot sound (the
    /// named caller is pinned Blocked and cannot exit) does NOT hold here: a
    /// lender is not pinned and can exit while the borrower lives, after which
    /// its slot may be reused by an unrelated process. What keeps this honest is
    /// the exit-time sweep that clears every `origin` naming a departing slot
    /// (Design/cap_reclaim_build.md section 0). Without that sweep this field is
    /// a hazard, not a record.
    ///
    pub origin: Option<usize>,
}

impl Capability {
    /// This capability as it should appear in `dest`'s table after `source`
    /// hands it over (Design/cap_reclaim.md D3).
    ///
    /// Ordinarily the new origin is `source`: whoever gave it away is who it
    /// goes back to if the new holder dies.
    ///
    /// **The homecoming rule** is the exception: if the capability is going back
    /// to the very process that lent it, the origin *clears*. It is owned
    /// outright again, and an origin still naming a process with no remaining
    /// claim would be a lie the next death would act on.
    ///
    /// Homecoming is decided by comparing *process slots*, never by noticing
    /// that a capability returned to the slot it left from -- those are not the
    /// same test, and only the first is correct. `shell-user` is the proof: its
    /// framebuffer leaves `FB_SLOT`, the wait handle is minted into the freed
    /// slot, and the app's hand-back lands wherever `recv_cap` chooses, so the
    /// shell carries a mutable `fb_slot` across launches. A slot-identity test
    /// would fail to clear the origin on every real hand-back.
    pub fn lent_to(self, source: usize, dest: usize) -> Capability {
        let origin = if self.origin == Some(dest) { None } else { Some(source) };
        Capability { origin, ..self }
    }
}

/// What the kernel must do, beyond emptying the slot, when a capability
/// leaves a table for good.
///
/// Most capabilities name no pooled kernel resource -- the D3b hardening
/// narrowing (2026-06-17) settled which ones do -- so most of these are
/// `DropSlot`. The point of naming the decision is that there are two places
/// that make it (`cap_release` and `process::teardown`) and they must not
/// drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseAction {
    /// Return this frame to the allocator. The caller must also unmap every
    /// mapping made through the slot -- the frame is about to be reusable by
    /// someone else, so a surviving mapping would be a hole in the isolation.
    FreeFrame { addr: u64 },
    /// Release the kernel ring-table slot. The ring's SQ/CQ frames are
    /// ordinary `Frame` capabilities and are freed on their own.
    ReleaseRing { id: usize },
    /// Drop one reference to an endpoint, freeing the endpoint slot if this
    /// was the last capability able to reach it.
    DropEndpoint,
    /// Unmap every framebuffer mapping made through this slot. Nothing is
    /// freed -- the pages name firmware MMIO and were never allocated from
    /// anywhere -- but they must stop being reachable, because the authority
    /// that justified the mapping is leaving (Design/fb_mapping.md D1).
    ///
    /// Deliberately NOT `FreeFrame`: routing a framebuffer through the frame
    /// path would hand ~1000 pages of MMIO to the frame allocator, which would
    /// later serve them as ordinary memory.
    UnmapFramebuffer,
    /// Unmap as `UnmapFramebuffer` does, but do **not** destroy the capability:
    /// hand it back to the process at `lender` (Design/cap_reclaim.md D1/D2).
    ///
    /// This variant exists because `UnmapFramebuffer` conflates two things that
    /// reclamation has to separate. The dying holder's mapping must still die
    /// with it -- that is the fb_mapping D1 invariant, and `gfxrevoke-user` is
    /// the regression test for it -- while the capability *value* survives to be
    /// installed in the lender's table. "Unmap" and "the authority is gone" are
    /// not the same event once a capability can be lent.
    ///
    /// Deliberately NOT `FreeFrame`, for the same reason `UnmapFramebuffer` is
    /// not: a framebuffer names ~1000 pages of firmware MMIO, and routing them
    /// into the frame allocator would have it serve them later as ordinary
    /// memory (Design/fb_mapping.md D7). The frame baselines are what would
    /// catch that, and they must not move.
    ReclaimTo { lender: usize },
    /// Nothing pooled: emptying the slot is the whole of the release.
    DropSlot,
    /// Not releasable on request. Only `Reply`: it names a caller that is
    /// Blocked-awaiting-reply, and dropping it would strand that caller
    /// forever. A live process must reply; a dying one is handled earlier by
    /// `ipc::reap_dying`, which wakes the caller before teardown drains.
    Refuse,
}

/// The release policy, as a pure decision. `syscall::sys_cap_release` runs it
/// for one slot on request; `process::teardown` runs it for every slot at
/// death (mapping `Refuse` to "just drop", since `reap_dying` has already
/// woken any stranded caller by then). Pure so the in-kernel test harness can
/// reach it -- a syscall needs a current process and a live address space,
/// and the harness has neither.
///
/// Takes the whole `Capability`, not just its object, because the decision now
/// depends on `origin` (Design/cap_reclaim.md D6, ruled WIDEN rather than adding
/// a sibling function). The payoff of `cap_release.md` D4 was that this policy is
/// written exactly once, because two places make the decision and must not drift;
/// a second function would have re-created the drift on day one.
///
/// **Callers interpret `ReclaimTo` differently, and that asymmetry is the point.**
/// It is the same shape as `Refuse`, which teardown already maps to "just drop".
/// Reclamation was ruled for capabilities whose holder *dies*
/// (Design/cap_reclaim.md is titled for exactly that), so `sys_cap_release` --
/// a live process voluntarily giving a capability up -- deliberately treats
/// `ReclaimTo` as plain `UnmapFramebuffer`. Whether a *voluntary* release should
/// also send a borrowed framebuffer home is a real question the ruling did not
/// decide; it would change `cap_release`'s observable behaviour, so it is not
/// being smuggled in here.
pub fn release_action(cap: &Capability) -> ReleaseAction {
    // Scope is `Framebuffer` only (Design/cap_reclaim.md D2, narrowed at ruling
    // time). `EventSource` was in the draft and was cut: the kernel hands every
    // process that needs input its own, `sys_spawn` moves exactly one capability
    // so a lender would have to give up the screen to lend a keyboard, and no
    // user crate lends one -- an arm with no caller and no test to distinguish it
    // from dead code. It becomes a one-line change here once a lender exists.
    if let CapObject::Framebuffer { .. } = cap.object {
        if let Some(lender) = cap.origin {
            return ReleaseAction::ReclaimTo { lender };
        }
    }
    match cap.object {
        CapObject::Frame { addr } => ReleaseAction::FreeFrame { addr },
        CapObject::Ring { id } => ReleaseAction::ReleaseRing { id },
        CapObject::Endpoint { .. } => ReleaseAction::DropEndpoint,
        CapObject::Reply { .. } => ReleaseAction::Refuse,
        // A Framebuffer names firmware MMIO -- nothing to free -- but its
        // mapping must go with the authority. Until v2.8 it did not: the
        // mapping outlived both release and transfer, so a process could keep
        // drawing through pixels it no longer had any right to. That is closed
        // (Design/fb_mapping.md D1); the invariant is that a live framebuffer
        // mapping exists only while the process holds a capability naming it.
        CapObject::Framebuffer { .. } => ReleaseAction::UnmapFramebuffer,
        // Pure inline data naming no pooled resource. A CpuTime budget is
        // forfeit rather than returned -- CPU time is not poolable.
        CapObject::CpuTime { .. }
        | CapObject::BlockRange { .. }
        | CapObject::EventSource { .. } => ReleaseAction::DropSlot,
    }
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

    /// Install a kernel-minted capability in the first free slot; returns the
    /// slot index. `origin` is `None`: nothing lent this.
    ///
    /// Deliberately keeps its two-argument signature so that none of its
    /// existing call sites move when `origin` is introduced. The lending path
    /// gets its own constructor rather than a third argument here, so that a
    /// call site says which case it is instead of passing `None`.
    pub fn mint(&mut self, object: CapObject, rights: u8) -> Result<usize, CapError> {
        self.install(Capability { object, rights, origin: None })
    }

    /// Install a capability *verbatim* in the first free slot, preserving its
    /// `origin`; returns the slot index.
    ///
    /// This is the sibling `mint` needs because `mint` reconstructs a capability
    /// from `(object, rights)` and therefore silently drops the origin. Both are
    /// wanted, and the distinction is exactly which case a call site is in:
    ///
    /// - **`mint`** -- the kernel is creating authority. No lender.
    /// - **`install`** -- an existing capability is *moving*. Its origin is part
    ///   of what moves, whether that is a transfer (with `lent_to` applied) or a
    ///   best-effort restore after a failed transfer (verbatim, so the giver
    ///   gets back precisely what it had).
    pub fn install(&mut self, cap: Capability) -> Result<usize, CapError> {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(cap);
                return Ok(i);
            }
        }
        Err(CapError::TableFull)
    }

    /// Clear every `origin` naming `slot`, returning how many were cleared.
    ///
    /// Run against every live table when a process exits
    /// (Design/cap_reclaim_build.md section 0). Without it, `origin` is a hazard
    /// rather than a record: a lender can exit while its borrower lives, and the
    /// process table reuses slots, so a surviving origin would eventually name
    /// an unrelated process and reclamation would hand that stranger the screen.
    ///
    /// It also earns its keep a second way -- it makes D4's "the lender is dead"
    /// fallback fall out for free. A dead lender's origin is already `None`, so
    /// the reclamation path simply finds nothing to return to and drops as it
    /// does today, with no liveness check to get wrong.
    pub fn clear_origin(&mut self, slot: usize) -> usize {
        let mut cleared = 0;
        for entry in self.slots.iter_mut() {
            if let Some(cap) = entry.as_mut() {
                if cap.origin == Some(slot) {
                    cap.origin = None;
                    cleared += 1;
                }
            }
        }
        cleared
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
