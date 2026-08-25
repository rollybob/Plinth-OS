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
mod event_rings;
mod fb_maps;
mod fbcon;
mod frame_alloc;
mod input;
mod iommu;
mod ipc;
mod mouse;
mod pci;
mod scheduler;
mod serial_probe;
mod virtio_blk;

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
    TestCase { name: "capability::event_source_rights", run: capability::event_source_rights },
    TestCase { name: "capability::release_action_per_kind", run: capability::release_action_per_kind },
    TestCase {
        name: "capability::release_action_reclaims_lent_recoverable_kinds",
        run: capability::release_action_reclaims_lent_recoverable_kinds,
    },
    TestCase {
        name: "capability::reclaim_target_sends_lent_recoverable_home",
        run: capability::reclaim_target_sends_lent_recoverable_home,
    },
    TestCase {
        name: "capability::reclaim_declines_when_lender_table_full",
        run: capability::reclaim_declines_when_lender_table_full,
    },
    TestCase {
        name: "capability::reserve_holds_a_slot_against_install",
        run: capability::reserve_holds_a_slot_against_install,
    },
    TestCase {
        name: "capability::reclaim_to_targets_only_its_reserved_slot",
        run: capability::reclaim_to_targets_only_its_reserved_slot,
    },
    TestCase {
        name: "capability::install_home_prefers_reservation_then_falls_back",
        run: capability::install_home_prefers_reservation_then_falls_back,
    },
    TestCase {
        name: "capability::release_action_refuses_reply",
        run: capability::release_action_refuses_reply,
    },
    TestCase {
        name: "capability::origin_recorded_on_transfer",
        run: capability::origin_recorded_on_transfer,
    },
    TestCase {
        name: "capability::origin_clears_on_homecoming",
        run: capability::origin_clears_on_homecoming,
    },
    TestCase {
        name: "capability::relending_preserves_the_root_lenders_claim",
        run: capability::relending_preserves_the_root_lenders_claim,
    },
    TestCase {
        name: "capability::origin_cleared_when_lender_exits",
        run: capability::origin_cleared_when_lender_exits,
    },
    TestCase {
        name: "capability::slot_reuse_past_table_size",
        run: capability::slot_reuse_past_table_size,
    },
    TestCase { name: "fb_maps::record_and_take", run: fb_maps::record_and_take },
    TestCase { name: "fb_maps::take_is_slot_scoped", run: fb_maps::take_is_slot_scoped },
    TestCase { name: "fb_maps::take_collects_duplicates", run: fb_maps::take_collects_duplicates },
    TestCase { name: "fb_maps::full_table_refuses", run: fb_maps::full_table_refuses },
    TestCase { name: "fb_maps::take_absent_is_zero", run: fb_maps::take_absent_is_zero },
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
    TestCase { name: "input::key_event_encoding", run: input::key_event_encoding },
    TestCase { name: "mouse::mouse_event_encoding", run: mouse::mouse_event_encoding },
    TestCase { name: "mouse::mouse_packet_assembles", run: mouse::mouse_packet_assembles },
    TestCase {
        name: "mouse::mouse_packet_buttons_and_signs",
        run: mouse::mouse_packet_buttons_and_signs,
    },
    TestCase { name: "mouse::mouse_packet_resyncs", run: mouse::mouse_packet_resyncs },
    TestCase { name: "mouse::mouse_axis_clamps", run: mouse::mouse_axis_clamps },
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
    TestCase { name: "event_rings::routes_by_source_and_cookie", run: event_rings::routes_by_source_and_cookie },
    TestCase { name: "event_rings::no_subscription_drops", run: event_rings::no_subscription_drops },
    TestCase { name: "event_rings::overflow_drops_newest_and_counts", run: event_rings::overflow_drops_newest_and_counts },
    TestCase { name: "event_rings::drop_flag_on_next_admitted", run: event_rings::drop_flag_on_next_admitted },
    TestCase { name: "event_rings::cancel_stops_delivery", run: event_rings::cancel_stops_delivery },
    TestCase { name: "event_rings::release_ring_clears_all", run: event_rings::release_ring_clears_all },
    TestCase { name: "event_rings::pool_full_and_duplicates_rejected", run: event_rings::pool_full_and_duplicates_rejected },
    TestCase { name: "virtio_blk::inflight_distinct_heads", run: virtio_blk::inflight_distinct_heads },
    TestCase { name: "virtio_blk::inflight_complete_routes", run: virtio_blk::inflight_complete_routes },
    TestCase { name: "virtio_blk::inflight_complete_frees_and_refills", run: virtio_blk::inflight_complete_frees_and_refills },
    TestCase { name: "virtio_blk::inflight_complete_unissued_none", run: virtio_blk::inflight_complete_unissued_none },
    TestCase { name: "virtio_blk::inflight_complete_bad_head", run: virtio_blk::inflight_complete_bad_head },
    TestCase { name: "fbcon::renders_known_string", run: fbcon::renders_known_string },
    TestCase { name: "fbcon::blit_places_pixels", run: fbcon::blit_places_pixels },
    TestCase { name: "fbcon::distinct_glyphs_distinct_hash", run: fbcon::distinct_glyphs_distinct_hash },
    TestCase { name: "fbcon::wrap_and_scroll_stable", run: fbcon::wrap_and_scroll_stable },
    TestCase { name: "pci::classifies_storage_subclasses", run: pci::classifies_storage_subclasses },
    TestCase { name: "pci::ahci_prog_if_distinguished", run: pci::ahci_prog_if_distinguished },
    TestCase { name: "serial::probe_detects_com1", run: serial_probe::probe_detects_com1 },
    TestCase { name: "serial::probe_rejects_absent_port", run: serial_probe::probe_rejects_absent_port },
    TestCase { name: "scheduler::picks_next_ready", run: scheduler::picks_next_ready },
    TestCase { name: "scheduler::skips_empty", run: scheduler::skips_empty },
    TestCase { name: "scheduler::wraps_around", run: scheduler::wraps_around },
    TestCase { name: "scheduler::none_when_alone", run: scheduler::none_when_alone },
    TestCase { name: "scheduler::never_picks_self", run: scheduler::never_picks_self },
    TestCase { name: "scheduler::round_robin_cycle", run: scheduler::round_robin_cycle },
    TestCase {
        name: "scheduler::reclaim_landing_absent_by_default",
        run: scheduler::reclaim_landing_absent_by_default,
    },
    TestCase {
        name: "scheduler::reclaim_landing_take_clears",
        run: scheduler::reclaim_landing_take_clears,
    },
    TestCase {
        name: "scheduler::reclaim_landing_first_write_wins",
        run: scheduler::reclaim_landing_first_write_wins,
    },
    TestCase {
        name: "scheduler::origin_sweep_reaches_running_processes",
        run: scheduler::origin_sweep_reaches_running_processes,
    },
    TestCase {
        name: "scheduler::lender_lookup_finds_running_lender",
        run: scheduler::lender_lookup_finds_running_lender,
    },
    TestCase {
        name: "scheduler::lender_lookup_ignores_idle_core_slot_zero",
        run: scheduler::lender_lookup_ignores_idle_core_slot_zero,
    },
    TestCase {
        name: "scheduler::per_core_state_restored",
        run: scheduler::per_core_state_restored,
    },
    TestCase {
        name: "scheduler::work_steal_skips_donor_current",
        run: scheduler::work_steal_skips_donor_current,
    },
    TestCase { name: "iommu::map_translate_roundtrip", run: iommu::map_translate_roundtrip },
    TestCase { name: "iommu::empty_domain_translates_none", run: iommu::empty_domain_translates_none },
    TestCase { name: "iommu::rejects_bad_requests", run: iommu::rejects_bad_requests },
    TestCase { name: "iommu::teardown_frees_every_table", run: iommu::teardown_frees_every_table },
    TestCase { name: "iommu::rejects_unsupported_width", run: iommu::rejects_unsupported_width },
    TestCase { name: "iommu::context_entry_encoding", run: iommu::context_entry_encoding },
    TestCase { name: "iommu::translation_tables_teardown_frees", run: iommu::translation_tables_teardown_frees },
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
