//! Frame allocator tests.

use super::TestCtx;
use crate::frame_alloc::{FrameError, FRAME_SIZE};
use crate::test_assert;

pub fn roundtrip(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let before = ctx.frames.free_frames();
    let addr = ctx.frames.alloc().map_err(|_| "alloc failed")?;
    test_assert!(addr % FRAME_SIZE == 0, "frame address not aligned");
    test_assert!(ctx.frames.free_frames() == before - 1, "free count did not drop");
    ctx.frames.dealloc(addr).map_err(|_| "dealloc failed")?;
    test_assert!(ctx.frames.free_frames() == before, "free count not restored");
    Ok(())
}

pub fn unique(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let a = ctx.frames.alloc().map_err(|_| "first alloc failed")?;
    let b = ctx.frames.alloc().map_err(|_| "second alloc failed")?;
    let distinct = a != b;
    ctx.frames.dealloc(a).map_err(|_| "dealloc a failed")?;
    ctx.frames.dealloc(b).map_err(|_| "dealloc b failed")?;
    test_assert!(distinct, "two live allocations returned the same frame");
    Ok(())
}

pub fn double_free(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let addr = ctx.frames.alloc().map_err(|_| "alloc failed")?;
    ctx.frames.dealloc(addr).map_err(|_| "first dealloc failed")?;
    test_assert!(
        ctx.frames.dealloc(addr) == Err(FrameError::NotAllocated),
        "double free was not rejected"
    );
    Ok(())
}

pub fn out_of_range(ctx: &mut TestCtx) -> Result<(), &'static str> {
    test_assert!(
        ctx.frames.dealloc(3) == Err(FrameError::OutOfRange),
        "unaligned address was accepted"
    );
    let beyond = u64::MAX & !(FRAME_SIZE - 1);
    test_assert!(
        ctx.frames.dealloc(beyond) == Err(FrameError::OutOfRange),
        "address beyond tracked memory was accepted"
    );
    Ok(())
}
