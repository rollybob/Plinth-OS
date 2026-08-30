//! xHCI (USB 3) host controller -- pure encoding layer.
//!
//! First slice of the USB HID milestone (`Design/usb_hid.md` section 4 step 1):
//! the deterministic, no-device data structures the driver builds and reads --
//! Transfer Request Blocks (TRBs), the producer command/transfer ring (Link-TRB
//! wrap + cycle-bit toggle), the event-ring consumer (cycle-follow), the Event
//! Ring Segment Table entry, and the 32-byte Slot/Endpoint/Input contexts. There
//! is no MMIO, no controller bring-up, and no allocation here -- every routine
//! operates on caller-supplied memory, so the in-kernel test suite drives it with
//! plain arrays, the same "pure structure over injected backing" discipline the
//! IOMMU `Domain` and IPC `WaitQueue` follow.
//!
//! The controller bring-up slice (map the BAR, reset, program the DCBAA +
//! command/event rings + interrupter, doorbells, MSI-X) builds on top of this;
//! that slice is what wires the module into boot and removes the `dead_code`
//! allowance on the `mod xhci` declaration.
//!
//! v1 scope (ruled in usb_hid.md): xHCI only, boot protocol only, 32-byte
//! contexts (HCCPARAMS1 CSZ = 0). Field coverage is what an Address Device (EP0)
//! and a boot interrupt-IN keyboard endpoint need, not the whole 1.2 spec.

use crate::{memory, pci};
use core::fmt::Write;

/// Extract a `width`-bit field at `shift` from a dword. `width` must be < 32.
#[inline]
fn field(word: u32, shift: u32, width: u32) -> u32 {
    (word >> shift) & ((1u32 << width) - 1)
}

/// Set a `width`-bit field at `shift` in `word` to `val`. `width` must be < 32
/// (whole-dword fields, e.g. the input-control add flags, are written directly).
#[inline]
fn set_field(word: &mut u32, shift: u32, width: u32, val: u32) {
    let mask = ((1u32 << width) - 1) << shift;
    *word = (*word & !mask) | ((val << shift) & mask);
}

// -------------------------------------------------------------------------
// TRB
// -------------------------------------------------------------------------

/// TRB type codes (xHCI 1.2, table 6-91) -- the subset this slice builds or reads.
/// Commands and events share the field; the values do not collide. The transfer
/// TRBs (Normal, Setup/Data/Status Stage) arrive with the control-transfer slice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TrbType {
    Link = 6,
    EnableSlot = 9,
    AddressDevice = 11,
    ConfigureEndpoint = 12,
    TransferEvent = 32,
    CommandCompletion = 33,
    PortStatusChange = 34,
}

const TRB_TYPE_SHIFT: u32 = 10;
const TRB_TYPE_WIDTH: u32 = 6;
const TRB_CYCLE: u32 = 1 << 0;
/// Toggle Cycle (TC), Link TRB control bit 1: tells the controller to invert its
/// consumer cycle state when it follows this link, which is what lets a ring wrap.
const TRB_LINK_TOGGLE: u32 = 1 << 1;
/// Block Set Address Request (BSR), Address Device control bit 9.
const TRB_BSR: u32 = 1 << 9;

/// A Transfer Request Block: 16 bytes, four little-endian dwords. `param` is the
/// combined dword0/dword1 (a pointer or an immediate setup payload), `status` is
/// dword2, and `control` is dword3 (cycle bit 0, TRB type bits 10-15, plus
/// per-type fields). On x86-64 (little-endian) this layout is byte-identical to
/// what the controller reads over DMA.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
#[repr(C, align(16))]
pub struct Trb {
    pub param: u64,
    pub status: u32,
    pub control: u32,
}

impl Trb {
    pub const fn zeroed() -> Self {
        Trb { param: 0, status: 0, control: 0 }
    }

    pub fn trb_type(&self) -> u8 {
        field(self.control, TRB_TYPE_SHIFT, TRB_TYPE_WIDTH) as u8
    }

