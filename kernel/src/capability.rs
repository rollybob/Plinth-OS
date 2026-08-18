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

/// `ABI.md` publishes this as a guaranteed minimum and `libplinth::MIN_CAP_SLOTS`
/// carries the same number, so the two cannot be changed independently without
/// this failing the build. Raising the limit is ABI-compatible, but it is not
/// free: `libplinth::REUSE_ROUNDS` is derived from the published value, and a
/// reuse demo that stops exceeding the real table goes green while asserting
/// nothing. Raise all three -- here, `libplinth`, `ABI.md` -- in one edit.
const _: () = assert!(
    MAX_CAPS == 16,
    "MAX_CAPS changed: update libplinth::MIN_CAP_SLOTS and ABI.md's table-sizes section to match",
);

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
    /// The linear framebuffer: a memory-mapped pixel region
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
    /// whole screen; a sub-region grant (Stage 4) is the same
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
    /// `None` if the kernel minted it (D3).
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
    /// (section 0). Without that sweep this field is
    /// a hazard, not a record.
    ///
    pub origin: Option<Origin>,
}

/// Where a lent capability goes home to: the lender, and the slot it left from.
///
/// Widened from a bare process slot on 2026-08-10 (D2,
/// ruled (D) -- homecoming reservation). The lender's slot answers *who* is owed
/// it, which is all reclamation needed while the kernel chose the landing slot
/// itself; `cap_slot` answers *where it goes*, which is what lets the lender know
/// the answer before the borrower has even died.
///
/// `u8` each, against `MAX_PROCESSES = 4` and `MAX_CAPS = 16`. Both are
/// placeholders and both may grow (D9); `u8` leaves room for
/// 256 of each, and the day either passes that, this struct is the one place to
/// widen. Deliberately not `usize`: `caps: CapTable` is the first field of
/// `Process`, so every byte here is multiplied by `MAX_CAPS` and then by
/// `MAX_PROCESSES` (A-5 -- growing `Capability` grew `Process` by 16x once
/// already, and that moved a frame baseline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Origin {
    /// The lender's process-table slot.
    pub proc_slot: u8,
    /// The capability slot in the lender's table that this was lent from, and
    /// that it returns to. Reserved at lend time from slice 2 onward; recorded
    /// but not yet acted on in slice 1.
    pub cap_slot: u8,
}

impl Origin {
    pub const fn new(proc_slot: usize, cap_slot: usize) -> Origin {
        Origin { proc_slot: proc_slot as u8, cap_slot: cap_slot as u8 }
    }

    /// The lender's process slot, widened back for comparison with the `usize`
    /// slots every other module speaks in.
    pub const fn lender(&self) -> usize {
        self.proc_slot as usize
    }

    /// The slot this capability comes home to.
    pub const fn home(&self) -> usize {
        self.cap_slot as usize
    }
}

