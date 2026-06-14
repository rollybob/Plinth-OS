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