    fn set_trb_type(&mut self, t: TrbType) {
        set_field(&mut self.control, TRB_TYPE_SHIFT, TRB_TYPE_WIDTH, t as u32);
    }

    pub fn cycle(&self) -> bool {
        self.control & TRB_CYCLE != 0
    }

    pub fn set_cycle(&mut self, c: bool) {
        if c {
            self.control |= TRB_CYCLE;
        } else {
            self.control &= !TRB_CYCLE;
        }
    }

    // -- Command TRB constructors (cycle left clear; the ring stamps it) --

    /// Link TRB: at a ring segment's end it points back to `ring_base_phys` and,
    /// with Toggle Cycle set, flips the controller's consumer cycle on the wrap.
    pub fn link(ring_base_phys: u64, toggle: bool) -> Self {
        let mut t = Trb { param: ring_base_phys, status: 0, control: 0 };
        t.set_trb_type(TrbType::Link);
        if toggle {
            t.control |= TRB_LINK_TOGGLE;
        }
        t
    }

    /// Enable Slot command. Slot Type (control bits 16-20) is 0 for USB.
    pub fn enable_slot() -> Self {
        let mut t = Trb::zeroed();
        t.set_trb_type(TrbType::EnableSlot);
        t
    }

    /// Address Device command: `input_ctx_phys` is the Input Context pointer,
    /// `slot_id` goes in control bits 24-31, and BSR (block set address) is set
    /// when only the slot/EP0 contexts should be initialised without issuing the
    /// USB SET_ADDRESS request yet.
    pub fn address_device(input_ctx_phys: u64, slot_id: u8, bsr: bool) -> Self {
        let mut t = Trb { param: input_ctx_phys, status: 0, control: 0 };
        t.set_trb_type(TrbType::AddressDevice);
        set_field(&mut t.control, 24, 8, slot_id as u32);
        if bsr {
            t.control |= TRB_BSR;
        }
        t
    }

    /// Configure Endpoint command: applies an Input Context that adds/drops
    /// endpoints (used to bring up the boot interrupt-IN endpoint).
    pub fn configure_endpoint(input_ctx_phys: u64, slot_id: u8) -> Self {
        let mut t = Trb { param: input_ctx_phys, status: 0, control: 0 };
        t.set_trb_type(TrbType::ConfigureEndpoint);
        set_field(&mut t.control, 24, 8, slot_id as u32);
        t
    }

    // -- Event TRB accessors (read side; the controller writes these) --

    /// Command Completion / Transfer Event completion code (status bits 24-31).
    /// Code 1 is Success (xHCI table 6-90).
    pub fn completion_code(&self) -> u8 {
        field(self.status, 24, 8) as u8
    }

    /// Command Completion / Transfer Event slot id (control bits 24-31).
    pub fn event_slot_id(&self) -> u8 {
        field(self.control, 24, 8) as u8
    }

    /// Transfer Event endpoint id / DCI (control bits 16-20).
    pub fn event_endpoint_id(&self) -> u8 {
        field(self.control, 16, 5) as u8
    }

    /// Transfer Event residual/transferred length (status bits 0-23).
    pub fn transfer_length(&self) -> u32 {
        field(self.status, 0, 24)
    }

    /// Command Completion command-TRB pointer / Transfer Event TRB pointer.
    pub fn trb_pointer(&self) -> u64 {
        self.param
    }

    /// Port Status Change Event port id (dword0 bits 24-31).
    pub fn port_id(&self) -> u8 {
        ((self.param >> 24) & 0xFF) as u8
    }
}

// -------------------------------------------------------------------------
// Producer ring (command ring / transfer ring)
// -------------------------------------------------------------------------

/// A software-produced ring: a segment of TRB slots whose last slot is a Link TRB
/// back to the start. The producer holds an enqueue index and a producer cycle
/// state (PCS); each pushed TRB is stamped with the PCS, and reaching the Link
/// slot stamps the link with the PCS (so the controller follows it) and toggles
/// the PCS for the next lap. This is the pure logic the bring-up slice drives over
/// a DMA frame; here it drives a caller-supplied `&mut [Trb]`.
pub struct ProducerRing<'a> {
    trbs: &'a mut [Trb],
    base_phys: u64,
    enqueue: usize,
    pcs: bool,
}

