//! Interrupt-controller seam (Local APIC + I/O APIC, with an 8259 PIC fallback).
//!
//! Every line-IRQ touchpoint that is specific to the interrupt controller --
//! bringing it up, masking/unmasking a line, and sending end-of-interrupt --
//! lives here and nowhere else. Devices (the PIT timer, the i8042 keyboard, the
//! virtio-blk completion line) drive their own device registers but route every
//! controller operation through this module, so nothing above it knows whether a
//! PIC or an APIC delivers the interrupt. See Design/input.md section 4 and
//! Design/broader_hardware.md Stage A2.
//!
//! At boot the 8259 PIC is remapped off the CPU exception vectors and fully
//! masked. If ACPI handed us an interrupt topology (`acpi::Topology`), the seam
//! then retires the PIC: it brings up the Local APIC and the I/O APIC and routes
//! every line through them. Without a MADT it falls back to driving the masked
//! PIC directly. Either way the four operations below are the whole controller
//! surface, and the device modules above never change.
//!
//! Line numbers stay the legacy ISA IRQ numbers (0 = PIT, 1 = keyboard, ...).
//! Under the I/O APIC, `unmask` maps a line to its global system interrupt and
//! polarity/trigger via the MADT Interrupt Source Overrides -- notably the
//! canonical IRQ0 -> GSI2 PIT remap -- and programs the matching redirection
//! entry to deliver `VECTOR_BASE + line` (the vector the device's IDT handler
//! sits at). EOI is a single Local APIC write, which also clears a level line's
//! I/O APIC remote-IRR once the device has been deasserted.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Mutex;
use x86_64::instructions::port::Port;
use x86_64::registers::model_specific::Msr;
use x86_64::structures::idt::InterruptStackFrame;

use crate::{acpi, interrupts, memory, percpu};

// 8259 master/slave command + data ports, and the end-of-interrupt command.
const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;
const PIC_EOI: u8 = 0x20;

/// Vector base line IRQs are delivered at: IRQ `n` is delivered at
/// `VECTOR_BASE + n` (both for the remapped PIC and for the I/O APIC redirection
/// entries). Must be >= 32 -- the CPU exception range is 0..32. The IDT installs
/// device handlers at these vectors, unchanged across the PIC/APIC swap.
pub const VECTOR_BASE: u8 = 0x20;

/// The Local APIC spurious-interrupt vector. Delivered if the LAPIC has nothing
/// real to hand the CPU; its handler does nothing (a spurious needs no EOI).
const SPURIOUS_VECTOR: u8 = 0xFF;

/// IPI vector used to wake an idling core so it re-checks for claimable
/// scheduler work (Stage B2.3, D4/section 5.4). Clear of every other vector
/// in use: line IRQs at `VECTOR_BASE..=VECTOR_BASE+12` (incl. IRQ12, the
/// mouse, mouse_input.md), virtio MSI-X at 0x30/0x31
/// (`virtio_blk::MSIX_VECTOR_BASE`), spurious at 0xFF.
const RESCHEDULE_VECTOR: u8 = 0xF0;

// IA32_APIC_BASE MSR and the Local APIC register offsets we touch.
const IA32_APIC_BASE: u32 = 0x1B;
const LAPIC_ID: u32 = 0x20;
const LAPIC_TPR: u32 = 0x80;
const LAPIC_EOI: u32 = 0xB0;
const LAPIC_SVR: u32 = 0xF0;
// I/O APIC indirect-register index: the version register (its bits 16..24 hold
// the maximum redirection-entry index).
const IOAPIC_VER: u32 = 0x01;

