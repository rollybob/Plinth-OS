//! Framebuffer mapping records (D2/D7).
//!
//! The bookkeeping half of "a live framebuffer mapping exists only while the
//! process holds a capability naming it". The unmapping itself needs a live
//! address space and so is out of the harness's reach; these cover the pure
//! record-keeping that decides *which* pages get unmapped, which is where an
//! off-by-one or a missed duplicate would actually hide.

use crate::process::{fb_record, fb_take_slot, FbMap, MAX_FB_MAPS};
use crate::test_assert;
use crate::tests::TestCtx;

fn empty() -> [Option<FbMap>; MAX_FB_MAPS] {
    [None; MAX_FB_MAPS]
}

fn rec(va_base: u64, pages: u32, slot: usize) -> FbMap {
    FbMap { va_base, pages, slot }
}

/// A record goes in and comes back out addressed by its slot, carrying the
/// page count intact -- the count is what the unmap loop walks.
pub fn record_and_take(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut maps = empty();
    test_assert!(fb_record(&mut maps, rec(0x1000_0000, 1000, 1)), "first record was refused");

    let mut out = [rec(0, 0, 0); MAX_FB_MAPS];
    let n = fb_take_slot(&mut maps, 1, &mut out);
    test_assert!(n == 1, "expected exactly one record for the slot");
    test_assert!(out[0].va_base == 0x1000_0000, "wrong base address returned");
    test_assert!(out[0].pages == 1000, "wrong page count returned");
    test_assert!(maps.iter().all(|e| e.is_none()), "taking a record must clear it");
    Ok(())
}

/// Taking one slot must not disturb another's records. A band holder and a
/// whole-screen holder can coexist in one table, and releasing one capability
/// must not unmap the other's pages.
pub fn take_is_slot_scoped(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut maps = empty();
    test_assert!(fb_record(&mut maps, rec(0x1000_0000, 500, 1)), "record 1 refused");
    test_assert!(fb_record(&mut maps, rec(0x1100_0000, 500, 2)), "record 2 refused");

    let mut out = [rec(0, 0, 0); MAX_FB_MAPS];
    let n = fb_take_slot(&mut maps, 1, &mut out);
    test_assert!(n == 1, "slot 1 should have exactly one record");
    test_assert!(out[0].va_base == 0x1000_0000, "took the wrong record");

    let n2 = fb_take_slot(&mut maps, 2, &mut out);
    test_assert!(n2 == 1, "slot 2's record must survive slot 1 being taken");
    test_assert!(out[0].va_base == 0x1100_0000, "slot 2's record was corrupted");
    Ok(())
}

/// One capability mapped at two addresses yields two records, and releasing it
/// must collect BOTH -- leaving one behind would leave a live mapping with no
/// capability, which is the whole defect being closed.
pub fn take_collects_duplicates(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut maps = empty();
    test_assert!(fb_record(&mut maps, rec(0x1000_0000, 10, 3)), "record a refused");
    test_assert!(fb_record(&mut maps, rec(0x1200_0000, 10, 3)), "record b refused");
    test_assert!(fb_record(&mut maps, rec(0x1400_0000, 10, 4)), "record c refused");

    let mut out = [rec(0, 0, 0); MAX_FB_MAPS];
    let n = fb_take_slot(&mut maps, 3, &mut out);
    test_assert!(n == 2, "both mappings through slot 3 must be collected");
    test_assert!(
        maps.iter().flatten().count() == 1,
        "the unrelated slot 4 record must remain"
    );
    Ok(())
}

/// The array is fixed, so a full table refuses rather than overwriting. This is
/// the condition `sys_fb_map` checks BEFORE mapping ~1000 pages, so that a full
/// table is an error return and never an untracked mapping.
pub fn full_table_refuses(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut maps = empty();
    for i in 0..MAX_FB_MAPS {
        test_assert!(fb_record(&mut maps, rec(0x1000_0000, 1, i)), "record refused early");
    }
    test_assert!(
        !fb_record(&mut maps, rec(0x9000_0000, 1, 9)),
        "a full table must refuse a new record"
    );

    // And a release frees capacity again, so a process that cycles mappings
    // does not wedge itself.
    let mut out = [rec(0, 0, 0); MAX_FB_MAPS];
    test_assert!(fb_take_slot(&mut maps, 0, &mut out) == 1, "expected to reclaim one record");
    test_assert!(
        fb_record(&mut maps, rec(0x9000_0000, 1, 9)),
        "a reclaimed record must be reusable"
    );
    Ok(())
}

/// Taking a slot with no records is not an error -- it is the ordinary case for
/// every non-framebuffer release, and for a capability that was never mapped.
pub fn take_absent_is_zero(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut maps = empty();
    test_assert!(fb_record(&mut maps, rec(0x1000_0000, 1, 1)), "record refused");
    let mut out = [rec(0, 0, 0); MAX_FB_MAPS];
    test_assert!(fb_take_slot(&mut maps, 7, &mut out) == 0, "absent slot must yield nothing");
    test_assert!(maps.iter().flatten().count() == 1, "an unrelated record was disturbed");
    Ok(())
}