impl<'a> ProducerRing<'a> {
    /// `trbs` must have at least two slots (one usable + the Link). `base_phys`
    /// is the physical address the Link TRB wraps to. The PCS starts set (1), the
    /// Link TRB starts clear so the controller will not follow it until the first
    /// wrap stamps it.
    pub fn new(trbs: &'a mut [Trb], base_phys: u64) -> Self {
        assert!(trbs.len() >= 2, "a producer ring needs a usable slot plus the link");
        for t in trbs.iter_mut() {
            *t = Trb::zeroed();
        }
        let last = trbs.len() - 1;
        trbs[last] = Trb::link(base_phys, true);
        ProducerRing { trbs, base_phys, enqueue: 0, pcs: true }
    }

    pub fn pcs(&self) -> bool {
        self.pcs
    }

    pub fn enqueue_index(&self) -> usize {
        self.enqueue
    }

    pub fn peek(&self, i: usize) -> Trb {
        self.trbs[i]
    }

    /// Write `trb` at the enqueue pointer (stamping the current PCS), advance, and
    /// if the next slot is the Link, arm the link with the PCS and wrap with a PCS
    /// toggle. Returns the physical address of the slot the TRB was written to (a
    /// Command Completion Event later points back to it).
    pub fn push(&mut self, mut trb: Trb) -> u64 {
        let idx = self.enqueue;
        trb.set_cycle(self.pcs);
        self.trbs[idx] = trb;
        let addr = self.base_phys + (idx as u64) * core::mem::size_of::<Trb>() as u64;

        self.enqueue += 1;
        // The last slot is the Link TRB, never a data slot: reaching it wraps.
        if self.enqueue == self.trbs.len() - 1 {
            let last = self.trbs.len() - 1;
            self.trbs[last].set_cycle(self.pcs);
            self.enqueue = 0;
            self.pcs = !self.pcs;
        }
        addr
    }
}

// -------------------------------------------------------------------------
// Event ring consumer
// -------------------------------------------------------------------------

/// The consumer side of a single-segment event ring. The controller writes event
/// TRBs and flips its producer cycle each lap; software follows by comparing each
/// slot's cycle bit against its consumer cycle state (CCS). A slot whose cycle
/// does not match the CCS has not been written this lap -> no event yet.
///
/// Backed by a raw pointer and read with `read_volatile`, exactly as the DMA
/// memory demands (the controller mutates it underneath us); tests point it at a
/// plain array and mutate that array to stand in for the controller.
pub struct EventRing {
    base: *const Trb,
    len: usize,
    dequeue: usize,
    ccs: bool,
}

impl EventRing {
    /// # Safety
    /// `base` must point to `len` contiguous, readable `Trb` slots that stay valid
    /// for the ring's lifetime (a DMA frame in real use, a fixed array in tests).
    pub unsafe fn new(base: *const Trb, len: usize) -> Self {
        assert!(len >= 1, "an event ring needs at least one slot");
        EventRing { base, len, dequeue: 0, ccs: true }
    }

    pub fn ccs(&self) -> bool {
        self.ccs
    }

    pub fn dequeue_index(&self) -> usize {
        self.dequeue
    }

    /// Physical/virtual address of the current dequeue slot -- what software would
    /// write back to the ERDP register after consuming events.
    pub fn dequeue_ptr(&self) -> *const Trb {
        // Safety: dequeue is always < len (advance wraps it), so this is in range.
        unsafe { self.base.add(self.dequeue) }
    }

    /// Return the next event if the controller has written one (its cycle matches
    /// the CCS), advancing the dequeue pointer and toggling the CCS on wrap.
    pub fn poll(&mut self) -> Option<Trb> {
        // Safety: dequeue < len by construction and after every advance.
        let trb = unsafe { core::ptr::read_volatile(self.base.add(self.dequeue)) };
        if trb.cycle() != self.ccs {
            return None;
        }
        self.dequeue += 1;
        if self.dequeue == self.len {
            self.dequeue = 0;
            self.ccs = !self.ccs;
        }
        Some(trb)
    }
}

