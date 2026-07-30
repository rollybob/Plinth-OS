//! Capability table tests.

use super::TestCtx;
use crate::capability::{
    release_action, CapError, CapObject, CapTable, Capability, ReleaseAction, MAX_CAPS,
    RIGHT_CONSUME, RIGHT_MAP, RIGHT_READ, RIGHT_WRITE,
};
use crate::test_assert;

/// A framebuffer-shaped capability, the kind reclamation exists for.
fn fb_cap(origin: Option<usize>) -> Capability {
    Capability {
        object: CapObject::Framebuffer {
            phys_base: 0x8000_0000,
            width: 1280,
            height: 800,
            stride: 1280,
            bytes_per_pixel: 4,
            format: 1,
        },
        rights: RIGHT_MAP | RIGHT_WRITE,
        origin,
    }
}

/// A transfer records the giver as the lender, and `install` preserves it where
/// `mint` would silently drop it (Design/cap_reclaim.md D3).
pub fn origin_recorded_on_transfer(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    // Process 1 hands a kernel-minted capability to process 2.
    let lent = fb_cap(None).lent_to(1, 2);
    test_assert!(lent.origin == Some(1), "a transfer must record the giver as the lender");

    let mut table = CapTable::new();
    let slot = table.install(lent).map_err(|_| "install failed")?;
    let back = table.lookup(slot, RIGHT_MAP).map_err(|_| "lookup failed")?;
    test_assert!(back.origin == Some(1), "install must preserve the origin");

    // The contrast that makes `install` necessary at all.
    let mut minted = CapTable::new();
    let s = minted.mint(lent.object, lent.rights).map_err(|_| "mint failed")?;
    let m = minted.lookup(s, RIGHT_MAP).map_err(|_| "lookup failed")?;
    test_assert!(m.origin.is_none(), "mint must produce a kernel capability with no lender");
    Ok(())
}

/// The homecoming rule: a capability handed back to the process that lent it is
/// owned outright again, so its origin clears rather than pointing at the
/// borrower. Decided by process slot, never by which table slot it lands in --
/// `shell-user`'s framebuffer never returns to the slot it left from.
pub fn origin_clears_on_homecoming(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    // Shell (1) lends the screen to the app (2), which hands it back.
    let lent = fb_cap(None).lent_to(1, 2);
    test_assert!(lent.origin == Some(1), "the shell must be recorded as the lender");
    let home = lent.lent_to(2, 1);
    test_assert!(home.origin.is_none(), "a capability returned to its lender must clear its origin");

    // A second round trip must behave identically -- the shell relaunches, and a
    // stale origin here would be the lie the next death acts on.
    let again = home.lent_to(1, 2);
    test_assert!(again.origin == Some(1), "a relaunch must record the lender again");
    test_assert!(again.lent_to(2, 1).origin.is_none(), "the second hand-back must clear too");

    // Passing it on to a THIRD process is not a homecoming: one level is tracked,
    // so the origin becomes whoever gave it away most recently (D4).
    let onward = lent.lent_to(2, 3);
    test_assert!(onward.origin == Some(2), "a hand-on records the most recent giver");
    Ok(())
}

/// A lender's exit must clear its slot from every surviving capability, because
/// `origin` names a process-table slot and the table reuses slots
/// (Design/cap_reclaim_build.md section 0). Without this, reclamation would
/// eventually mint the screen into an unrelated process.
pub fn origin_cleared_when_lender_exits(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut table = CapTable::new();
    let from_one = table.install(fb_cap(Some(1))).map_err(|_| "install failed")?;
    let from_two = table.install(fb_cap(Some(2))).map_err(|_| "install failed")?;
    let kernel = table.mint(CapObject::Frame { addr: 0x1000 }, RIGHT_READ).map_err(|_| "mint")?;

    test_assert!(table.clear_origin(1) == 1, "exactly the one capability lent by 1 must clear");
    test_assert!(
        table.lookup(from_one, RIGHT_MAP).map_err(|_| "lookup")?.origin.is_none(),
        "a dead lender's origin must not survive its slot"
    );
    test_assert!(
        table.lookup(from_two, RIGHT_MAP).map_err(|_| "lookup")?.origin == Some(2),
        "an unrelated lender must be left alone"
    );
    test_assert!(
        table.lookup(kernel, RIGHT_READ).map_err(|_| "lookup")?.origin.is_none(),
        "a kernel capability has no origin to clear"
    );
    // Idempotent: a second sweep of the same slot finds nothing left.
    test_assert!(table.clear_origin(1) == 0, "sweeping a cleared slot must be a no-op");
    Ok(())
}

