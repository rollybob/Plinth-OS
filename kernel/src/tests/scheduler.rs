//! Unit tests for the scheduler's ready-queue policy.
//!
//! `pick_next` is the one piece of scheduling logic that is a pure function of
//! the slot states, so it is tested directly -- the way `elf::parse` is tested
//! without ever entering userspace. The context-switch mechanism itself is
//! exercised by the integration smoke (the interleaving spin demo); there is
//! no way to assert an exact preemptive trace as a unit test, by design (see
//! Design/timer_scheduler.md section 2).

use super::TestCtx;
use crate::capability::{CapObject, Capability, RIGHT_MAP, RIGHT_WRITE};
use crate::process::{self, Process};
use crate::scheduler::{
    self, clear_origins_naming, lender_cap_count, pick_next, swap_current_slot, State,
    MAX_PROCESSES,
};
use crate::test_assert;

/// A process holding one framebuffer capability lent by `lender`.
fn borrower_of(lender: Option<usize>) -> Process {
    let mut p = Process::new();
    p.caps
        .install(Capability {
            object: CapObject::Framebuffer {
                phys_base: 0x8000_0000,
                width: 1280,
                height: 800,
                stride: 1280,
                bytes_per_pixel: 4,
                format: 1,
            },
            rights: RIGHT_MAP | RIGHT_WRITE,
            origin: lender,
        })
        .expect("fresh table");
    p
}

/// A core the test suite is definitely not running on. The suite runs on the BSP
/// before any AP is scheduling, so every other core's `CURRENT` is free.
const PARKED_CORE: usize = 3;

/// **The test the SMP reach bug slipped past.**
///
/// `clear_origins_naming` must reach a *running* process. A running process is
/// not in the scheduler's `TABLE` -- `resume_process` moves its `Process` into
/// the per-core `process::CURRENT` -- so the first cut of that sweep, which
/// walked `TABLE` alone, silently skipped any borrower executing on another core.
/// Its `origin` then outlived its lender, the slot was recycled, and reclamation
/// would have handed the screen to a process that never lent anything.
///
/// The existing `capability::origin_cleared_when_lender_exits` cannot catch that:
/// it exercises `CapTable::clear_origin`, one layer BELOW where the bug lived.
/// This one drives the scheduler function itself, with a borrower staged where
/// only a running process lives. Assert the returned COUNT, not just the field --
/// an undercount is the cheapest signal that the reach is wrong.
pub fn origin_sweep_reaches_running_processes(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    const LENDER: usize = 2;
    let prev = process::swap_current_on_core(PARKED_CORE, Some(borrower_of(Some(LENDER))));

    let cleared = clear_origins_naming(LENDER);

    // Read the staged process back before restoring, so the assertion sees the
    // table the sweep actually touched.
    let staged = process::swap_current_on_core(PARKED_CORE, prev);
    let origin_after = staged
        .as_ref()
        .and_then(|p| p.caps.iter().next())
        .map(|c| c.origin)
        .ok_or("staged borrower vanished")?;

    test_assert!(
        cleared == 1,
        "the sweep did not reach a RUNNING process -- it must walk the per-core \
         CURRENT as well as TABLE, or a borrower on another core keeps a stale lender"
    );
    test_assert!(origin_after.is_none(), "the stale origin survived the sweep");
    Ok(())
}

/// The other half of the same bug: finding a lender that is *running*.
///
/// A `TABLE`-only lookup returns "no such lender", and `reclaim_lent_caps` used
/// to read that as the capability simply having nowhere to go -- which is also
/// what a legitimately full table looks like (D4). A wrong answer wearing a
/// correct one's clothes, which is why those two outcomes are now distinct arms.
pub fn lender_lookup_finds_running_lender(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    const LENDER: usize = 2;
    // Nothing occupies slot LENDER in TABLE, so a TABLE-only lookup finds nothing.
    test_assert!(
        lender_cap_count(LENDER).is_none(),
        "precondition: no suspended process should occupy the lender slot"
    );

    // Stage the lender as RUNNING: its Process parked in a core's CURRENT, and
    // CURRENT_SLOT saying which table slot that core is running.
    let prev_proc = process::swap_current_on_core(PARKED_CORE, Some(borrower_of(None)));
    let prev_slot = swap_current_slot(PARKED_CORE, LENDER);

    let found = lender_cap_count(LENDER);

    swap_current_slot(PARKED_CORE, prev_slot);
    process::swap_current_on_core(PARKED_CORE, prev_proc);

    test_assert!(
        found == Some(1),
        "a RUNNING lender was not found -- reclamation would have destroyed the \
         capability and been indistinguishable from a full table"
    );
    Ok(())
}