// -------------------------------------------------------------------------
// Event Ring Segment Table
// -------------------------------------------------------------------------

/// One Event Ring Segment Table entry (16 bytes): the segment's base address
/// (64-bit, low 6 bits reserved -> 64-byte aligned) and its size in TRBs (dword2
/// bits 0-15). dword3 is reserved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ErstEntry {
    pub base_phys: u64,
    pub size: u16,
}

impl ErstEntry {
    pub fn encode(&self) -> [u32; 4] {
        [
            (self.base_phys & 0xFFFF_FFC0) as u32,
            (self.base_phys >> 32) as u32,
            self.size as u32,
            0,
        ]
    }
}

// -------------------------------------------------------------------------
// Device Context Index / endpoint types
// -------------------------------------------------------------------------

/// Endpoint Context EP Type field (xHCI table 6-9).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum EpType {
    IsochOut = 1,
    BulkOut = 2,
    InterruptOut = 3,
    Control = 4,
    IsochIn = 5,
    BulkIn = 6,
    InterruptIn = 7,
}

/// USB port speed IDs as they appear in PORTSC and the Slot Context Speed field.
pub const SPEED_FULL: u8 = 1;
pub const SPEED_LOW: u8 = 2;
pub const SPEED_HIGH: u8 = 3;
pub const SPEED_SUPER: u8 = 4;

/// Device Context Index for an endpoint. EP0 (the bidirectional control endpoint)
/// is DCI 1; a directional endpoint N is `N*2 + (1 if IN else 0)`. The DCI is also
/// the doorbell target for that endpoint.
pub fn dci(ep_num: u8, dir_in: bool) -> u8 {
    if ep_num == 0 {
        1
    } else {
        ep_num * 2 + dir_in as u8
    }
}

// -------------------------------------------------------------------------
// Contexts (32-byte, CSZ = 0)
// -------------------------------------------------------------------------

/// Slot Context (32 bytes / 8 dwords), xHCI 6.2.2. Only the fields an Address
/// Device on a root-hub-attached device needs are exposed.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
#[repr(C, align(32))]
pub struct SlotContext {
    pub dwords: [u32; 8],
}

impl SlotContext {
    pub const fn zeroed() -> Self {
        SlotContext { dwords: [0; 8] }
    }

    /// Route String (dword0 bits 0-19): 0 for a device on a root-hub port.
    pub fn set_route_string(&mut self, route: u32) {
        set_field(&mut self.dwords[0], 0, 20, route);
    }
    pub fn route_string(&self) -> u32 {
        field(self.dwords[0], 0, 20)
    }

    /// Speed (dword0 bits 20-23).
    pub fn set_speed(&mut self, speed: u8) {
        set_field(&mut self.dwords[0], 20, 4, speed as u32);
    }
    pub fn speed(&self) -> u8 {
        field(self.dwords[0], 20, 4) as u8
    }

    /// Context Entries (dword0 bits 27-31): the index of the last valid endpoint
    /// context. 1 means "slot + EP0 only".
    pub fn set_context_entries(&mut self, entries: u8) {
        set_field(&mut self.dwords[0], 27, 5, entries as u32);
    }
    pub fn context_entries(&self) -> u8 {
        field(self.dwords[0], 27, 5) as u8
    }

    /// Root Hub Port Number (dword1 bits 16-23), 1-based.
    pub fn set_root_hub_port(&mut self, port: u8) {
        set_field(&mut self.dwords[1], 16, 8, port as u32);
    }
    pub fn root_hub_port(&self) -> u8 {
        field(self.dwords[1], 16, 8) as u8
    }

    /// USB Device Address (dword3 bits 0-7), filled by the controller after
    /// Address Device -- exposed for read-back.
    pub fn device_address(&self) -> u8 {
        field(self.dwords[3], 0, 8) as u8
    }
}