pub fn mint_lookup(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut table = CapTable::new();
    let obj = CapObject::Frame { addr: 0x1000 };
    let slot = table.mint(obj, RIGHT_READ | RIGHT_WRITE).map_err(|_| "mint failed")?;
    let cap = table.lookup(slot, RIGHT_READ).map_err(|_| "lookup failed")?;
    test_assert!(cap.object == obj, "object does not match what was minted");
    Ok(())
}

pub fn rights_denied(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut table = CapTable::new();
    let slot = table
        .mint(CapObject::Frame { addr: 0x1000 }, RIGHT_READ)
        .map_err(|_| "mint failed")?;
    test_assert!(
        table.lookup(slot, RIGHT_WRITE) == Err(CapError::RightsDenied),
        "write allowed by read-only capability"
    );
    test_assert!(
        table.lookup(slot, RIGHT_READ | RIGHT_WRITE) == Err(CapError::RightsDenied),
        "combined rights allowed when only read granted"
    );
    test_assert!(table.lookup(slot, RIGHT_READ).is_ok(), "granted right denied");
    Ok(())
}

pub fn revoke(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut table = CapTable::new();
    let slot = table
        .mint(CapObject::Frame { addr: 0x1000 }, RIGHT_READ)
        .map_err(|_| "mint failed")?;
    table.revoke(slot).map_err(|_| "revoke failed")?;
    test_assert!(
        table.lookup(slot, RIGHT_READ) == Err(CapError::EmptySlot),
        "lookup succeeded after revoke"
    );
    test_assert!(
        table.revoke(slot) == Err(CapError::EmptySlot),
        "second revoke succeeded"
    );
    let reused = table
        .mint(CapObject::Frame { addr: 0x2000 }, RIGHT_READ)
        .map_err(|_| "mint after revoke failed")?;
    test_assert!(reused == slot, "revoked slot was not reused");
    Ok(())
}

pub fn table_full(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut table = CapTable::new();
    for i in 0..MAX_CAPS {
        let addr = (i as u64 + 1) * 0x1000;
        table
            .mint(CapObject::Frame { addr }, RIGHT_READ)
            .map_err(|_| "mint failed before table was full")?;
    }
    test_assert!(
        table.mint(CapObject::Frame { addr: 0xdead_0000 }, RIGHT_READ)
            == Err(CapError::TableFull),
        "mint succeeded on a full table"
    );
    Ok(())
}

pub fn bad_slot(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let table = CapTable::new();
    test_assert!(
        table.lookup(MAX_CAPS, RIGHT_READ) == Err(CapError::BadSlot),
        "out-of-range slot index was accepted"
    );
    test_assert!(
        table.lookup(0, RIGHT_READ) == Err(CapError::EmptySlot),
        "empty slot lookup did not report EmptySlot"
    );
    Ok(())
}

/// A CpuTime budget steps down to exactly zero, and the charge past zero
/// is rejected without disturbing the (now-empty) budget.
pub fn cpu_charge_lifecycle(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut table = CapTable::new();
    let slot = table
        .mint(CapObject::CpuTime { budget: 1024 }, RIGHT_CONSUME)
        .map_err(|_| "mint failed")?;

    test_assert!(table.charge(slot, 256, RIGHT_CONSUME) == Ok(768), "first charge wrong");
    test_assert!(table.charge(slot, 256, RIGHT_CONSUME) == Ok(512), "second charge wrong");
    test_assert!(table.charge(slot, 512, RIGHT_CONSUME) == Ok(0), "drain to zero wrong");

    // Charging past zero overdraws; the budget must stay at zero.
    test_assert!(
        table.charge(slot, 1, RIGHT_CONSUME) == Err(CapError::Insufficient),
        "overdraw was not rejected"
    );
    test_assert!(
        table.charge(slot, 0, RIGHT_CONSUME) == Ok(0),
        "budget was disturbed by the rejected overdraw"
    );
    Ok(())
}

