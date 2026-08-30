//! Unit tests for the xHCI pure encoding layer (`crate::xhci`).
//!
//! These pin the deterministic data structures the USB HID driver builds and
//! reads before any controller exists: TRB field packing, the command-TRB
//! constructors, event-TRB decoding, the producer ring's Link-TRB wrap + cycle
//! toggle, the event-ring consumer's cycle-follow, the ERST entry, and the
//! 32-byte Slot/Endpoint/Input context field layouts. No device, no MMIO -- the
//! same "pure structure over injected backing" discipline as the IOMMU tests,
//! with the backing here being plain stack arrays (an event ring is read through
//! a raw pointer, exactly as the DMA path will).

use super::TestCtx;
use crate::test_assert;
use crate::xhci::{
    dci, EndpointContext, EpType, ErstEntry, EventRing, InputControlContext, ProducerRing,
    SlotContext, Trb, TrbType, SPEED_FULL, SPEED_HIGH, SPEED_LOW, SPEED_SUPER,
};
use core::mem::size_of;

/// TRBs are 16 bytes and contexts 32 bytes; the DMA layout depends on it.
pub fn struct_sizes(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    test_assert!(size_of::<Trb>() == 16, "TRB must be 16 bytes");
    test_assert!(size_of::<SlotContext>() == 32, "slot context must be 32 bytes");
    test_assert!(size_of::<EndpointContext>() == 32, "endpoint context must be 32 bytes");
    test_assert!(size_of::<InputControlContext>() == 32, "input control context must be 32 bytes");
    Ok(())
}

/// Generic TRB type/cycle accessors round-trip, and each command constructor sets
/// its type and payload without disturbing the cycle bit (the ring stamps that).
pub fn trb_fields_and_commands(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut t = Trb::zeroed();
    test_assert!(!t.cycle(), "zeroed TRB has cycle clear");
    t.set_cycle(true);
    test_assert!(t.cycle(), "set_cycle(true) sets the cycle bit");
    t.set_cycle(false);
    test_assert!(!t.cycle(), "set_cycle(false) clears it");

    // Link TRB: points at the wrap target, carries Toggle Cycle, type Link.
    let base = 0x0002_0000u64;
    let link = Trb::link(base, true);
    test_assert!(link.trb_type() == TrbType::Link as u8, "link TRB type");
    test_assert!(link.param == base, "link TRB points at the ring base");
    test_assert!(link.control & (1 << 1) != 0, "link TRB carries Toggle Cycle");
    test_assert!(!link.cycle(), "constructor leaves the cycle clear for the ring to stamp");
    let link_no_toggle = Trb::link(base, false);
    test_assert!(link_no_toggle.control & (1 << 1) == 0, "no-toggle link clears TC");

    // Enable Slot.
    let en = Trb::enable_slot();
    test_assert!(en.trb_type() == TrbType::EnableSlot as u8, "enable slot type");

    // Address Device: input-ctx pointer in param, slot id in control 24-31, BSR.
    let ictx = 0x0003_1000u64;
    let ad = Trb::address_device(ictx, 7, true);
    test_assert!(ad.trb_type() == TrbType::AddressDevice as u8, "address device type");
    test_assert!(ad.param == ictx, "address device carries the input context ptr");
    test_assert!(ad.event_slot_id() == 7, "address device slot id (control 24-31)");
    test_assert!(ad.control & (1 << 9) != 0, "address device BSR bit set");
    let ad_no_bsr = Trb::address_device(ictx, 3, false);
    test_assert!(ad_no_bsr.control & (1 << 9) == 0, "no-BSR clears bit 9");
    test_assert!(ad_no_bsr.event_slot_id() == 3, "slot id encodes independently of BSR");

    // Configure Endpoint.
    let ce = Trb::configure_endpoint(ictx, 5);
    test_assert!(ce.trb_type() == TrbType::ConfigureEndpoint as u8, "configure endpoint type");
    test_assert!(ce.event_slot_id() == 5, "configure endpoint slot id");
    Ok(())
}