impl Capability {
    /// This capability as it should appear in `dest`'s table after `source`
    /// hands it over (D3).
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
    /// **Which level is tracked: the ROOT lender, not the most recent giver**
    /// (D8, ruled 2026-07-30). An origin that is already `Some` survives the hop;
    /// only an unborrowed capability records a new one.
    ///
    /// That is what keeps a sub-loan honest without the derivation tree D1
    /// declined. A lends to B (`origin = A`); B hands it on to C (`origin` stays
    /// `A`); C hands it back to B -- **not** a homecoming, because B is not the
    /// origin, so B holds it while still owing A. Only the hand-back to A clears.
    /// A dying holder at any depth therefore returns the capability to the
    /// process that owns it rather than to an intermediary that never did, which
    /// is what reclamation is for.
    ///
    /// Until D8 the origin was overwritten on every hop, so that third step read
    /// as a homecoming: `origin` cleared and B ended up owning outright a
    /// capability it had only borrowed, with A's claim silently gone and A's
    /// death sweeping nothing (K-013). Note what the fix did not need -- the
    /// tree. Only the decision not to overwrite. This is still exactly one
    /// `Option<Origin>` and still one level; the level is now the root.
    ///
    /// **The slot-identity trap got sharper on 2026-08-10, not safer.** The rule
    /// above -- compare process slots, never "did it come back to the slot it
    /// left from" -- was written when those two tests gave visibly different
    /// answers, because a hand-back landed wherever `install` chose. Under
    /// homecoming reservation (D2(D)) a returning capability
    /// *does* come back to the slot it left, so **the two tests now agree in
    /// every case a demo exercises** and a slot-identity test would pass the
    /// whole suite. It is still wrong: the re-lending case (A -> B -> C, C hands
    /// back to B) turns on *whose* claim is outstanding, and no comparison of
    /// slot numbers can answer that. Compare `proc_slot`. Never derive homecoming
    /// from `cap_slot`, and never from `Origin`'s derived `PartialEq`, which
    /// compares both fields.
    pub fn lent_to(self, source: Origin, dest: usize) -> Capability {
        let origin = match self.origin {
            // Home: the owner holds it outright again. Process slots, per above.
            Some(o) if o.lender() == dest => None,
            // Already on loan. Passing it on moves the capability, not the claim.
            Some(o) => Some(o),
            // A first loan, from an outright owner.
            None => Some(source),
        };
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
    /// that justified the mapping is leaving (D1).
    ///
    /// Deliberately NOT `FreeFrame`: routing a framebuffer through the frame
    /// path would hand ~1000 pages of MMIO to the frame allocator, which would
    /// later serve them as ordinary memory.
    UnmapFramebuffer,
    /// Unmap as `UnmapFramebuffer` does, but do **not** destroy the capability:
    /// hand it back to the process at `lender` (D1/D2).
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
    /// memory (D7). The frame baselines are what would
    /// catch that, and they must not move.
    ReclaimTo { origin: Origin },
    /// Nothing pooled: emptying the slot is the whole of the release.
    DropSlot,
    /// Not releasable on request. Only `Reply`: it names a caller that is
    /// Blocked-awaiting-reply, and dropping it would strand that caller
    /// forever. A live process must reply; a dying one is handled earlier by
    /// `ipc::reap_dying`, which wakes the caller before teardown drains.
    Refuse,
}

/// Where a dying process's capability should go home to, and what it should look
/// like when it gets there. `None` means it is not reclaimable and dies as usual.
///
/// Pure, and separated from the process-table walk on purpose: the walk needs
/// `static mut TABLE` and the in-kernel harness has no process table, exactly as
/// `release_action` and `process::fb_record` are pure for the same reason. This
/// is the part with a decision in it, so this is the part the tests can reach.
///
/// The returned capability has come home, so `lent_to` clears its origin -- the
/// lender owns it outright again, and an origin still naming the dead borrower
/// would be a lie the *next* death would act on. Reusing `lent_to` rather than
/// writing `origin: None` here is deliberate: there is one homecoming rule.
pub fn reclaim_target(cap: &Capability, dying_slot: usize) -> Option<(Origin, Capability)> {
    match release_action(cap) {
        ReleaseAction::ReclaimTo { origin } => {
            // `dying_slot` is the source PROCESS; the source capability slot is
            // irrelevant here because this call always takes `lent_to`'s
            // homecoming branch (the origin names `dest` by construction), and
            // that branch reads neither field of `source`. Passing the origin's
            // own home keeps the argument well-formed without inventing a slot.
            let source = Origin::new(dying_slot, origin.home());
            Some((origin, cap.lent_to(source, origin.lender())))
        }
        _ => None,
    }
}

/// The release policy, as a pure decision. `syscall::sys_cap_release` runs it
/// for one slot on request; `process::teardown` runs it for every slot at
/// death (mapping `Refuse` to "just drop", since `reap_dying` has already
/// woken any stranded caller by then). Pure so the in-kernel test harness can
/// reach it -- a syscall needs a current process and a live address space,
/// and the harness has neither.
///
/// Takes the whole `Capability`, not just its object, because the decision now
/// depends on `origin` (D6, ruled WIDEN rather than adding
/// a sibling function). The payoff of D4 was that this policy is
/// written exactly once, because two places make the decision and must not drift;
/// a second function would have re-created the drift on day one.
///
/// **Both callers now act on `ReclaimTo` the same way -- send the capability
/// home -- and that symmetry was a ruling.** Reclamation was first ruled only for
/// a holder that *dies*, and `sys_cap_release` used to
/// treat `ReclaimTo` as plain `UnmapFramebuffer` for a live process giving a
/// capability up. That stranded the lender's reserved slot when a borrower
/// politely released instead of crashing -- a crash returned the screen, a
/// release did not. Ruled 2026-08-15 (cap_release-on-reserved): a voluntary
/// release routes a borrowed capability home too, via `scheduler::reclaim_cap_home`,
/// the same helper the death path uses. `teardown` still maps this arm (and
/// `Refuse`) to "just drop", because by teardown the reclamation has already run.
/// Can a capability of this kind come home to its lender when a borrower dies?
///
/// The reclaimable SET, named in exactly one place. Two decisions read it and
/// they must not drift (I7): `release_action` asks it at death time, and the
/// lending path asks it at lend time to decide whether to reserve a homecoming
/// slot. A slot reserved for something that can never return would be burnt for
/// the rest of the process's life, so "would this come back?" has to give the
/// same answer at both ends.
///
/// The set is `Framebuffer`, `BlockRange`, and `EventSource` -- the kinds no
/// syscall can re-mint, which is also exactly the set that carries no refcount
/// and names no pool (D6 / D2's "cannot-recreate"
/// property, applied honestly to every kind). Widened from `Framebuffer`-only in
/// slice 4 (2026-08-17), once `blkreclaim-user` gave `BlockRange` a lender:
/// before that a lent non-framebuffer capability was dropped with the dying
/// borrower and the lender got nothing back.
pub const fn is_reclaimable_kind(object: &CapObject) -> bool {
    matches!(
        object,
        CapObject::Framebuffer { .. } | CapObject::BlockRange { .. } | CapObject::EventSource { .. }
    )
}

/// Does handing this capability away create a NEW loan, one that should reserve
/// a homecoming slot? **The single rule for when a lend reserves.**
///
/// Two conditions, and the second is the one that is easy to miss. The kind must
/// be able to come home at all (`is_reclaimable_kind`) -- reserving a slot for
/// something that will never return burns it for the process's lifetime. And the
/// giver must be the **outright owner**: a capability that already carries an
/// origin is on loan from someone else, and passing it on moves the capability
/// without moving the claim (D8, the root-lender rule). An intermediary that
/// reserved a slot would be holding one open for a capability that is going home
/// to a different process entirely.
///
/// That second condition is K-025's shape on the reservation side, and it is
/// stated here rather than at each call site for the same reason K-025 happened:
/// there is more than one lend path, and the one that did not go through the
/// shared rule is the one that got it wrong.
pub fn lend_reserves_home(cap: &Capability) -> bool {
    cap.origin.is_none() && is_reclaimable_kind(&cap.object)
}

pub fn release_action(cap: &Capability) -> ReleaseAction {
    // Scope is the reclaimable set -- `Framebuffer`, `BlockRange`, `EventSource`
    // (D6, widened 2026-08-17 from D2's
    // Framebuffer-only). `is_reclaimable_kind` names that set in one place; a lent
    // capability of any of those kinds goes home to its lender, and every other
    // kind falls through to the per-kind match below. `EventSource` was cut from
    // D2's draft for having no lender; slice 4 added the set once `blkreclaim-user`
    // gave `BlockRange` one, and the shared predicate keeps this death-time answer
    // and the lend-time reservation from drifting.
    if is_reclaimable_kind(&cap.object) {
        if let Some(origin) = cap.origin {
            return ReleaseAction::ReclaimTo { origin };
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
        // (D1); the invariant is that a live framebuffer
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
    /// Slots held open for a capability that is out on loan and will come back
    /// here (D2, ruled (D) -- homecoming reservation).
    ///
    /// A reserved slot is empty but **not free**: `install` skips it, so nothing
    /// else can take the place a lent capability is coming home to. That is the
    /// whole mechanism. Because the destination is fixed when the loan starts,
    /// the lender knows it before the borrower has even died, which is what makes
    /// the notification an optimisation rather than the only route to the fact.
    ///
    /// A bare `[bool; MAX_CAPS]` rather than a bitmask: `MAX_CAPS` is a
    /// placeholder that may grow (D9), and an array grows with it while a `u16`
    /// would silently stop covering the table. It costs `MAX_CAPS` bytes against
    /// the 256 this milestone's slice 1 gave back.
    ///
    /// Nothing needs to record *who* a slot is reserved for. The returning
    /// capability carries its own destination in `origin.cap_slot`; this array
    /// only answers "is this slot spoken for".
    reserved: [bool; MAX_CAPS],
}

impl CapTable {
    pub const fn new() -> CapTable {
        CapTable { slots: [None; MAX_CAPS], reserved: [false; MAX_CAPS] }
    }

    /// Read the capability at `slot` without a rights check.
    ///
    /// For kernel-internal decisions about a capability's *kind*, where no right
    /// is being exercised -- the lending path asks this to decide whether the
    /// slot is worth reserving. Not a substitute for `lookup`, which is what a
    /// syscall must use.
    pub fn peek(&self, slot: usize) -> Option<Capability> {
        *self.slots.get(slot)?
    }

    /// Hold `slot` open for a capability that is going out on loan from it.
    ///
    /// Separate from the revoke so the caller can unmap first: the lending path
    /// runs `process::revoke_and_unmap_for_lend`, which must take the mapping
    /// down with the authority (D1) before the slot is spoken
    /// for.
    pub fn reserve(&mut self, slot: usize) {
        if let Some(r) = self.reserved.get_mut(slot) {
            *r = true;
        }
    }

    /// Place `cap` in this table, honouring a homecoming reservation if there is
    /// one -- **the single rule for where an arriving capability goes**.
    ///
    /// `prior` is the capability's origin *before* the transfer, and it has to be
    /// passed in because `lent_to` has already cleared it by the time a
    /// capability arrives home: the homecoming rule fires exactly when the
    /// destination is the origin, which is exactly the case this needs to
    /// recognise. Reading `cap.origin` here would always see `None` and always
    /// fall through to `install`.
    ///
    /// **Every path that puts a capability into a table should call this rather
    /// than `install`.** Slice 2 taught `install` to skip
    /// reserved slots but taught only the death path to target them, so the
    /// cooperative hand-back landed past its own reservation and stranded a slot
    /// per launch -- green the whole time. That is a rule living in more places
    /// than one (I7); this function is the fix for the class, not the instance.
    pub fn install_home(
        &mut self,
        cap: Capability,
        prior: Option<Origin>,
        dest_proc: usize,
    ) -> Option<usize> {
        if let Some(o) = prior {
            if o.lender() == dest_proc {
                if let Some(slot) = self.reclaim_to(o.home(), cap) {
                    return Some(slot);
                }
            }
        }
        self.install(cap).ok()
    }

    /// Is `slot` held open for a returning capability?
    pub fn is_reserved(&self, slot: usize) -> bool {
        self.reserved.get(slot).copied().unwrap_or(false)
    }

    /// Give up a reservation without anything coming home.
    ///
    /// The spawn-failure rollback path uses this: the loan never happened, so the
    /// slot must go back to being ordinarily free.
    pub fn clear_reservation(&mut self, slot: usize) {
        if let Some(r) = self.reserved.get_mut(slot) {
            *r = false;
        }
    }

    /// Put a returning capability back in the slot reserved for it.
    ///
    /// `Some(slot)` on success. `None` if the slot is not reserved or is somehow
    /// occupied -- callers fall back to `reclaim`, which is what every lend did
    /// before reservation existed.
    pub fn reclaim_to(&mut self, slot: usize, cap: Capability) -> Option<usize> {
        if !self.is_reserved(slot) || self.slots.get(slot)?.is_some() {
            return None;
        }
        self.slots[slot] = Some(cap);
        self.reserved[slot] = false;
        Some(slot)
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
            // `reserved` is the reason this is not simply "first empty": a slot
            // held open for a returning capability is empty and unavailable.
            if slot.is_none() && !self.reserved[i] {
                *slot = Some(cap);
                return Ok(i);
            }
        }
        Err(CapError::TableFull)
    }

    /// Install `cap` and return its slot, or `None` if the table is full.
    ///
    /// The reclamation half of a death: `Some` means the capability came home,
    /// `None` is D4's second fallback -- the lender filled its own 16 slots, so
    /// there is nowhere to put it and it dies as it would have anyway. That is a
    /// hazard the lender controls rather than a trap, and not a hypothetical one:
    /// the shell used to leak a slot per launch and the ninth spawn failed with
    /// the table full (`shell-user`, "the 2026-06-27 crash"). ABI v2.8's
    /// `cap_release` is what turned that from unfixable into routine.
    pub fn reclaim(&mut self, cap: Capability) -> Option<usize> {
        self.install(cap).ok()
    }

    /// Clear every `origin` naming `slot`, returning how many were cleared.
    ///
    /// Run against every live table when a process exits
    /// (section 0). Without it, `origin` is a hazard
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
                // `proc_slot` only -- this asks whose claim is departing, and
                // `Origin`'s derived `PartialEq` would also compare `cap_slot`.
                if cap.origin.map(|o| o.lender()) == Some(slot) {
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