/// Spending a CpuTime budget needs RIGHT_CONSUME; a budget minted without
/// it cannot be charged.
pub fn cpu_charge_rights_denied(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut table = CapTable::new();
    let slot = table
        .mint(CapObject::CpuTime { budget: 100 }, RIGHT_READ)
        .map_err(|_| "mint failed")?;
    test_assert!(
        table.charge(slot, 1, RIGHT_CONSUME) == Err(CapError::RightsDenied),
        "charge allowed without RIGHT_CONSUME"
    );
    Ok(())
}

/// charge only applies to CpuTime; aiming it at a Frame is a type error,
/// even when the rights check passes.
pub fn cpu_charge_wrong_type(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut table = CapTable::new();
    let slot = table
        .mint(CapObject::Frame { addr: 0x1000 }, RIGHT_CONSUME)
        .map_err(|_| "mint failed")?;
    test_assert!(
        table.charge(slot, 1, RIGHT_CONSUME) == Err(CapError::WrongType),
        "charging a frame did not report WrongType"
    );
    Ok(())
}

/// A BlockRange names a device + sector run and gates the two I/O directions
/// by RIGHT_READ / RIGHT_WRITE. A range minted read-only must satisfy a read
/// lookup and refuse a write lookup -- the cap-level half of the block
/// multiplexing guard (the sector-bounds half lives in the block syscall and is
/// exercised end-to-end by the blk demo). Also pins that the device index is
/// carried in the capability, not inferred.
pub fn block_range_rights(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut table = CapTable::new();
    let obj = CapObject::BlockRange { dev: 1, start: 8, count: 4 };
    let slot = table.mint(obj, RIGHT_READ).map_err(|_| "mint failed")?;

    let cap = table.lookup(slot, RIGHT_READ).map_err(|_| "read lookup failed")?;
    test_assert!(cap.object == obj, "BlockRange did not round-trip through the table");
    let CapObject::BlockRange { dev, start, count } = cap.object else {
        return Err("looked-up capability is not a BlockRange");
    };
    test_assert!(dev == 1 && start == 8 && count == 4, "BlockRange fields altered");

    test_assert!(
        table.lookup(slot, RIGHT_WRITE) == Err(CapError::RightsDenied),
        "write allowed by a read-only BlockRange"
    );
    Ok(())
}

/// An EventSource names an input device and gates reading by RIGHT_READ. A
/// source minted read-only must satisfy a read lookup and refuse a write
/// lookup, and the device id must round-trip -- the cap-level half of the input
/// multiplexing gate (a capability to one source names only that id, so it can
/// never reach another).
pub fn event_source_rights(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut table = CapTable::new();
    let obj = CapObject::EventSource { id: 0 };
    let slot = table.mint(obj, RIGHT_READ).map_err(|_| "mint failed")?;

    let cap = table.lookup(slot, RIGHT_READ).map_err(|_| "read lookup failed")?;
    test_assert!(cap.object == obj, "EventSource did not round-trip through the table");
    let CapObject::EventSource { id } = cap.object else {
        return Err("looked-up capability is not an EventSource");
    };
    test_assert!(id == 0, "EventSource id altered");

    test_assert!(
        table.lookup(slot, RIGHT_WRITE) == Err(CapError::RightsDenied),
        "write allowed by a read-only EventSource"
    );
    Ok(())
}