/// Endpoint Context (32 bytes / 8 dwords), xHCI 6.2.3.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
#[repr(C, align(32))]
pub struct EndpointContext {
    pub dwords: [u32; 8],
}

impl EndpointContext {
    pub const fn zeroed() -> Self {
        EndpointContext { dwords: [0; 8] }
    }

    /// Interval (dword0 bits 16-23): the endpoint's polling interval exponent,
    /// taken from the endpoint descriptor's bInterval (encoded per xHCI 6.2.3.6).
    pub fn set_interval(&mut self, interval: u8) {
        set_field(&mut self.dwords[0], 16, 8, interval as u32);
    }
    pub fn interval(&self) -> u8 {
        field(self.dwords[0], 16, 8) as u8
    }

    /// Error Count / CErr (dword1 bits 1-2): 3 is the usual "retry three times".
    pub fn set_error_count(&mut self, cerr: u8) {
        set_field(&mut self.dwords[1], 1, 2, cerr as u32);
    }
    pub fn error_count(&self) -> u8 {
        field(self.dwords[1], 1, 2) as u8
    }

    /// EP Type (dword1 bits 3-5).
    pub fn set_ep_type(&mut self, ep: EpType) {
        set_field(&mut self.dwords[1], 3, 3, ep as u32);
    }
    pub fn ep_type(&self) -> u8 {
        field(self.dwords[1], 3, 3) as u8
    }

    /// Max Burst Size (dword1 bits 8-15): 0 for full/low/high speed.
    pub fn set_max_burst(&mut self, burst: u8) {
        set_field(&mut self.dwords[1], 8, 8, burst as u32);
    }
    pub fn max_burst(&self) -> u8 {
        field(self.dwords[1], 8, 8) as u8
    }

    /// Max Packet Size (dword1 bits 16-31).
    pub fn set_max_packet_size(&mut self, mps: u16) {
        set_field(&mut self.dwords[1], 16, 16, mps as u32);
    }
    pub fn max_packet_size(&self) -> u16 {
        field(self.dwords[1], 16, 16) as u16
    }

    /// TR Dequeue Pointer (dwords 2-3) plus the Dequeue Cycle State (dword2 bit 0).
    /// The pointer must be 16-byte aligned; its low bits carry DCS, not address.
    pub fn set_tr_dequeue(&mut self, ptr_phys: u64, dcs: bool) {
        debug_assert!(ptr_phys & 0xF == 0, "TR dequeue pointer must be 16-byte aligned");
        self.dwords[2] = (ptr_phys as u32 & 0xFFFF_FFF0) | (dcs as u32);
        self.dwords[3] = (ptr_phys >> 32) as u32;
    }
    pub fn tr_dequeue(&self) -> u64 {
        ((self.dwords[3] as u64) << 32) | (self.dwords[2] as u64 & 0xFFFF_FFF0)
    }
    pub fn dcs(&self) -> bool {
        self.dwords[2] & 1 != 0
    }

    /// Average TRB Length (dword4 bits 0-15): a controller scheduling hint; 8 for a
    /// boot keyboard's tiny interrupt reports.
    pub fn set_avg_trb_len(&mut self, len: u16) {
        set_field(&mut self.dwords[4], 0, 16, len as u32);
    }
    pub fn avg_trb_len(&self) -> u16 {
        field(self.dwords[4], 0, 16) as u16
    }
}

/// Input Control Context (32 bytes / 8 dwords), xHCI 6.2.5.1: which device-context
/// entries a command adds or drops. Flag bit i corresponds to Device Context Index
/// i (0 = slot, 1 = EP0, ...). Drop flags' bits 0-1 are reserved and must be 0.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
#[repr(C, align(32))]
pub struct InputControlContext {
    pub dwords: [u32; 8],
}

impl InputControlContext {
    pub const fn zeroed() -> Self {
        InputControlContext { dwords: [0; 8] }
    }

    /// Mark the context at `dci` to be added (Add Context flags, dword1).
    pub fn add_context(&mut self, dci: u8) {
        self.dwords[1] |= 1u32 << dci;
    }
    pub fn add_flags(&self) -> u32 {
        self.dwords[1]
    }