// Local APIC Interrupt Command Register (ICR): two 32-bit halves. ICR_HIGH's
// bits 24..32 are the physical destination APIC id; writing ICR_LOW issues
// the IPI (Intel SDM Vol 3A, 10.6). Public: smp.rs's INIT-SIPI-SIPI sender
// uses the same registers.
pub const ICR_LOW: u32 = 0x300;
pub const ICR_HIGH: u32 = 0x310;
/// Delivery mode Fixed (Intel SDM Table 10-1), in ICR_LOW bits 8..11.
const DELIVERY_FIXED: u32 = 0;
/// Delivery Status (Intel SDM Vol 3A 10.6.1), ICR_LOW bit 12: 1 while a
/// previously written IPI command is still being sent.
const DELIVERY_STATUS_PENDING: u32 = 1 << 12;

/// True once the LAPIC + I/O APIC are up and delivering; false means the PIC
/// fallback is live. Set once at boot, read on every controller op.
static APIC_MODE: AtomicBool = AtomicBool::new(false);
/// The Local APIC's mapped MMIO base (kernel virtual). Read locklessly by `eoi`
/// in the interrupt path.
static LAPIC_VA: AtomicU64 = AtomicU64::new(0);
/// The I/O APIC programming state, set at init and read by `unmask`/`mask`.
static IOAPIC: Mutex<Option<IoApicState>> = Mutex::new(None);
/// Physical APIC id of each AP that reported alive (`smp::start_aps`), indexed
/// by its dense core id; `None` for a core id never brought up. Lets
/// `send_reschedule_ipi` target each online AP individually instead of the
/// "all excluding self" destination shorthand (Stage B2.3: that shorthand,
/// under repeated back-to-back sends with 2+ APs online, was implicated in a
/// real crash found by booting under PLINTH_SMP=3/4 -- never reproduced with
/// it removed, and not reproduced since switching to per-target sends either).
static ONLINE_APIC_IDS: Mutex<[Option<u8>; percpu::MAX_CORES]> = Mutex::new([None; percpu::MAX_CORES]);

/// Record that `core_id` is alive at `apic_id`, so `send_reschedule_ipi` can
/// target it. Call once, from `smp::start_aps`, right after an AP reports in.
pub fn mark_ap_online(core_id: usize, apic_id: u8) {
    ONLINE_APIC_IDS.lock()[core_id] = Some(apic_id);
}

/// What `unmask`/`mask` need to program an I/O APIC redirection entry: the
/// mapped MMIO base, the GSI base, the destination (BSP) APIC id, and the MADT
/// source overrides that remap legacy lines.
struct IoApicState {
    va: u64,
    gsi_base: u32,
    bsp_id: u8,
    isos: [acpi::Iso; acpi::MAX_ISOS],
    iso_count: usize,
}

/// Bring up the interrupt controller. Always remaps and fully masks the 8259
/// first (so a stray PIC line can never land on an exception vector); then, given
/// an ACPI topology, retires the PIC in favour of the LAPIC + I/O APIC. Call once
/// at boot, interrupts off, before any device unmasks its line.
pub fn init(topo: Option<&acpi::Topology>) {
    // SAFETY: single-threaded boot; the fixed legacy PIC ports, programmed
    // exactly once before any interrupt is enabled.
    unsafe {
        remap();
        Port::<u8>::new(PIC1_DATA).write(0xFF);
        Port::<u8>::new(PIC2_DATA).write(0xFF);
    }

    let Some(t) = topo else {
        return; // no MADT: keep driving the (masked) PIC directly.
    };

    // Install the spurious and reschedule-IPI handlers before the LAPIC is
    // software-enabled (no interrupt can fire yet -- IF=0 throughout boot --
    // but keep the ordering honest), bring up the LAPIC and I/O APIC, then
    // commit APIC mode.
    interrupts::set_irq_handler(SPURIOUS_VECTOR, spurious_interrupt);
    interrupts::set_irq_handler(RESCHEDULE_VECTOR, reschedule_interrupt);
    let bsp_id = enable_lapic(t);
    let va = init_ioapic(t);
    *IOAPIC.lock() = Some(IoApicState {
        va,
        gsi_base: t.ioapic_gsi_base,
        bsp_id,
        isos: t.isos,
        iso_count: t.iso_count,
    });
    APIC_MODE.store(true, Ordering::Relaxed);
}

