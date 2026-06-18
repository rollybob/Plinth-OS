//! In-kernel test harness.
//!
//! Compiled only with `--features tests`. The suite runs right after the
//! frame allocator comes up, and the kernel exits QEMU as soon as it
//! finishes -- a test build never proceeds to normal boot. xtask drives
//! this via `cargo xtask test` and parses the tag lines below:
//!
//!   [TEST] name            test starting
//!   [PASS] name            test returned Ok(())
//!   [FAIL] name: reason    test returned Err(reason)
//!   [SUITE] N passed, M failed

mod capability;
mod elf;
mod frame_alloc;
mod ipc;
mod scheduler;

use crate::frame_alloc::FrameAlloc;
use crate::serial;
use core::fmt::Write;

/// Shared state handed to every test. Only the frame allocator is
/// shared; everything else a test needs, it builds fresh.
pub struct TestCtx<'a> {
    pub frames: &'a mut FrameAlloc,
}

pub struct TestCase {
    pub name: &'static str,
    /// Two independent lifetimes, deliberately: with a single
    /// `for<'a> fn(&'a mut TestCtx<'a>)` every reborrow in the runner
    /// loop would have to live as long as the context itself, which the
    /// borrow checker rejects after the first iteration.
    pub run: for<'a, 'b> fn(&'b mut TestCtx<'a>) -> Result<(), &'static str>,
}

/// Return Err(msg) from the surrounding *test function* if the condition
/// fails. Do not call inside a closure: the early return would exit the
/// closure, not the test.
#[macro_export]
macro_rules! test_assert {
    ($cond:expr, $msg:expr) => {
        if !($cond) {
            return Err($msg);
        }
    };
}