/// The event-TRB decoders read the fields the controller writes for the three
/// events bring-up cares about: command completion, transfer, port status change.
pub fn event_decode(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    // Command Completion: code in status 24-31, slot id in control 24-31, the
    // completed command's TRB pointer in param.
    let cmd_ptr = 0x0002_0010u64;
    let mut cc = Trb::zeroed();
    cc.param = cmd_ptr;
    cc.status = (1u32) << 24; // completion code 1 = Success
    cc.control = ((TrbType::CommandCompletion as u32) << 10) | ((9u32) << 24);
    test_assert!(cc.trb_type() == TrbType::CommandCompletion as u8, "cc type");
    test_assert!(cc.completion_code() == 1, "cc success code");
    test_assert!(cc.event_slot_id() == 9, "cc slot id");
    test_assert!(cc.trb_pointer() == cmd_ptr, "cc points at its command TRB");

    // Transfer Event: length in status 0-23, code 24-31, endpoint id (DCI) in
    // control 16-20, slot id 24-31.
    let td_ptr = 0x0004_0040u64;
    let mut te = Trb::zeroed();
    te.param = td_ptr;
    te.status = ((1u32) << 24) | 6; // code 1, transferred 6 bytes
    te.control = ((TrbType::TransferEvent as u32) << 10) | ((3u32) << 16) | ((9u32) << 24);
    test_assert!(te.trb_type() == TrbType::TransferEvent as u8, "te type");
    test_assert!(te.completion_code() == 1, "te code");
    test_assert!(te.transfer_length() == 6, "te transferred length");
    test_assert!(te.event_endpoint_id() == 3, "te endpoint id / DCI");
    test_assert!(te.event_slot_id() == 9, "te slot id");
    test_assert!(te.trb_pointer() == td_ptr, "te points at the transfer TRB");

    // Port Status Change: port id in dword0 bits 24-31.
    let mut ps = Trb::zeroed();
    ps.param = (2u64) << 24;
    ps.control = (TrbType::PortStatusChange as u32) << 10;
    test_assert!(ps.trb_type() == TrbType::PortStatusChange as u8, "ps type");
    test_assert!(ps.port_id() == 2, "ps port id");
    Ok(())
}

/// The load-bearing ring test: pushes fill usable slots stamped with the PCS;
/// reaching the Link slot arms the link with the PCS and wraps with a PCS toggle,
/// so the next lap stamps the opposite cycle. Also checks the returned slot
/// addresses (a command completion later points back to them).
pub fn producer_ring_wraps_and_toggles_cycle(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let base = 0x0002_0000u64;
    let mut buf = [Trb::zeroed(); 4]; // 3 usable slots + 1 link
    // Safety: buf outlives `ring` and is 4 contiguous TRBs; the ring drives it
    // through the raw pointer (the DMA-memory model), tests inspect via peek().
    let mut ring = unsafe { ProducerRing::new(buf.as_mut_ptr(), buf.len(), base) };
    test_assert!(ring.pcs(), "PCS starts set");
    test_assert!(ring.enqueue_index() == 0, "enqueue starts at 0");

    let a0 = ring.push(Trb::enable_slot());
    let a1 = ring.push(Trb::enable_slot());
    let a2 = ring.push(Trb::enable_slot());
    test_assert!(a0 == base, "first slot address");
    test_assert!(a1 == base + 16, "second slot address");
    test_assert!(a2 == base + 32, "third slot address");

    // The third push filled the last usable slot -> wrap happened.
    test_assert!(ring.enqueue_index() == 0, "enqueue wrapped to 0");
    test_assert!(!ring.pcs(), "PCS toggled after the wrap");
    for i in 0..3 {
        test_assert!(ring.peek(i).cycle(), "lap-1 data TRBs stamped cycle 1");
        test_assert!(ring.peek(i).trb_type() == TrbType::EnableSlot as u8, "data TRB type kept");
    }
    let link = ring.peek(3);
    test_assert!(link.trb_type() == TrbType::Link as u8, "slot 3 is the link");
    test_assert!(link.cycle(), "link armed with the lap-1 PCS");
    test_assert!(link.control & (1 << 1) != 0, "link keeps Toggle Cycle");
    test_assert!(link.param == base, "link wraps to the ring base");

    // Next lap stamps the opposite cycle at slot 0.
    let a3 = ring.push(Trb::enable_slot());
    test_assert!(a3 == base, "lap-2 wraps back to slot 0");
    test_assert!(!ring.peek(0).cycle(), "lap-2 stamps cycle 0");
    test_assert!(ring.enqueue_index() == 1, "enqueue advanced after lap-2 push");
    Ok(())
}