/// Unmask IRQ `line` so the controller delivers it. Under the APIC this programs
/// and unmasks the line's I/O APIC redirection entry; under the PIC it clears the
/// mask bit (and the cascade line for a slave-PIC line).
pub fn unmask(line: u8) {
    if APIC_MODE.load(Ordering::Relaxed) {
        ioapic_route(line, false);
        return;
    }
    set_mask(line, false);
    if line >= 8 {
        set_mask(2, false); // cascade to the slave
    }
}

/// Mask IRQ `line` so the controller stops delivering it.
#[allow(dead_code)] // the symmetric op; used once the mouse line can be disabled
pub fn mask(line: u8) {
    if APIC_MODE.load(Ordering::Relaxed) {
        ioapic_route(line, true);
        return;
    }
    set_mask(line, true);
}

/// True once the LAPIC + I/O APIC are up (vs. the PIC fallback). Lets a device
/// that is itself part of the Local APIC -- today, its per-core timer -- know
/// whether there is a LAPIC to program at all.
pub fn apic_mode() -> bool {
    APIC_MODE.load(Ordering::Relaxed)
}

/// The mapped LAPIC MMIO base, if the APIC is active. The LAPIC's own timer
/// (the LVT Timer + count registers) is local-APIC hardware, not a line IRQ,
/// so `timer.rs` programs it directly through this and `lapic_reg_read`/
/// `lapic_reg_write` rather than through `unmask`/`mask` -- this is the one
/// register window a device needs from the seam to do that. Returns `None`
/// under the PIC fallback, where there is no LAPIC to hand out.
pub fn lapic_base() -> Option<u64> {
    apic_mode().then(|| LAPIC_VA.load(Ordering::Relaxed))
}

/// Read a Local APIC register at `off` from a base returned by `lapic_base`.
/// SAFETY: `va` must be a value `lapic_base` returned (so it is the mapped
/// LAPIC page) and `off` a defined register offset.
pub unsafe fn lapic_reg_read(va: u64, off: u32) -> u32 {
    lapic_read(va, off)
}

/// Write a Local APIC register at `off`. Same SAFETY contract as
/// `lapic_reg_read`.
pub unsafe fn lapic_reg_write(va: u64, off: u32, val: u32) {
    lapic_write(va, off, val)
}

/// The boot CPU's APIC id, if the APIC is active. Needed by anything that
/// targets the LAPIC directly by physical destination -- today, an MSI-X
/// table entry's Message Address (Stage A3, D7) -- the same id the I/O APIC
/// redirection entries already use as their destination.
pub fn bsp_apic_id() -> Option<u8> {
    IOAPIC.lock().as_ref().map(|s| s.bsp_id)
}

/// Acknowledge IRQ `line`. Under the APIC a single Local APIC EOI ends the
/// in-service interrupt (and, for a level I/O APIC line whose device has already
/// been deasserted, clears its remote IRR). Under the PIC, EOI the master, and
/// the slave too for a line >= 8.
pub fn eoi(line: u8) {
    if APIC_MODE.load(Ordering::Relaxed) {
        // SAFETY: the LAPIC MMIO is mapped at init; writing the EOI register only
        // ends the in-service interrupt.
        unsafe { lapic_write(LAPIC_VA.load(Ordering::Relaxed), LAPIC_EOI, 0) };
        return;
    }
    // SAFETY: the fixed PIC command ports; an EOI only ends the in-service IRQ.
    unsafe {
        if line >= 8 {
            Port::<u8>::new(PIC2_CMD).write(PIC_EOI);
        }
        Port::<u8>::new(PIC1_CMD).write(PIC_EOI);
    }
}

// --- Local APIC + I/O APIC (the APIC path) ---