    /// Mark the context at `dci` to be dropped (Drop Context flags, dword0).
    /// DCI 0/1 cannot be dropped (reserved), so this guards against it.
    pub fn drop_context(&mut self, dci: u8) {
        debug_assert!(dci >= 2, "slot (0) and EP0 (1) cannot be dropped");
        self.dwords[0] |= 1u32 << dci;
    }
    pub fn drop_flags(&self) -> u32 {
        self.dwords[0]
    }
}

// -------------------------------------------------------------------------
// Controller bring-up (step 1b): discover, map, read capabilities, reset.
//
// This is the MMIO half of step 1 (usb_hid.md section 4). It finds the xHCI
// controller by PCI class, sizes and maps its register BAR, reads and reports the
// capability registers, and resets the controller. Programming the DCBAA +
// command/event rings and setting Run/Stop (so a port-connect posts a Port Status
// Change event on the event ring, decoded with the EventRing/Trb accessors above)
// is the next increment.
// -------------------------------------------------------------------------

// MMIO accessors. All controller registers are read/written volatile.
#[inline]
fn r8(a: u64) -> u8 {
    // Safety: `a` is inside the mapped xHCI register BAR (map_kernel_mmio).
    unsafe { core::ptr::read_volatile(a as *const u8) }
}
#[inline]
fn r16(a: u64) -> u16 {
    unsafe { core::ptr::read_volatile(a as *const u16) }
}
#[inline]
fn r32(a: u64) -> u32 {
    unsafe { core::ptr::read_volatile(a as *const u32) }
}
#[inline]
fn w32(a: u64, v: u32) {
    unsafe { core::ptr::write_volatile(a as *mut u32, v) }
}

// Capability registers, offsets from the MMIO base (xHCI 5.3).
const CAP_CAPLENGTH: u64 = 0x00;
const CAP_HCIVERSION: u64 = 0x02;
const CAP_HCSPARAMS1: u64 = 0x04;
const CAP_HCCPARAMS1: u64 = 0x10;
const CAP_DBOFF: u64 = 0x14;
const CAP_RTSOFF: u64 = 0x18;

// Operational registers, offsets from the operational base (MMIO + CAPLENGTH).
const OP_USBCMD: u64 = 0x00;
const OP_USBSTS: u64 = 0x04;

const USBCMD_RS: u32 = 1 << 0; // Run/Stop
const USBCMD_HCRST: u32 = 1 << 1; // Host Controller Reset
const USBSTS_HCH: u32 = 1 << 0; // HCHalted
const USBSTS_CNR: u32 = 1 << 11; // Controller Not Ready

/// A brought-up xHCI controller: the mapped register bases and the parsed
/// capability parameters. The ring/DCBAA fields arrive with the next increment.
pub struct Xhci {
    mmio: u64,
    op: u64,
    rt: u64,
    db: u64,
    max_slots: u8,
    max_ports: u8,
    csz: bool,
}

impl Xhci {
    pub fn max_slots(&self) -> u8 {
        self.max_slots
    }
    pub fn max_ports(&self) -> u8 {
        self.max_ports
    }
    pub fn context_size(&self) -> usize {
        if self.csz {
            64
        } else {
            32
        }
    }
    /// Runtime-register base (interrupters live here); used when the event ring is
    /// programmed in the next increment.
    pub fn runtime_base(&self) -> u64 {
        self.rt
    }
    /// Doorbell-array base; used to ring the command/endpoint doorbells later.
    pub fn doorbell_base(&self) -> u64 {
        self.db
    }