/// The event-ring consumer returns events only when their cycle matches the CCS,
/// advances the dequeue pointer, toggles the CCS on wrap, and reports None when
/// the current slot has not been written this lap.
pub fn event_ring_consumes_and_toggles(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [Trb::zeroed(); 3];
    // Safety: buf outlives `er`, is 3 contiguous readable TRBs; we mutate buf to
    // stand in for the controller's DMA writes, reading it back through the ring.
    let mut er = unsafe { EventRing::new(buf.as_ptr(), 3) };
    test_assert!(er.ccs(), "CCS starts set");

    // Controller writes two events this lap (cycle 1); slot 2 not yet written.
    buf[0].param = 0xAA;
    buf[0].set_cycle(true);
    buf[1].param = 0xBB;
    buf[1].set_cycle(true);

    let e0 = er.poll().ok_or("expected event 0")?;
    test_assert!(e0.param == 0xAA, "first event content");
    let e1 = er.poll().ok_or("expected event 1")?;
    test_assert!(e1.param == 0xBB, "second event content");
    test_assert!(er.dequeue_index() == 2, "dequeue advanced to the unwritten slot");
    test_assert!(er.poll().is_none(), "unwritten slot (cycle != CCS) yields None");
    test_assert!(er.ccs(), "CCS unchanged before the wrap");
    test_assert!(er.dequeue_ptr() == unsafe { buf.as_ptr().add(2) }, "dequeue ptr tracks the slot");

    // Controller writes the last slot this lap -> consuming it wraps and toggles.
    buf[2].param = 0xCC;
    buf[2].set_cycle(true);
    let e2 = er.poll().ok_or("expected event 2")?;
    test_assert!(e2.param == 0xCC, "third event content");
    test_assert!(er.dequeue_index() == 0, "dequeue wrapped");
    test_assert!(!er.ccs(), "CCS toggled on wrap");

    // Slot 0 still holds lap-1 cycle (1) != CCS (0) -> nothing new yet.
    test_assert!(er.poll().is_none(), "no next-lap event until the controller rewrites slot 0");
    // Next lap the controller writes cycle 0.
    buf[0].param = 0xDD;
    buf[0].set_cycle(false);
    let e3 = er.poll().ok_or("expected next-lap event")?;
    test_assert!(e3.param == 0xDD, "next-lap event content");
    test_assert!(er.dequeue_index() == 1, "dequeue advanced on the next lap");
    Ok(())
}

/// The ERST entry packs the 64-byte-aligned segment base and the size in TRBs.
pub fn erst_entry_encoding(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let e = ErstEntry { base_phys: 0x0001_2340u64, size: 64 };
    let w = e.encode();
    test_assert!(w[0] == 0x0001_2340, "low base (64-byte aligned bits kept)");
    test_assert!(w[1] == 0, "high base");
    test_assert!(w[2] == 64, "segment size in TRBs");
    test_assert!(w[3] == 0, "dword3 reserved");
    // Low 6 bits of the base are reserved and must be masked off.
    let e2 = ErstEntry { base_phys: 0x0001_237Fu64, size: 16 };
    test_assert!(e2.encode()[0] == 0x0001_2340, "reserved low 6 bits masked");
    Ok(())
}

/// Ep types, speed IDs, and the Device Context Index arithmetic.
pub fn constants_and_dci(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    test_assert!(EpType::IsochOut as u8 == 1, "isoch out");
    test_assert!(EpType::BulkOut as u8 == 2, "bulk out");
    test_assert!(EpType::InterruptOut as u8 == 3, "interrupt out");
    test_assert!(EpType::Control as u8 == 4, "control");
    test_assert!(EpType::IsochIn as u8 == 5, "isoch in");
    test_assert!(EpType::BulkIn as u8 == 6, "bulk in");
    test_assert!(EpType::InterruptIn as u8 == 7, "interrupt in");

    test_assert!(SPEED_FULL == 1 && SPEED_LOW == 2 && SPEED_HIGH == 3 && SPEED_SUPER == 4, "speed ids");

    // EP0 is always DCI 1; a directional endpoint N is N*2 (+1 for IN).
    test_assert!(dci(0, true) == 1, "EP0 is DCI 1");
    test_assert!(dci(0, false) == 1, "EP0 is DCI 1 regardless of direction");
    test_assert!(dci(1, false) == 2, "EP1 OUT is DCI 2");
    test_assert!(dci(1, true) == 3, "EP1 IN is DCI 3");
    test_assert!(dci(2, true) == 5, "EP2 IN is DCI 5");
    Ok(())
}

