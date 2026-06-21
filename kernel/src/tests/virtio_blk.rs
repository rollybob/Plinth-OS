//! Unit tests for the virtio-blk in-flight pool + completion demux.
//!
//! `Inflights` is the completion-routing core (Design/async_rings.md section 5)
//! factored out of the live driver so it is pure logic over plain arrays -- no
//! MMIO, no device, no scheduler -- the same move that makes `ipc::WaitQueue`
//! and `scheduler::pick_next` testable. Here we drive it the way a completion
//! IRQ would: claim slots (submit), then route the device's echoed chain heads
//! back to their completion targets (complete) in arbitrary order. The full
//! block path is exercised end to end by the `blk`/`fs` smoke; these pin the
//! demux invariant the smoke cannot isolate -- every issued head routes to
//! exactly the target that issued it, exactly once.

use super::TestCtx;
use crate::test_assert;
use crate::virtio_blk::{Completion, Inflight, Inflights};

/// A `Completion::Ring` with a distinct cookie, so a routed completion is
/// identifiable by which submission it came from.
fn ring(n: u64) -> Inflight {
    Inflight { target: Completion::Ring { ring: n as usize, user_data: n } }
}

/// Heads handed out are distinct, chain-aligned, in range, and the pool refuses
/// once every slot is claimed.
pub fn inflight_distinct_heads(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut pool = Inflights::new(4);
    let mut heads = [0u16; 4];
    for (i, h) in heads.iter_mut().enumerate() {
        *h = pool.submit(ring(i as u64)).ok_or("submit should succeed")?;
        test_assert!(*h % 3 == 0, "head must be chain-aligned");
        test_assert!((*h / 3) < 4, "head must be within the slot count");
    }
    // All four heads distinct.
    for i in 0..4 {
        for j in (i + 1)..4 {
            test_assert!(heads[i] != heads[j], "heads must be distinct");
        }
    }
    test_assert!(pool.submit(ring(9)).is_none(), "full pool refuses");
    Ok(())
}

/// Each completion routes back to the target that issued it, regardless of the
/// order completions arrive in.
pub fn inflight_complete_routes(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut pool = Inflights::new(4);
    let h0 = pool.submit(ring(10)).ok_or("submit 0")?;
    let h1 = pool.submit(ring(11)).ok_or("submit 1")?;
    let h2 = pool.submit(ring(12)).ok_or("submit 2")?;
    // Complete out of submission order: the routing key is the head, not order.
    test_assert!(pool.complete(h1) == Some(ring(11)), "h1 -> cookie 11");
    test_assert!(pool.complete(h0) == Some(ring(10)), "h0 -> cookie 10");
    test_assert!(pool.complete(h2) == Some(ring(12)), "h2 -> cookie 12");
    Ok(())
}

/// A completed slot returns to the pool and can be reissued; the pool tracks
/// occupancy across the full claim/complete/reclaim cycle.
pub fn inflight_complete_frees_and_refills(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut pool = Inflights::new(2);
    let a = pool.submit(ring(1)).ok_or("submit a")?;
    let _b = pool.submit(ring(2)).ok_or("submit b")?;
    test_assert!(pool.submit(ring(3)).is_none(), "pool full after 2");
    test_assert!(pool.any_live(), "live while requests outstanding");

    test_assert!(pool.complete(a) == Some(ring(1)), "a completes to cookie 1");
    // A slot freed -> a new submit succeeds again.
    let c = pool.submit(ring(3)).ok_or("submit c after free")?;
    test_assert!(pool.complete(c) == Some(ring(3)), "reissued slot routes to 3");
    Ok(())
}

/// A head the kernel never issued -- or one already completed -- routes to
/// nothing (a device/spec violation the driver drops rather than mis-routing).
pub fn inflight_complete_unissued_none(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut pool = Inflights::new(4);
    let h = pool.submit(ring(5)).ok_or("submit")?;
    // A different, in-range, chain-aligned head that was never claimed.
    let other = if h == 0 { 3 } else { 0 };
    test_assert!(pool.complete(other).is_none(), "unissued head routes to nothing");
    test_assert!(pool.complete(h) == Some(ring(5)), "the live head routes");
    test_assert!(pool.complete(h).is_none(), "double-complete routes to nothing");
    Ok(())
}

/// A malformed head -- not chain-aligned, or past the slot count -- is rejected
/// without indexing out of bounds.
pub fn inflight_complete_bad_head(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut pool = Inflights::new(4);
    let _ = pool.submit(ring(7)).ok_or("submit")?;
    test_assert!(pool.complete(1).is_none(), "non-chain-aligned head rejected");
    test_assert!(pool.complete(2).is_none(), "non-chain-aligned head rejected");
    test_assert!(pool.complete(99).is_none(), "out-of-range head rejected");
    Ok(())
}