    /// Halt (if running) then reset the controller: assert HCRST, wait for it to
    /// self-clear, then wait for CNR (Controller Not Ready) to clear. Returns
    /// false on timeout rather than hanging the boot.
    fn reset<W: Write>(&mut self, out: &mut W) -> bool {
        let cmd = r32(self.op + OP_USBCMD);
        if cmd & USBCMD_RS != 0 {
            w32(self.op + OP_USBCMD, cmd & !USBCMD_RS);
            if !wait_until(|| r32(self.op + OP_USBSTS) & USBSTS_HCH != 0) {
                let _ = writeln!(out, "plinth: xhci: halt timed out");
                return false;
            }
        }
        w32(self.op + OP_USBCMD, USBCMD_HCRST);
        if !wait_until(|| r32(self.op + OP_USBCMD) & USBCMD_HCRST == 0) {
            let _ = writeln!(out, "plinth: xhci: reset (HCRST) did not clear");
            return false;
        }
        if !wait_until(|| r32(self.op + OP_USBSTS) & USBSTS_CNR == 0) {
            let _ = writeln!(out, "plinth: xhci: controller not ready (CNR set)");
            return false;
        }
        let _ = writeln!(out, "plinth: xhci: reset ok (halted, cnr clear)");
        true
    }
}

/// Poll `cond` up to a fixed bound with a relaxed spin. There is no fine-grained
/// timer on this path, so the wait is bounded by iteration count -- generous for
/// TCG QEMU, and a real controller readies in microseconds. Returns false if the
/// condition never held (a real controller/firmware fault), so the caller reports
/// and moves on instead of hanging (first_metal_boot.md D3).
fn wait_until<F: Fn() -> bool>(cond: F) -> bool {
    for _ in 0..50_000_000u64 {
        if cond() {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Discover, map, and reset the xHCI controller. Returns None (quietly) when no
/// xHCI is present, so a build with no USB controller boots unchanged; returns
/// None (with a reported reason) when a present controller fails to map or reset.
pub fn init<W: Write>(out: &mut W) -> Option<Xhci> {
    // xHCI = PCI class 0x0C (serial bus), subclass 0x03 (USB), prog-if 0x30.
    let loc = pci::find_class(0x0C, 0x03, 0x30)?;

    // BAR0 is the register file (a 64-bit memory BAR on QEMU). Size it rather than
    // assume a QEMU-specific extent (first_metal_boot.md D6: no hardcoded layout).
    let bar = pci::read_bar(loc, 0);
    let size = pci::bar_size(loc, 0);
    let mmio = match memory::map_kernel_mmio(bar, size) {
        Ok(va) => va,
        Err(e) => {
            let _ = writeln!(out, "plinth: xhci: BAR map failed: {e}");
            return None;
        }
    };

    // Memory-space decode + bus mastering: the controller DMAs its own rings.
    pci::enable_bus_master(loc);

    let cap_len = r8(mmio + CAP_CAPLENGTH) as u64;
    let hciversion = r16(mmio + CAP_HCIVERSION);
    let hcs1 = r32(mmio + CAP_HCSPARAMS1);
    let hcc1 = r32(mmio + CAP_HCCPARAMS1);
    // DBOFF is dword-aligned (low 2 bits reserved); RTSOFF 32-byte-aligned (low 5).
    let dboff = (r32(mmio + CAP_DBOFF) & !0x3) as u64;
    let rtsoff = (r32(mmio + CAP_RTSOFF) & !0x1F) as u64;

    let max_slots = (hcs1 & 0xFF) as u8;
    let max_ports = ((hcs1 >> 24) & 0xFF) as u8;
    let csz = hcc1 & (1 << 2) != 0; // Context Size: 1 => 64-byte contexts

    let mut x = Xhci {
        mmio,
        op: mmio + cap_len,
        rt: mmio + rtsoff,
        db: mmio + dboff,
        max_slots,
        max_ports,
        csz,
    };

    let _ = writeln!(
        out,
        "plinth: xhci: controller at {:02x}:{:02x} bar 0x{:x} size 0x{:x}",
        loc.bus, loc.slot, bar, size
    );
    let _ = writeln!(
        out,
        "plinth: xhci: caplen {} hciversion 0x{:04x} slots {} ports {} csz {}",
        cap_len, hciversion, max_slots, max_ports, csz as u8
    );

    if !x.reset(out) {
        return None;
    }
    Some(x)
}