/// An idle core's `CURRENT_SLOT` still reads 0 from initialisation, so a lookup
/// that matched on that map alone would false-hit slot 0 and give up on a lender
/// that was really elsewhere. Pinned because the loop is written to tolerate it.
pub fn lender_lookup_ignores_idle_core_slot_zero(
    _ctx: &mut TestCtx,
) -> Result<(), &'static str> {
    const LENDER: usize = 0;
    // PARKED_CORE claims to be running slot 0 but holds no process at all.
    let prev_slot = swap_current_slot(PARKED_CORE, LENDER);
    let prev_proc = process::swap_current_on_core(PARKED_CORE, None);
    // A different core genuinely runs slot 0.
    let other = PARKED_CORE + 1;
    let prev_other_slot = swap_current_slot(other, LENDER);
    let prev_other = process::swap_current_on_core(other, Some(borrower_of(None)));

    let found = lender_cap_count(LENDER);

    process::swap_current_on_core(other, prev_other);
    swap_current_slot(other, prev_other_slot);
    process::swap_current_on_core(PARKED_CORE, prev_proc);
    swap_current_slot(PARKED_CORE, prev_slot);

    test_assert!(
        found == Some(1),
        "an idle core advertising slot 0 masked the core really running it"
    );
    Ok(())
}

/// Belt and braces: the suite must leave per-core state exactly as it found it,
/// or a later test (or the kernel itself) inherits a phantom process. Runs after
/// the three above.
pub fn per_core_state_restored(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    for core in 0..crate::percpu::MAX_CORES {
        let taken = process::swap_current_on_core(core, None);
        let was_empty = taken.is_none();
        process::swap_current_on_core(core, taken);
        test_assert!(was_empty, "a staged process was left behind in a core's CURRENT");
    }
    let _ = scheduler::current_slot();
    Ok(())
}

/// Build a state array from a slice, padding the rest with Empty.
fn slots(init: &[State]) -> [State; MAX_PROCESSES] {
    let mut s = [State::Empty; MAX_PROCESSES];
    for (i, &st) in init.iter().enumerate() {
        s[i] = st;
    }
    s
}

/// The next Ready slot immediately after the running one is chosen.
pub fn picks_next_ready(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let s = slots(&[State::Running, State::Ready, State::Ready, State::Ready]);
    test_assert!(pick_next(&s, 0) == Some(1), "expected slot 1 after 0");
    test_assert!(pick_next(&s, 1) == Some(2), "expected slot 2 after 1");
    Ok(())
}

/// Empty slots are skipped.
pub fn skips_empty(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let s = slots(&[State::Running, State::Empty, State::Ready]);
    test_assert!(pick_next(&s, 0) == Some(2), "should skip the empty slot 1");
    Ok(())
}

/// The search wraps past the end of the table back to the start.
pub fn wraps_around(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    // current = 3 (last); only slot 0 is Ready.
    let s = slots(&[State::Ready, State::Empty, State::Empty, State::Running]);
    test_assert!(pick_next(&s, 3) == Some(0), "should wrap to slot 0");
    Ok(())
}

/// With no other runnable process, the running one is kept (None = no switch).
pub fn none_when_alone(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let s = slots(&[State::Running, State::Empty, State::Empty, State::Empty]);
    test_assert!(pick_next(&s, 0).is_none(), "no other Ready -> None");
    Ok(())
}

/// A process never selects itself, even if its own slot were marked Ready.
pub fn never_picks_self(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut s = slots(&[State::Empty, State::Empty, State::Empty, State::Empty]);
    s[1] = State::Ready; // pretend the current slot is Ready
    test_assert!(pick_next(&s, 1).is_none(), "must not return current");
    Ok(())
}

/// Round-robin is fair: starting from each position, the selection advances by
/// one each time around a fully-Ready table.
pub fn round_robin_cycle(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let s = slots(&[State::Ready, State::Ready, State::Ready, State::Ready]);
    test_assert!(pick_next(&s, 0) == Some(1), "0 -> 1");
    test_assert!(pick_next(&s, 1) == Some(2), "1 -> 2");
    test_assert!(pick_next(&s, 2) == Some(3), "2 -> 3");
    test_assert!(pick_next(&s, 3) == Some(0), "3 -> 0 (wrap)");
    Ok(())
}
