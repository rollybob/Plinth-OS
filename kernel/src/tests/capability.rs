//! Capability table tests.

use super::TestCtx;
use crate::capability::{
    CapError, CapObject, CapTable, MAX_CAPS, RIGHT_CONSUME, RIGHT_MAP, RIGHT_READ, RIGHT_WRITE,
};
use crate::test_assert;

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
