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

/// TRB type codes (xHCI 1.2, table 6-91) -- the subset this driver builds or reads.
/// Commands and events share the field; the values do not collide.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TrbType {
    Normal = 1,
    SetupStage = 2,
    DataStage = 3,
    StatusStage = 4,
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
/// Interrupt On Completion (IOC), transfer-TRB control bit 5.
const TRB_IOC: u32 = 1 << 5;
/// Immediate Data (IDT), Setup Stage control bit 6.
const TRB_IDT: u32 = 1 << 6;
/// Direction (DIR), Data/Status Stage control bit 16 (1 = IN).
const TRB_DIR: u32 = 1 << 16;

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

    /// Setup Stage TRB of a control transfer: the 8-byte setup packet rides inline
    /// (IDT). `trt` is the Transfer Type (0 = no data, 2 = OUT data, 3 = IN data).
    pub fn setup_stage(setup_packet: u64, trt: u8) -> Self {
        let mut t = Trb { param: setup_packet, status: 8, control: 0 };
        t.set_trb_type(TrbType::SetupStage);
        t.control |= TRB_IDT;
        set_field(&mut t.control, 16, 2, trt as u32);
        t
    }

    /// Data Stage TRB: `buf_phys` is the data buffer, `len` its byte count,
    /// `dir_in` true for a device-to-host (IN) transfer.
    pub fn data_stage(buf_phys: u64, len: u32, dir_in: bool) -> Self {
        let mut t = Trb { param: buf_phys, status: len & 0x1_FFFF, control: 0 };
        t.set_trb_type(TrbType::DataStage);
        if dir_in {
            t.control |= TRB_DIR;
        }
        t
    }

    /// Status Stage TRB: the zero-length handshake that ends a control transfer.
    /// `dir_in` is the status direction (opposite the data direction); `ioc`
    /// requests a Transfer Event on completion.
    pub fn status_stage(dir_in: bool, ioc: bool) -> Self {
        let mut t = Trb::zeroed();
        t.set_trb_type(TrbType::StatusStage);
        if dir_in {
            t.control |= TRB_DIR;
        }
        if ioc {
            t.control |= TRB_IOC;
        }
        t
    }

    /// Normal TRB: a single data buffer on a transfer ring (the boot interrupt-IN
    /// endpoint's report reads).
    pub fn normal(buf_phys: u64, len: u32, ioc: bool) -> Self {
        let mut t = Trb { param: buf_phys, status: len & 0x1_FFFF, control: 0 };
        t.set_trb_type(TrbType::Normal);
        if ioc {
            t.control |= TRB_IOC;
        }
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
/// the PCS for the next lap.
///
/// Backed by a raw pointer and driven with volatile accesses -- the ring lives in
/// DMA memory the controller reads, so the writes must not be reordered or elided,
/// and the type must be storable in a long-lived driver (or handed to a library OS
/// that drives the device directly, the exokernel end-state) rather than borrowing
/// its backing. `EventRing` is raw-pointer for the mirror-image reason (the device
/// writes it); the two rings are deliberately symmetric. Tests point it at a plain
/// array.
pub struct ProducerRing {
    base: *mut Trb,
    cap: usize,
    base_phys: u64,
    enqueue: usize,
    pcs: bool,
}

impl ProducerRing {
    /// `base`/`cap` describe at least two contiguous TRB slots (one usable + the
    /// Link); `base_phys` is the physical address the Link TRB wraps to. The PCS
    /// starts set (1); the Link TRB starts clear so the controller will not follow
    /// it until the first wrap stamps it. All slots are zeroed here.
    ///
    /// # Safety
    /// `base` must point to `cap` contiguous, writable `Trb` slots that stay valid
    /// for the ring's lifetime (a DMA frame in the driver, a fixed array in tests).
    pub unsafe fn new(base: *mut Trb, cap: usize, base_phys: u64) -> Self {
        assert!(cap >= 2, "a producer ring needs a usable slot plus the link");
        for i in 0..cap {
            core::ptr::write_volatile(base.add(i), Trb::zeroed());
        }
        core::ptr::write_volatile(base.add(cap - 1), Trb::link(base_phys, true));
        ProducerRing { base, cap, base_phys, enqueue: 0, pcs: true }
    }

    pub fn pcs(&self) -> bool {
        self.pcs
    }

    pub fn enqueue_index(&self) -> usize {
        self.enqueue
    }

    pub fn peek(&self, i: usize) -> Trb {
        // Safety: i < cap by the caller's contract; base spans cap slots.
        unsafe { core::ptr::read_volatile(self.base.add(i)) }
    }

    /// Write `trb` at the enqueue pointer (stamping the current PCS), advance, and
    /// if the next slot is the Link, arm the link with the PCS and wrap with a PCS
    /// toggle. Returns the physical address of the slot the TRB was written to (a
    /// Command Completion Event later points back to it).
    pub fn push(&mut self, mut trb: Trb) -> u64 {
        let idx = self.enqueue;
        trb.set_cycle(self.pcs);
        // Safety: idx < cap - 1 always (the wrap below keeps it in range).
        unsafe { core::ptr::write_volatile(self.base.add(idx), trb) };
        let addr = self.base_phys + (idx as u64) * core::mem::size_of::<Trb>() as u64;

        self.enqueue += 1;
        // The last slot is the Link TRB, never a data slot: reaching it wraps.
        if self.enqueue == self.cap - 1 {
            // Arm the link with the current PCS so the controller follows it.
            unsafe {
                let mut link = core::ptr::read_volatile(self.base.add(self.cap - 1));
                link.set_cycle(self.pcs);
                core::ptr::write_volatile(self.base.add(self.cap - 1), link);
            }
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
/// Write a 64-bit controller register as two 32-bit stores (low then high). The
/// 64-bit xHCI registers are defined for 32-bit access, and a split write cannot
/// be dropped the way a single 64-bit MMIO store can be on some paths -- the same
/// guard virtio-blk uses for its ring-address registers.
#[inline]
fn w64(a: u64, v: u64) {
    w32(a, v as u32);
    w32(a + 4, (v >> 32) as u32);
}

/// Allocate one frame, zero it, and return (physical, virtual) addresses. DMA
/// structures (DCBAA, rings, ERST, scratchpad) live in these; the controller sees
/// the physical address, the kernel accesses the frame at `phys + PHYS_OFFSET`.
/// v1 runs xHCI untranslated (usb_hid.md D9), so the physical address is what the
/// controller uses directly.
fn alloc_zeroed() -> Result<(u64, u64), &'static str> {
    let phys = {
        let mut g = crate::frame_alloc::FRAME_ALLOC.lock();
        let fa = g.as_mut().ok_or("frame allocator not initialised")?;
        fa.alloc().map_err(|_| "out of frames for xhci")?
    };
    let va = memory::phys_offset() + phys;
    // Safety: the frame is freshly allocated and mapped at phys_offset; nothing
    // else aliases it.
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, crate::frame_alloc::FRAME_SIZE as usize) };
    Ok((phys, va))
}

// Capability registers, offsets from the MMIO base (xHCI 5.3).
const CAP_CAPLENGTH: u64 = 0x00;
const CAP_HCIVERSION: u64 = 0x02;
const CAP_HCSPARAMS1: u64 = 0x04;
const CAP_HCCPARAMS1: u64 = 0x10;
const CAP_DBOFF: u64 = 0x14;
const CAP_RTSOFF: u64 = 0x18;

const CAP_HCSPARAMS2: u64 = 0x08;

// Operational registers, offsets from the operational base (MMIO + CAPLENGTH).
const OP_USBCMD: u64 = 0x00;
const OP_USBSTS: u64 = 0x04;
const OP_CRCR: u64 = 0x18; // Command Ring Control (64-bit)
const OP_DCBAAP: u64 = 0x30; // Device Context Base Address Array Pointer (64-bit)
const OP_CONFIG: u64 = 0x38; // Configure (MaxSlotsEn in bits 0-7)

const USBCMD_RS: u32 = 1 << 0; // Run/Stop
const USBCMD_HCRST: u32 = 1 << 1; // Host Controller Reset
const USBSTS_HCH: u32 = 1 << 0; // HCHalted
const USBSTS_EINT: u32 = 1 << 3; // Event Interrupt (write-1-to-clear)
const USBSTS_CNR: u32 = 1 << 11; // Controller Not Ready

const CRCR_RCS: u64 = 1 << 0; // Ring Cycle State (initial producer cycle)

// Interrupter 0 register set, offset from the runtime base.
const IR0: u64 = 0x20;
const IR_ERSTSZ: u64 = 0x08; // Event Ring Segment Table Size
const IR_ERSTBA: u64 = 0x10; // ERST Base Address (64-bit)
const IR_ERDP: u64 = 0x18; // Event Ring Dequeue Pointer (64-bit)
const ERDP_EHB: u64 = 1 << 3; // Event Handler Busy (write-1-to-clear)

// Port status/control register (PORTSC) bits (xHCI 5.4.8).
const PORTSC_CCS: u32 = 1 << 0; // Current Connect Status
const PORTSC_PED: u32 = 1 << 1; // Port Enabled/Disabled (RW1CS)
const PORTSC_PR: u32 = 1 << 4; // Port Reset
const PORTSC_PP: u32 = 1 << 9; // Port Power
const PORTSC_CHANGE_BITS: u32 = 0x7F << 17; // CSC/PEC/WRC/OCC/PRC/PLC/CEC (RW1C)

/// A brought-up xHCI controller: the mapped register bases, the parsed capability
/// parameters, and the command + event rings it drives. The rings are owned
/// (self-contained raw-pointer types), so the whole controller is a movable driver
/// object -- a kernel resident today, and the shape a library OS could hold to
/// drive the device directly (the exokernel end-state, as in direct binding).
pub struct Xhci {
    mmio: u64,
    op: u64,
    rt: u64,
    db: u64,
    ir0: u64,
    erseg_phys: u64,
    dcbaa_va: u64,
    port: u8,
    max_slots: u8,
    max_ports: u8,
    csz: bool,
    cmd: Option<ProducerRing>,
    event: Option<EventRing>,
    dev: Option<Device>,
}

/// The single enumerated device (v1 handles one boot keyboard). Holds its slot,
/// root-hub port and speed, its EP0 control transfer ring, and its Input + output
/// Device Context frames.
struct Device {
    slot: u8,
    port: u8,
    speed: u8,
    ep0: ProducerRing,
    dev_ctx_phys: u64,
    dev_ctx_va: u64,
    input_ctx_phys: u64,
    input_ctx_va: u64,
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

    /// Program the controller and start it, then wait for the first Port Status
    /// Change Event (a device connected at a root port posts one on Run). Sets up
    /// the DCBAA (+ scratchpad if required), the command ring (CRCR), and a
    /// one-segment event ring on interrupter 0 (polled -- no MSI-X yet, D5's
    /// bring-up fallback), then sets Run/Stop. Returns false on a fatal setup
    /// failure; a missing port event is reported but not fatal.
    fn program_and_run<W: Write>(&mut self, out: &mut W) -> bool {
        let trbs_per_frame = (crate::frame_alloc::FRAME_SIZE / 16) as usize;

        // Enable device slots (CONFIG.MaxSlotsEn).
        let slots = self.max_slots.max(1);
        w32(self.op + OP_CONFIG, slots as u32);

        // Device Context Base Address Array, plus scratchpad buffers if the
        // controller demands any (HCSPARAMS2 Max Scratchpad Bufs Hi[25:21] Lo[31:27]).
        let (dcbaa_phys, dcbaa_va) = match alloc_zeroed() {
            Ok(x) => x,
            Err(e) => {
                let _ = writeln!(out, "plinth: xhci: dcbaa alloc: {e}");
                return false;
            }
        };
        let hcs2 = r32(self.mmio + CAP_HCSPARAMS2);
        let scratch = (((hcs2 >> 21) & 0x1F) << 5) | ((hcs2 >> 27) & 0x1F);
        if scratch > 0 {
            let (arr_phys, arr_va) = match alloc_zeroed() {
                Ok(x) => x,
                Err(e) => {
                    let _ = writeln!(out, "plinth: xhci: scratchpad array alloc: {e}");
                    return false;
                }
            };
            for i in 0..scratch as u64 {
                let (buf_phys, _) = match alloc_zeroed() {
                    Ok(x) => x,
                    Err(e) => {
                        let _ = writeln!(out, "plinth: xhci: scratchpad buffer alloc: {e}");
                        return false;
                    }
                };
                // Safety: arr_va is a mapped, page-sized frame; i < scratch fits.
                unsafe { core::ptr::write_volatile((arr_va + i * 8) as *mut u64, buf_phys) };
            }
            // DCBAA[0] points at the scratchpad buffer array (xHCI 6.1).
            unsafe { core::ptr::write_volatile(dcbaa_va as *mut u64, arr_phys) };
        }
        w64(self.op + OP_DCBAAP, dcbaa_phys);
        self.dcbaa_va = dcbaa_va; // kept so Address Device can set DCBAA[slot]

        // Command ring: build the producer ring over the frame (this writes the
        // Link TRB), store it on the controller, then point CRCR at it with the
        // initial cycle state.
        let (cr_phys, cr_va) = match alloc_zeroed() {
            Ok(x) => x,
            Err(e) => {
                let _ = writeln!(out, "plinth: xhci: command ring alloc: {e}");
                return false;
            }
        };
        // Safety: cr_va is a mapped page-sized frame; trbs_per_frame TRBs fit.
        self.cmd = Some(unsafe { ProducerRing::new(cr_va as *mut Trb, trbs_per_frame, cr_phys) });
        w64(self.op + OP_CRCR, cr_phys | CRCR_RCS);

        // Event ring: one segment + a one-entry ERST on interrupter 0.
        let (erseg_phys, erseg_va) = match alloc_zeroed() {
            Ok(x) => x,
            Err(e) => {
                let _ = writeln!(out, "plinth: xhci: event ring alloc: {e}");
                return false;
            }
        };
        let (erst_phys, erst_va) = match alloc_zeroed() {
            Ok(x) => x,
            Err(e) => {
                let _ = writeln!(out, "plinth: xhci: erst alloc: {e}");
                return false;
            }
        };
        let entry = ErstEntry { base_phys: erseg_phys, size: trbs_per_frame as u16 }.encode();
        for (i, word) in entry.iter().enumerate() {
            // Safety: erst_va is a mapped frame; the ERST entry is 16 bytes.
            unsafe { core::ptr::write_volatile((erst_va + (i as u64) * 4) as *mut u32, *word) };
        }
        // Order per xHCI 4.9.4: size, then dequeue, then base (the base write arms it).
        w32(self.ir0 + IR_ERSTSZ, 1);
        w64(self.ir0 + IR_ERDP, erseg_phys);
        w64(self.ir0 + IR_ERSTBA, erst_phys);
        self.erseg_phys = erseg_phys;
        // Safety: erseg_va is a mapped frame of trbs_per_frame zeroed TRBs.
        self.event = Some(unsafe { EventRing::new(erseg_va as *const Trb, trbs_per_frame) });

        // Run.
        let cmd = r32(self.op + OP_USBCMD);
        w32(self.op + OP_USBCMD, cmd | USBCMD_RS);
        if !wait_until(|| r32(self.op + OP_USBSTS) & USBSTS_HCH == 0) {
            let _ = writeln!(out, "plinth: xhci: run failed (controller stayed halted)");
            return false;
        }
        let _ = writeln!(out, "plinth: xhci: running (slots enabled {slots})");

        // Reset the connected port(s) to generate a Port Status Change Event. A
        // device attached before the event ring was armed only latches the port's
        // change bits (no event re-posts on Run), so a fresh Port Reset is what
        // produces an event the ring delivers -- and it is the first enumeration
        // step regardless. PORTSC[p] = op + 0x400 + (p-1)*0x10.
        let mut port = None;
        for p in 1..=self.max_ports {
            let addr = self.op + 0x400 + (p as u64 - 1) * 0x10;
            let portsc = r32(addr);
            if portsc & PORTSC_CCS == 0 {
                continue; // no device on this port
            }
            // Assert Port Reset with power on; drop the RW1C change bits and PED so
            // this write neither clears a pending change nor disables the port.
            let base = portsc & !PORTSC_CHANGE_BITS & !PORTSC_PED;
            w32(addr, base | PORTSC_PR | PORTSC_PP);
            if let Some(ev) = self.poll_event(TrbType::PortStatusChange as u8) {
                port = Some(ev.port_id());
                self.port = p; // the connected, now-reset port -- Address Device targets it
                break;
            }
        }
        // Acknowledge the event interrupt bit (write-1-to-clear).
        w32(self.op + OP_USBSTS, USBSTS_EINT);

        match port {
            Some(p) => {
                let _ = writeln!(
                    out,
                    "plinth: xhci: port status change on port {p} (device connected)"
                );
            }
            None => {
                let _ = writeln!(out, "plinth: xhci: no port event seen");
            }
        }
        true
    }

    /// Poll the event ring for the next event of `want_type`, draining (and
    /// ignoring) any other events, then advance ERDP past what was consumed and
    /// clear Event Handler Busy. Bounded, so a missing event reports rather than
    /// hangs. Returns None if the wanted event never arrived.
    fn poll_event(&mut self, want_type: u8) -> Option<Trb> {
        let ir0 = self.ir0;
        let erseg_phys = self.erseg_phys;
        let event = self.event.as_mut()?;
        let mut found = None;
        for _ in 0..20_000_000u64 {
            match event.poll() {
                Some(ev) => {
                    if ev.trb_type() == want_type {
                        found = Some(ev);
                        break;
                    }
                    // A different event this early: keep draining.
                }
                None => core::hint::spin_loop(),
            }
        }
        // Advance ERDP to the current dequeue slot and clear Event Handler Busy.
        let erdp = erseg_phys + (event.dequeue_index() as u64) * 16;
        w64(ir0 + IR_ERDP, erdp | ERDP_EHB);
        found
    }

    /// Issue an Enable Slot command and await its Command Completion Event -- the
    /// first device-enumeration step (step 2). Rings the command doorbell (DB0,
    /// target 0). Returns the device slot id the controller assigned, or None on
    /// failure. Proves the command ring + doorbell + completion path end to end.
    fn enable_slot<W: Write>(&mut self, out: &mut W) -> Option<u8> {
        self.cmd.as_mut()?.push(Trb::enable_slot());
        // Ring the command doorbell (doorbell 0, target 0).
        w32(self.db, 0);

        let ev = self.poll_event(TrbType::CommandCompletion as u8)?;
        // Completion code 1 = Success (xHCI table 6-90).
        if ev.completion_code() != 1 {
            let _ = writeln!(
                out,
                "plinth: xhci: enable slot failed (completion code {})",
                ev.completion_code()
            );
            return None;
        }
        let slot = ev.event_slot_id();
        let _ = writeln!(out, "plinth: xhci: enable slot ok -> slot id {slot}");
        Some(slot)
    }

    /// Port speed field from PORTSC[port] (bits 10-13); valid after the port is
    /// reset/enabled.
    fn port_speed(&self, port: u8) -> u8 {
        ((r32(self.op + 0x400 + (port as u64 - 1) * 0x10) >> 10) & 0xF) as u8
    }

    /// Address Device (step 2): give the enumerated slot a Device Context and an
    /// EP0 control-transfer ring, hand the controller an Input Context describing
    /// the slot + EP0, and issue the command (BSR=0, so it also sends the USB
    /// SET_ADDRESS). Records the device on success. This is the last command-only
    /// enumeration step; reading descriptors over EP0 is the next slice.
    fn address_device<W: Write>(&mut self, slot: u8, out: &mut W) -> bool {
        let trbs_per_frame = (crate::frame_alloc::FRAME_SIZE / 16) as usize;
        // Context stride: 64 bytes if the controller uses large contexts (CSZ=1),
        // else 32. QEMU is 32; computed so the layout is right either way.
        let cs = if self.csz { 0x40u64 } else { 0x20u64 };

        let port = self.port;
        let speed = self.port_speed(port);
        let mps = ep0_mps(speed);

        // Output Device Context (the controller writes it), the Input Context (we
        // write it), and the EP0 transfer ring -- one zeroed frame each.
        let (dev_ctx_phys, dev_ctx_va) = match alloc_zeroed() {
            Ok(x) => x,
            Err(e) => {
                let _ = writeln!(out, "plinth: xhci: device context alloc: {e}");
                return false;
            }
        };
        let (input_ctx_phys, input_ctx_va) = match alloc_zeroed() {
            Ok(x) => x,
            Err(e) => {
                let _ = writeln!(out, "plinth: xhci: input context alloc: {e}");
                return false;
            }
        };
        let (ep0_phys, ep0_va) = match alloc_zeroed() {
            Ok(x) => x,
            Err(e) => {
                let _ = writeln!(out, "plinth: xhci: ep0 ring alloc: {e}");
                return false;
            }
        };
        // Safety: ep0_va is a mapped page-sized frame; trbs_per_frame TRBs fit.
        let ep0 = unsafe { ProducerRing::new(ep0_va as *mut Trb, trbs_per_frame, ep0_phys) };

        // Input Control Context (offset 0): add the slot (A0) and EP0 (A1).
        let mut icc = InputControlContext::zeroed();
        icc.add_context(0);
        icc.add_context(dci(0, true));
        write_context(input_ctx_va, &icc.dwords);

        // Slot Context (offset cs): root-hub-attached, one context entry (EP0).
        let mut sc = SlotContext::zeroed();
        sc.set_route_string(0);
        sc.set_speed(speed);
        sc.set_context_entries(1);
        sc.set_root_hub_port(port);
        write_context(input_ctx_va + cs, &sc.dwords);

        // EP0 Endpoint Context (offset cs*2 = DCI 1): control endpoint, its TR
        // dequeue pointer = the EP0 ring, DCS matching the ring's initial PCS.
        let mut ep = EndpointContext::zeroed();
        ep.set_ep_type(EpType::Control);
        ep.set_max_packet_size(mps);
        ep.set_error_count(3);
        ep.set_tr_dequeue(ep0_phys, true);
        ep.set_avg_trb_len(8);
        write_context(input_ctx_va + cs * 2, &ep.dwords);

        // DCBAA[slot] -> the output Device Context.
        unsafe {
            core::ptr::write_volatile((self.dcbaa_va + slot as u64 * 8) as *mut u64, dev_ctx_phys)
        };

        // Issue the command and await completion.
        if let Some(c) = self.cmd.as_mut() {
            c.push(Trb::address_device(input_ctx_phys, slot, false));
        }
        w32(self.db, 0);
        let ev = match self.poll_event(TrbType::CommandCompletion as u8) {
            Some(e) => e,
            None => {
                let _ = writeln!(out, "plinth: xhci: address device: no completion event");
                return false;
            }
        };
        if ev.completion_code() != 1 {
            let _ = writeln!(
                out,
                "plinth: xhci: address device failed (completion code {})",
                ev.completion_code()
            );
            return false;
        }
        // The assigned USB address lands in the output slot context (dword3 bits 0-7).
        let usb_addr = unsafe { core::ptr::read_volatile((dev_ctx_va + 0x0C) as *const u32) } & 0xFF;
        let _ = writeln!(
            out,
            "plinth: xhci: address device ok (slot {slot}, port {port}, speed {speed}, usb addr {usb_addr})"
        );

        self.dev = Some(Device {
            slot,
            port,
            speed,
            ep0,
            dev_ctx_phys,
            dev_ctx_va,
            input_ctx_phys,
            input_ctx_va,
        });
        true
    }

    /// Issue a control transfer on the device's EP0 ring (Setup [+ Data] + Status),
    /// ring the EP0 doorbell, and await the Transfer Event. `req_type/req/value/
    /// index` are the USB setup fields; `buf_phys`/`len` the data buffer (`len` 0
    /// for a no-data request). Only IN-data and no-data requests are issued here.
    /// Returns true on Success or Short Packet.
    fn control_in<W: Write>(
        &mut self,
        req_type: u8,
        req: u8,
        value: u16,
        index: u16,
        buf_phys: u64,
        len: u16,
        out: &mut W,
    ) -> bool {
        let slot = match self.dev.as_ref() {
            Some(d) => d.slot,
            None => return false,
        };
        let setup = (req_type as u64)
            | ((req as u64) << 8)
            | ((value as u64) << 16)
            | ((index as u64) << 32)
            | ((len as u64) << 48);
        // TRT: 3 = IN data, 0 = no data.
        let trt = if len > 0 { 3 } else { 0 };
        match self.dev.as_mut() {
            Some(dev) => {
                dev.ep0.push(Trb::setup_stage(setup, trt));
                if len > 0 {
                    dev.ep0.push(Trb::data_stage(buf_phys, len as u32, true));
                }
                // Status direction is opposite the data: OUT after IN data, IN for
                // a no-data request. IOC so exactly one Transfer Event fires.
                dev.ep0.push(Trb::status_stage(len == 0, true));
            }
            None => return false,
        }
        // Ring the EP0 doorbell: doorbell[slot], target = EP0 DCI (1).
        w32(self.db + slot as u64 * 4, dci(0, true) as u32);
        let ev = match self.poll_event(TrbType::TransferEvent as u8) {
            Some(e) => e,
            None => {
                let _ = writeln!(out, "plinth: xhci: control transfer: no completion");
                return false;
            }
        };
        let code = ev.completion_code();
        // 1 = Success, 13 = Short Packet (a device may legitimately return fewer bytes).
        if code != 1 && code != 13 {
            let _ = writeln!(out, "plinth: xhci: control transfer failed (code {code})");
            return false;
        }
        true
    }

    /// Read the device + configuration descriptors over EP0 and set the
    /// configuration -- the descriptor half of step 2. Reports VID/PID and whether
    /// the device is a boot-protocol HID keyboard (interface class 3 / sub 1 /
    /// protocol 1).
    fn describe_device<W: Write>(&mut self, out: &mut W) -> bool {
        let (buf_phys, buf_va) = match alloc_zeroed() {
            Ok(x) => x,
            Err(e) => {
                let _ = writeln!(out, "plinth: xhci: descriptor buffer alloc: {e}");
                return false;
            }
        };
        // Safety: buf_va is a mapped frame; all reads below are within its 64 bytes.
        let rd8 = |off: u64| -> u8 { unsafe { core::ptr::read_volatile((buf_va + off) as *const u8) } };
        let rd16 = |off: u64| -> u16 { (rd8(off) as u16) | ((rd8(off + 1) as u16) << 8) };

        // Device descriptor (18 bytes): GET_DESCRIPTOR, type 1 (device).
        if !self.control_in(0x80, 6, 1 << 8, 0, buf_phys, 18, out) {
            return false;
        }
        let vid = rd16(8);
        let pid = rd16(10);
        let dev_class = rd8(4);
        let _ = writeln!(
            out,
            "plinth: xhci: device descriptor: vid {vid:#06x} pid {pid:#06x} class {dev_class}"
        );

        // Configuration descriptor: read up to 64 bytes (config + interface + ...).
        // Zero the buffer first so a short read leaves no stale bytes behind.
        unsafe { core::ptr::write_bytes(buf_va as *mut u8, 0, 64) };
        if !self.control_in(0x80, 6, 2 << 8, 0, buf_phys, 64, out) {
            return false;
        }
        let total = (rd16(2) as u64).min(64);
        let config_value = rd8(5);
        // Walk the descriptor chain for the first interface descriptor (type 4).
        let mut off = rd8(0) as u64; // skip the 9-byte config descriptor
        let mut iface = None;
        while off + 2 <= total {
            let blen = rd8(off) as u64;
            if blen == 0 {
                break;
            }
            if rd8(off + 1) == 4 && off + 8 <= total {
                iface = Some((rd8(off + 5), rd8(off + 6), rd8(off + 7)));
                break;
            }
            off += blen;
        }
        match iface {
            Some((c, s, p)) => {
                let boot_kbd = c == 3 && s == 1 && p == 1;
                let _ = writeln!(
                    out,
                    "plinth: xhci: interface class {c} subclass {s} protocol {p}{}",
                    if boot_kbd { " (boot keyboard)" } else { "" }
                );
            }
            None => {
                let _ = writeln!(out, "plinth: xhci: no interface descriptor found");
            }
        }

        // Set Configuration (no-data control transfer).
        if !self.control_in(0x00, 9, config_value as u16, 0, 0, 0, out) {
            return false;
        }
        let _ = writeln!(out, "plinth: xhci: configured (config value {config_value})");
        true
    }
}

/// Poll `cond` up to a fixed bound with a relaxed spin. There is no fine-grained
/// timer on this path, so the wait is bounded by iteration count -- generous for
/// TCG QEMU, and a real controller readies in microseconds. Returns false if the
/// condition never held (a real controller/firmware fault), so the caller reports
/// and moves on instead of hanging (first_metal_boot.md D3).
/// Write a 32-byte context (8 dwords) into a DMA frame at `dst_va`, volatile.
fn write_context(dst_va: u64, dwords: &[u32; 8]) {
    for (i, w) in dwords.iter().enumerate() {
        // Safety: dst_va is inside a mapped context frame with room for 8 dwords.
        unsafe { core::ptr::write_volatile((dst_va + (i as u64) * 4) as *mut u32, *w) };
    }
}

/// Initial EP0 max packet size by port speed. Full/Low speed start at 8 (the real
/// value is read from the device descriptor later); High is 64, Super 512.
fn ep0_mps(speed: u8) -> u16 {
    match speed {
        SPEED_HIGH => 64,
        SPEED_SUPER => 512,
        _ => 8, // low/full (and unknown): the safe minimum
    }
}

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
        ir0: mmio + rtsoff + IR0,
        erseg_phys: 0,
        dcbaa_va: 0,
        port: 0,
        max_slots,
        max_ports,
        csz,
        cmd: None,
        event: None,
        dev: None,
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
    if !x.program_and_run(out) {
        return None;
    }
    // Step 2: enumerate the device -- Enable Slot, Address Device, descriptors.
    if let Some(slot) = x.enable_slot(out) {
        if x.address_device(slot, out) {
            x.describe_device(out);
        }
    }
    Some(x)
}