/// The release policy (Design/cap_release.md D4): every capability kind maps to
/// exactly one action, and the two callers -- `cap_release` and
/// `process::teardown` -- both read it from here, so they cannot drift apart.
///
/// This is the whole of what the in-kernel harness can reach: `sys_cap_release`
/// itself needs a current process and a live address space, so the syscall's
/// end-to-end behaviour is proved by caprelease-user in the smoke instead.
/// Guarding the decision here is what stops a future capability kind being
/// added with a reclaim path at death and a leak on request.
pub fn release_action_per_kind(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    test_assert!(
        release_action(&CapObject::Frame { addr: 0x4000 })
            == ReleaseAction::FreeFrame { addr: 0x4000 },
        "a Frame must be returned to the allocator, at its own address"
    );
    test_assert!(
        release_action(&CapObject::Ring { id: 2 }) == ReleaseAction::ReleaseRing { id: 2 },
        "a Ring must release its kernel table slot, by its own id"
    );
    test_assert!(
        release_action(&CapObject::Endpoint { id: 3 }) == ReleaseAction::DropEndpoint,
        "an Endpoint must drop a reference so free-at-zero can fire"
    );

    // A Framebuffer owns nothing poolable either, but it is NOT a plain
    // DropSlot: its mapping has to come down with the authority, or a process
    // that transfers or releases the screen keeps drawing on it
    // (Design/fb_mapping.md D1). Pinned separately from the list below so that
    // regressing it back to DropSlot fails here rather than silently.
    test_assert!(
        release_action(&CapObject::Framebuffer {
            phys_base: 0x8000_0000,
            width: 1280,
            height: 800,
            stride: 1280,
            bytes_per_pixel: 4,
            format: 1,
        }) == ReleaseAction::UnmapFramebuffer,
        "a Framebuffer must unmap on release -- access must not outlive authority"
    );

    // The kinds the D3b narrowing (2026-06-17) settled as owning nothing
    // poolable AND holding no mapping. If a new one is added it must be
    // classified deliberately, which is what this list is for.
    for obj in [
        CapObject::CpuTime { budget: 100 },
        CapObject::BlockRange { dev: 0, start: 8, count: 4 },
        CapObject::EventSource { id: 0 },
    ] {
        test_assert!(
            release_action(&obj) == ReleaseAction::DropSlot,
            "a capability naming no pooled resource must just vacate its slot"
        );
    }
    Ok(())
}

/// A Reply capability is the one kind release refuses (D3). It names a caller
/// that is Blocked-awaiting-reply, and `capability.rs` leans on that -- "the
/// caller cannot run or exit until replied" is why a Reply needs no generation
/// counter. Releasing one would strand that caller forever, so `cap_release`
/// returns an error and leaves the slot alone; a server that wants out replies,
/// and a server that dies is handled by `ipc::reap_dying`.
pub fn release_action_refuses_reply(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    test_assert!(
        release_action(&CapObject::Reply { caller: 1 }) == ReleaseAction::Refuse,
        "releasing a Reply was allowed -- it would strand the blocked caller"
    );
    Ok(())
}

/// The reuse property the whole fix exists for, at table level: a revoked slot
/// is handed straight back out by the next mint, so a process that releases
/// what it is done with can spawn indefinitely through a 16-slot table. The
/// `revoke` test above already pins one round of this; here it is driven past
/// the table size, which is the shape of the 2026-06-27 crash (the shell got
/// roughly nine launches before the table filled).
pub fn slot_reuse_past_table_size(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut table = CapTable::new();
    // Occupy one slot permanently, the way a process holds its CPU budget.
    table
        .mint(CapObject::CpuTime { budget: 10 }, RIGHT_CONSUME)
        .map_err(|_| "mint of the standing capability failed")?;

    let mut round = 0usize;
    while round < MAX_CAPS * 2 {
        let slot = table
            .mint(CapObject::Endpoint { id: round % 8 }, RIGHT_READ)
            .map_err(|_| "mint failed -- the table filled, so a release did not free its slot")?;
        test_assert!(slot != 0, "the standing capability's slot was handed out");
        table.revoke(slot).map_err(|_| "revoke of a live slot failed")?;
        round += 1;
    }
    Ok(())
}

/// The full ownership story: a frame moves from the allocator into a
/// capability, the capability is revoked, and the frame returns to the
/// allocator. This is the cycle the syscall layer will drive for real
/// processes.
pub fn frame_cap_lifecycle(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut table = CapTable::new();
    let before = ctx.frames.free_frames();

    let addr = ctx.frames.alloc().map_err(|_| "alloc failed")?;
    let slot = table
        .mint(CapObject::Frame { addr }, RIGHT_READ | RIGHT_WRITE | RIGHT_MAP)
        .map_err(|_| "mint failed")?;

    let cap = table.revoke(slot).map_err(|_| "revoke failed")?;
    let CapObject::Frame { addr: revoked_addr } = cap.object else {
        return Err("revoked capability is not a frame");
    };
    test_assert!(revoked_addr == addr, "revoked capability names a different frame");

    ctx.frames.dealloc(revoked_addr).map_err(|_| "dealloc failed")?;
    test_assert!(ctx.frames.free_frames() == before, "frame did not return to the pool");
    Ok(())
}