const TESTS: &[TestCase] = &[
    TestCase { name: "frame_alloc::roundtrip", run: frame_alloc::roundtrip },
    TestCase { name: "frame_alloc::unique", run: frame_alloc::unique },
    TestCase { name: "frame_alloc::double_free", run: frame_alloc::double_free },
    TestCase { name: "frame_alloc::out_of_range", run: frame_alloc::out_of_range },
    TestCase { name: "capability::mint_lookup", run: capability::mint_lookup },
    TestCase { name: "capability::rights_denied", run: capability::rights_denied },
    TestCase { name: "capability::revoke", run: capability::revoke },
    TestCase { name: "capability::table_full", run: capability::table_full },
    TestCase { name: "capability::bad_slot", run: capability::bad_slot },
    TestCase { name: "capability::frame_cap_lifecycle", run: capability::frame_cap_lifecycle },
    TestCase { name: "capability::cpu_charge_lifecycle", run: capability::cpu_charge_lifecycle },
    TestCase { name: "capability::cpu_charge_rights_denied", run: capability::cpu_charge_rights_denied },
    TestCase { name: "capability::cpu_charge_wrong_type", run: capability::cpu_charge_wrong_type },
    TestCase { name: "capability::block_range_rights", run: capability::block_range_rights },
    TestCase { name: "elf::valid_minimal", run: elf::valid_minimal },
    TestCase { name: "elf::valid_three_segments", run: elf::valid_three_segments },
    TestCase { name: "elf::too_small", run: elf::too_small },
    TestCase { name: "elf::bad_magic", run: elf::bad_magic },
    TestCase { name: "elf::bad_class", run: elf::bad_class },
    TestCase { name: "elf::not_exec", run: elf::not_exec },
    TestCase { name: "elf::bad_machine", run: elf::bad_machine },
    TestCase { name: "elf::phdrs_out_of_bounds", run: elf::phdrs_out_of_bounds },
    TestCase { name: "elf::segment_file_range", run: elf::segment_file_range },
    TestCase { name: "elf::segment_sizes", run: elf::segment_sizes },
    TestCase { name: "elf::segment_unaligned", run: elf::segment_unaligned },
    TestCase { name: "elf::segment_out_of_window", run: elf::segment_out_of_window },
    TestCase { name: "elf::wx_violation", run: elf::wx_violation },
    TestCase { name: "elf::bad_flags_unreadable", run: elf::bad_flags_unreadable },
    TestCase { name: "elf::dynamic_interp", run: elf::dynamic_interp },
    TestCase { name: "elf::no_loadable", run: elf::no_loadable },
    TestCase { name: "elf::too_large", run: elf::too_large },
    TestCase { name: "elf::bad_entry", run: elf::bad_entry },
    TestCase { name: "elf::segment_overlap", run: elf::segment_overlap },
    TestCase { name: "elf::bad_phentsize", run: elf::bad_phentsize },
    TestCase { name: "elf::too_many_phdrs", run: elf::too_many_phdrs },
    TestCase { name: "elf::phoff_overflow", run: elf::phoff_overflow },
    TestCase { name: "elf::segment_file_offset_overflow", run: elf::segment_file_offset_overflow },
    TestCase { name: "elf::segment_vaddr_overflow", run: elf::segment_vaddr_overflow },
    TestCase { name: "ipc::wq_fifo_order", run: ipc::wq_fifo_order },
    TestCase { name: "ipc::wq_single", run: ipc::wq_single },
    TestCase { name: "ipc::wq_take_matches_sender_side", run: ipc::wq_take_matches_sender_side },
    TestCase { name: "ipc::wq_take_matches_receiver_side", run: ipc::wq_take_matches_receiver_side },
    TestCase { name: "ipc::wq_take_empty", run: ipc::wq_take_empty },
    TestCase { name: "ipc::wq_refill_other_side", run: ipc::wq_refill_other_side },
    TestCase { name: "ipc::wq_is_empty", run: ipc::wq_is_empty },
    TestCase { name: "ipc::ep_refcount_sender", run: ipc::ep_refcount_sender },
    TestCase { name: "ipc::ep_refcount_receiver", run: ipc::ep_refcount_receiver },
    TestCase { name: "ipc::ep_refcount_directional_split", run: ipc::ep_refcount_directional_split },
    TestCase { name: "ipc::ep_refcount_multiple_same_side", run: ipc::ep_refcount_multiple_same_side },
    TestCase { name: "ipc::ep_refcount_dual_right_cap", run: ipc::ep_refcount_dual_right_cap },
    TestCase { name: "ipc::ep_strand_last_side", run: ipc::ep_strand_last_side },
    TestCase { name: "ipc::ep_strand_not_last", run: ipc::ep_strand_not_last },
    TestCase { name: "ipc::ep_strand_no_reference", run: ipc::ep_strand_no_reference },
    TestCase { name: "scheduler::picks_next_ready", run: scheduler::picks_next_ready },
    TestCase { name: "scheduler::skips_empty", run: scheduler::skips_empty },
    TestCase { name: "scheduler::wraps_around", run: scheduler::wraps_around },
    TestCase { name: "scheduler::none_when_alone", run: scheduler::none_when_alone },
    TestCase { name: "scheduler::never_picks_self", run: scheduler::never_picks_self },
    TestCase { name: "scheduler::round_robin_cycle", run: scheduler::round_robin_cycle },
];

/// Run every registered test. Returns true if all passed.
pub fn run_all(ctx: &mut TestCtx) -> bool {
    let mut serial = serial::init();
    let mut passed = 0u32;
    let mut failed = 0u32;

    for t in TESTS {
        let _ = writeln!(serial, "[TEST] {}", t.name);
        match (t.run)(ctx) {
            Ok(()) => {
                passed += 1;
                let _ = writeln!(serial, "[PASS] {}", t.name);
            }
            Err(msg) => {
                failed += 1;
                let _ = writeln!(serial, "[FAIL] {}: {}", t.name, msg);
            }
        }
    }

    let _ = writeln!(serial, "[SUITE] {} passed, {} failed", passed, failed);
    failed == 0
}