/// IA32_APIC_BASE's global-enable bit (bit 11). Setting it while leaving bit
/// 10 (x2APIC) clear keeps xAPIC/MMIO mode. This MSR is per-core architectural
/// state -- every core that wants its LAPIC live must set this for itself.
fn enable_lapic_msr() {
    // SAFETY: the architectural LAPIC-enable MSR, on this core only.
    unsafe {
        let mut msr = Msr::new(IA32_APIC_BASE);
        let base = msr.read();
        msr.write(base | (1 << 11));
    }
}

/// Software-enable the BSP's Local APIC and return its APIC id. Globally
/// enables the LAPIC via IA32_APIC_BASE, maps its MMIO page (once -- every
/// core's LAPIC sits at the same physical/virtual address by construction;
/// only the *enable* below is per-core), sets the spurious vector (with the
/// enable bit) and a zero task priority (accept all vectors).
fn enable_lapic(t: &acpi::Topology) -> u8 {
    enable_lapic_msr();
    let va = memory::map_kernel_mmio(t.lapic_base, 0x1000).expect("map LAPIC MMIO");
    LAPIC_VA.store(va, Ordering::Relaxed);
    // SAFETY: `va` is the freshly mapped LAPIC MMIO page; these are the defined
    // LAPIC registers, written once at boot with IF=0.
    unsafe {
        let bsp_id = (lapic_read(va, LAPIC_ID) >> 24) as u8;
        lapic_write(va, LAPIC_SVR, (1 << 8) | SPURIOUS_VECTOR as u32);
        lapic_write(va, LAPIC_TPR, 0);
        bsp_id
    }
}

/// Software-enable THIS core's Local APIC and return its own APIC id (Stage
/// B2.2). The MMIO mapping is already up (the BSP's `init` did it -- the
/// same virtual address resolves to each core's own LAPIC hardware); an AP
/// only needs the per-core MSR enable plus its own SVR/TPR. Call once per AP,
/// after the BSP has called `init` with a MADT (i.e. `apic_mode()` is true).
pub fn enable_lapic_ap() -> u8 {
    enable_lapic_msr();
    let va = LAPIC_VA.load(Ordering::Relaxed);
    // SAFETY: `va` is the BSP-mapped LAPIC MMIO page, valid on every core;
    // these are the defined LAPIC registers, written once per AP with IF=0.
    unsafe {
        let id = (lapic_read(va, LAPIC_ID) >> 24) as u8;
        lapic_write(va, LAPIC_SVR, (1 << 8) | SPURIOUS_VECTOR as u32);
        lapic_write(va, LAPIC_TPR, 0);
        id
    }
}

/// Map the I/O APIC and mask every redirection entry (a clean slate -- devices
/// unmask their own line). Returns the mapped MMIO base.
fn init_ioapic(t: &acpi::Topology) -> u64 {
    let va = memory::map_kernel_mmio(t.ioapic_base, 0x1000).expect("map IOAPIC MMIO");
    // SAFETY: `va` is the freshly mapped I/O APIC MMIO page; the indirect
    // register pair is the defined access method, used once at boot with IF=0.
    unsafe {
        let max_entry = (ioapic_read(va, IOAPIC_VER) >> 16) & 0xFF;
        for n in 0..=max_entry {
            ioapic_write(va, 0x10 + 2 * n, 1 << 16); // low: masked
            ioapic_write(va, 0x11 + 2 * n, 0); // high: destination 0
        }
    }
    va
}

