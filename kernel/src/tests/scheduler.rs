//! Unit tests for the scheduler's ready-queue policy.
//!
//! `pick_next` is the one piece of scheduling logic that is a pure function of
//! the slot states, so it is tested directly -- the way `elf::parse` is tested
//! without ever entering userspace. The context-switch mechanism itself is
//! exercised by the integration smoke (the interleaving spin demo); there is
//! no way to assert an exact preemptive trace as a unit test, by design (see
//! Design/timer_scheduler.md section 2).

use super::TestCtx;
use crate::scheduler::{pick_next, State, MAX_PROCESSES};
use crate::test_assert;

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