/// Slot Context field packing round-trips, and the controller-written device
/// address reads back from dword3.
pub fn slot_context_fields(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut s = SlotContext::zeroed();
    s.set_route_string(0);
    s.set_speed(SPEED_HIGH);
    s.set_context_entries(1); // slot + EP0
    s.set_root_hub_port(3);
    test_assert!(s.route_string() == 0, "route string");
    test_assert!(s.speed() == SPEED_HIGH, "speed");
    test_assert!(s.context_entries() == 1, "context entries");
    test_assert!(s.root_hub_port() == 3, "root hub port");

    // Speed and context-entries share dword0; setting one must not disturb others.
    s.set_route_string(0x12345);
    test_assert!(s.route_string() == 0x12345, "route string after neighbour writes");
    test_assert!(s.speed() == SPEED_HIGH, "speed survives a route-string write");
    test_assert!(s.context_entries() == 1, "context entries survive too");

    // Device address is controller-written into dword3 bits 0-7.
    s.dwords[3] = (s.dwords[3] & !0xFF) | 0x2A;
    test_assert!(s.device_address() == 0x2A, "device address reads back");
    Ok(())
}

/// Endpoint Context field packing, including the 64-bit TR dequeue pointer and its
/// dequeue cycle state bit sharing the low nibble.
pub fn endpoint_context_fields(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut e = EndpointContext::zeroed();
    e.set_ep_type(EpType::InterruptIn);
    e.set_max_packet_size(8);
    e.set_max_burst(0);
    e.set_error_count(3);
    e.set_interval(7);
    e.set_avg_trb_len(8);
    test_assert!(e.ep_type() == EpType::InterruptIn as u8, "ep type");
    test_assert!(e.max_packet_size() == 8, "max packet size");
    test_assert!(e.max_burst() == 0, "max burst");
    test_assert!(e.error_count() == 3, "error count");
    test_assert!(e.interval() == 7, "interval");
    test_assert!(e.avg_trb_len() == 8, "avg trb len");

    // TR dequeue pointer: 16-byte aligned, low bit is DCS not address.
    let ptr = 0x0004_5670u64; // 16-aligned
    e.set_tr_dequeue(ptr, true);
    test_assert!(e.tr_dequeue() == ptr, "TR dequeue pointer round-trips");
    test_assert!(e.dcs(), "dequeue cycle state set");
    e.set_tr_dequeue(ptr, false);
    test_assert!(!e.dcs(), "dequeue cycle state cleared");
    test_assert!(e.tr_dequeue() == ptr, "pointer unchanged by DCS");

    // A high-half pointer exercises dword3.
    let hi = 0x1_0000_0000u64;
    e.set_tr_dequeue(hi, false);
    test_assert!(e.tr_dequeue() == hi, "64-bit TR dequeue pointer");
    Ok(())
}

/// Input Control Context add/drop flags map to Device Context Indices.
pub fn input_control_context_flags(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    // Address Device adds the slot (DCI 0) and EP0 (DCI 1).
    let mut ic = InputControlContext::zeroed();
    ic.add_context(dci(0, true)); // EP0 -> DCI 1
    ic.add_context(0); // slot context
    test_assert!(ic.add_flags() == 0b11, "add flags for slot + EP0");
    test_assert!(ic.drop_flags() == 0, "nothing dropped");

    // Configure Endpoint adds the interrupt-IN endpoint (say EP1 IN -> DCI 3).
    let mut ic2 = InputControlContext::zeroed();
    let d = dci(1, true);
    ic2.add_context(0); // slot context is re-evaluated
    ic2.add_context(d);
    test_assert!(ic2.add_flags() == (1 | (1 << d)), "add slot + interrupt-IN endpoint");

    // Dropping an endpoint sets its bit in the drop flags.
    ic2.drop_context(dci(2, false)); // DCI 4
    test_assert!(ic2.drop_flags() == (1 << 4), "drop flag for DCI 4");
    Ok(())
}