/// Program (and mask or unmask) the I/O APIC redirection entry for ISA `line`:
/// resolve its GSI and polarity/trigger from the MADT overrides, and route the
/// matching redirection entry to deliver `VECTOR_BASE + line` to the BSP.
fn ioapic_route(line: u8, masked: bool) {
    let guard = IOAPIC.lock();
    let Some(state) = guard.as_ref() else {
        return;
    };
    let (gsi, active_low, level) = resolve(state, line);
    if gsi < state.gsi_base {
        return; // not this I/O APIC's range
    }
    let entry = gsi - state.gsi_base;
    let reg_lo = 0x10 + 2 * entry;
    let reg_hi = 0x11 + 2 * entry;

    // Low word: vector, fixed delivery (000), physical destination (0), polarity
    // and trigger from the override, and the mask bit. High word: destination
    // APIC id in bits 56..64 (i.e. the high register's bits 24..32).
    let mut low = (VECTOR_BASE + line) as u32;
    if active_low {
        low |= 1 << 13;
    }
    if level {
        low |= 1 << 15;
    }
    if masked {
        low |= 1 << 16;
    }
    let high = (state.bsp_id as u32) << 24;

    // SAFETY: `state.va` is the mapped I/O APIC; `entry` is within this APIC's
    // GSI range (checked above). Write the destination first, then the low word,
    // both with IF=0.
    unsafe {
        ioapic_write(state.va, reg_hi, high);
        ioapic_write(state.va, reg_lo, low);
    }
}

/// Resolve an ISA `line` to its (GSI, active-low, level) via the MADT source
/// overrides, defaulting to the ISA convention (GSI = line, active high, edge).
fn resolve(state: &IoApicState, line: u8) -> (u32, bool, bool) {
    for iso in &state.isos[..state.iso_count] {
        if iso.source == line {
            return (iso.gsi, iso.active_low, iso.level);
        }
    }
    (line as u32, false, false)
}

/// The Local APIC spurious-interrupt handler: nothing to do, and no EOI.
extern "x86-interrupt" fn spurious_interrupt(_frame: InterruptStackFrame) {}

/// Wake every other online core out of `hlt` so it re-checks for claimable
/// scheduler work (Stage B2.3, section 5.4) -- the scheduler calls this
/// whenever it makes a slot Ready (`scheduler::setup_process`/`wake_with`).
/// A no-op under the PIC fallback (no LAPIC to send from -- the same
/// reasoning as every other APIC-only operation in this module), and a no-op
/// if no AP has reported online yet.
///
/// Sends one targeted (physical destination) IPI per online AP rather than
/// using the ICR's "all excluding self" destination shorthand. The shorthand
/// is simpler and was the first implementation, but with 2+ APs online it
/// was directly implicated in a real, repeatable crash (found by booting
/// under `PLINTH_SMP=3`/`4`, never `PLINTH_SMP=2` where there is only one
/// possible target): disabling the IPI entirely made the crash all but
/// disappear, and per-target sends below have shown the same stability in
/// the same repeated testing. Whether the exact mechanism is a QEMU xAPIC
/// emulation gap with the broadcast shorthand specifically, or something
/// about the SDM's delivery-status discipline under broadcast that a fixed
/// per-target dest sidesteps, isn't nailed down -- but per-target sends are
/// no less correct (every online AP still gets woken) and are the safer
/// choice either way.
pub fn send_reschedule_ipi() {
    let Some(va) = lapic_base() else {
        return; // no MADT / no LAPIC -- nothing to IPI, and no AP woke either
    };
    let targets = *ONLINE_APIC_IDS.lock();
    for apic_id in targets.into_iter().flatten() {
        let dest = (apic_id as u32) << 24;
        // SAFETY: `va` came from `lapic_base()`, so the APIC is up; `dest`
        // names a specific physical APIC id this function's own caller
        // recorded as online.
        unsafe {
            // Intel SDM Vol 3A 10.6.1: software must not write a new ICR
            // command while the previous one's Delivery Status (bit 12) is
            // still "send pending" -- callers here (setup_process,
            // wake_with) can issue several of these back to back (one per
            // online AP, here, and one per process in run()'s setup loop),
            // so without this wait a later send can land on top of an
            // in-flight one. Bounded by nothing on real hardware (delivery
            // is fast), but capped here defensively rather than risk an
            // infinite spin if delivery status ever genuinely doesn't clear.
            let mut spins = 0;
            while lapic_read(va, ICR_LOW) & DELIVERY_STATUS_PENDING != 0 && spins < 100_000 {
                spins += 1;
            }
            lapic_write(va, ICR_HIGH, dest);
            lapic_write(va, ICR_LOW, DELIVERY_FIXED | RESCHEDULE_VECTOR as u32);
        }
    }
}

/// The reschedule IPI's handler: its only job is to break a `hlt` -- the
/// woken core's own idle loop (`scheduler::ap_idle_loop` /
/// `idle_until_runnable`) re-checks TABLE itself, so there is nothing else
/// to do here besides EOI.
extern "x86-interrupt" fn reschedule_interrupt(_frame: InterruptStackFrame) {
    // SAFETY: a single Local APIC EOI write, the same as any other
    // APIC-delivered interrupt; this handler is only ever installed once
    // APIC_MODE is the live controller.
    unsafe { lapic_write(LAPIC_VA.load(Ordering::Relaxed), LAPIC_EOI, 0) };
}

unsafe fn lapic_read(va: u64, off: u32) -> u32 {
    read_volatile((va + off as u64) as *const u32)
}

unsafe fn lapic_write(va: u64, off: u32, val: u32) {
    write_volatile((va + off as u64) as *mut u32, val);
}

/// Read an I/O APIC indirect register: select it via IOREGSEL (offset 0), read
/// the value from IOWIN (offset 0x10).
unsafe fn ioapic_read(va: u64, reg: u32) -> u32 {
    write_volatile(va as *mut u32, reg);
    read_volatile((va + 0x10) as *const u32)
}

/// Write an I/O APIC indirect register.
unsafe fn ioapic_write(va: u64, reg: u32, val: u32) {
    write_volatile(va as *mut u32, reg);
    write_volatile((va + 0x10) as *mut u32, val);
}

// --- 8259 PIC (the fallback path, and the boot-time disable) ---

/// Set or clear the mask bit for `line` in its PIC's interrupt-mask register
/// (read-modify-write, so it never disturbs the other lines).
fn set_mask(line: u8, masked: bool) {
    let (data_port, bit) = if line < 8 {
        (PIC1_DATA, line)
    } else {
        (PIC2_DATA, line - 8)
    };
    // SAFETY: the fixed PIC data ports hold the interrupt-mask register.
    unsafe {
        let mut port = Port::<u8>::new(data_port);
        let mut imr: u8 = port.read();
        if masked {
            imr |= 1 << bit;
        } else {
            imr &= !(1 << bit);
        }
        port.write(imr);
    }
}

/// ICW1-4: master -> VECTOR_BASE, slave -> VECTOR_BASE+8, 8086 mode.
unsafe fn remap() {
    let mut c1 = Port::<u8>::new(PIC1_CMD);
    let mut d1 = Port::<u8>::new(PIC1_DATA);
    let mut c2 = Port::<u8>::new(PIC2_CMD);
    let mut d2 = Port::<u8>::new(PIC2_DATA);

    c1.write(0x11); io_wait(); // ICW1: begin init, ICW4 to follow
    c2.write(0x11); io_wait();
    d1.write(VECTOR_BASE); io_wait(); // ICW2: master vector offset
    d2.write(VECTOR_BASE + 8); io_wait(); // ICW2: slave vector offset
    d1.write(0x04); io_wait(); // ICW3: slave is wired to master IRQ2
    d2.write(0x02); io_wait(); // ICW3: slave cascade identity
    d1.write(0x01); io_wait(); // ICW4: 8086 mode
    d2.write(0x01); io_wait();
}

/// A brief settling delay between PIC command bytes, by writing an unused port.
/// Real 8259s need it between ICW writes; harmless on QEMU.
unsafe fn io_wait() {
    Port::<u8>::new(0x80).write(0u8);
}
