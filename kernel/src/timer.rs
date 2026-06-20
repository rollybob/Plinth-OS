//! The periodic tick that drives preemptive scheduling -- the PIT (8254) under
//! the PIC fallback, the Local APIC's own per-core timer once Stage A2 has
//! brought up the APIC (Design/broader_hardware.md D9).
//!
//! This module owns the timer *device(s)*: program a periodic source and count
//! ticks. Every interrupt-*controller* operation (the remap, the unmask, the
//! EOI) goes through the `irq` seam; the LAPIC timer is local-APIC hardware
//! rather than a line IRQ, so it is programmed directly through the seam's
//! `lapic_base`/`lapic_reg_*` window instead of `unmask`. Either way this
//! module delivers the SAME vector (`TIMER_VECTOR`) the scheduler already
//! installed its naked context-switch stub at, so nothing above this module
//! changes. The interrupt *handler* and the context switch it drives live in
//! `scheduler.rs` (which calls `note_tick` + `irq::eoi` from its `timer_tick`).
//!
//! Interrupt discipline (the basis for a non-preemptible kernel): ring 3 runs
//! with interrupts enabled (usermode.rs sets IF in the user RFLAGS), so the
//! timer fires while a user process is on the CPU. Kernel code always runs with
//! interrupts disabled -- syscalls mask IF via SFMask, and the handler is
//! reached through an interrupt gate (which clears IF on entry) -- so the
//! kernel is never reentered by the timer and holds no lock across a tick.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::instructions::port::Port;

use crate::irq;

/// IRQ0's legacy vector, and also where the LAPIC timer's LVT entry points:
/// the same vector either way, so `scheduler::register`'s IDT installation and
/// `timer_entry` stub need no per-source variant.
pub const TIMER_VECTOR: usize = irq::VECTOR_BASE as usize; // IRQ0 -> VECTOR_BASE + 0

// 8254 PIT: channel-0 data and command ports, and the channel input clock.
const PIT_CH0: u16 = 0x40;
const PIT_CMD: u16 = 0x43;
const PIT_HZ: u32 = 1_193_182;

// Local APIC timer registers (offsets within the LAPIC MMIO page; see
// `irq::lapic_base`). LVT_TIMER's low byte is the vector; bit 16 masks
// delivery, bit 17 selects periodic mode (one-shot is bit 17 = 0).
const LVT_TIMER: u32 = 0x320;
const TIMER_INITIAL_COUNT: u32 = 0x380;
const TIMER_CURRENT_COUNT: u32 = 0x390;
const TIMER_DIVIDE_CONFIG: u32 = 0x3E0;
const LVT_MASKED: u32 = 1 << 16;
const LVT_PERIODIC: u32 = 1 << 17;
/// Divide the LAPIC's bus clock by 16 before counting -- a fast enough tick
/// for a 100 Hz target without the initial-count register running too close
/// to zero (and so to its rounding error) at the divide-by-1 end.
const DIVIDE_BY_16: u32 = 0b011;

/// Ticks since the timer was armed. The scheduler bumps it once per tick and
/// the boot path prints it as proof the timer fired.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Program a periodic `hz`-Hz tick at `TIMER_VECTOR` and enable it. The
/// interrupt controller must already be initialised (`irq::init`). Call once
/// at boot, interrupts off. Uses the LAPIC's own timer once Stage A2 has
/// brought it up (`irq::apic_mode()`); falls back to the PIT under the PIC
/// (no LAPIC to program there).
pub fn arm(hz: u32) {
    if let Some(va) = irq::lapic_base() {
        // SAFETY: `va` came from `lapic_base`, so it is the mapped LAPIC page;
        // single-threaded boot, IF=0.
        unsafe { arm_lapic_timer(va, hz) };
        return;
    }
    // SAFETY: single-threaded boot; the fixed PIT ports, programmed once.
    unsafe { program_pit(hz) };
    irq::unmask(0); // IRQ0 (the timer line)
}

/// Ticks elapsed since `arm`.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Record one timer tick. Called once per IRQ0 from `scheduler::timer_tick`.
pub fn note_tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Program channel 0 as a rate generator (mode 2) with the 16-bit divisor that
/// yields `hz` interrupts per second.
unsafe fn program_pit(hz: u32) {
    let divisor = (PIT_HZ / hz) as u16;
    let mut cmd = Port::<u8>::new(PIT_CMD);
    let mut ch0 = Port::<u8>::new(PIT_CH0);
    cmd.write(0x34); // channel 0, lobyte/hibyte access, mode 2, binary
    ch0.write((divisor & 0xFF) as u8);
    ch0.write((divisor >> 8) as u8);
}

/// Latch and read the PIT's free-running 16-bit countdown value (channel 0,
/// whatever mode it is currently in). Used only as a polled time reference for
/// LAPIC-timer calibration below -- its output pin is never wired to an IRQ
/// for this purpose, so it never competes with the LAPIC for the tick.
unsafe fn read_pit_count() -> u16 {
    Port::<u8>::new(PIT_CMD).write(0x00); // latch channel 0's current count
    let lo = Port::<u8>::new(PIT_CH0).read() as u16;
    let hi = Port::<u8>::new(PIT_CH0).read() as u16;
    (hi << 8) | lo
}

/// Bound on a busy-wait poll, mirroring the no-silent-hang discipline
/// `keyboard.rs`'s `POLL_MAX` uses for i8042 status polling: a wedged or
/// absent PIT degrades to a (wrong but finite) wait instead of hanging boot.
const WAIT_POLL_MAX: u32 = 50_000_000;

/// How many PIT ticks (at `PIT_HZ`) the LAPIC-timer calibration window spans:
/// ~10 ms, short enough that the PIT's free-running 16-bit count (programmed
/// with the max divisor, ~52.6 ms per wrap) never wraps during the wait.
const CAL_WINDOW_PIT_TICKS: u16 = (PIT_HZ / 100) as u16;

/// Largest single busy-wait this module will attempt, in PIT ticks: stays
/// comfortably under the ~52.6 ms wrap period `program_pit(19)` (the wait
/// clock below) gives the free-running counter. A caller wanting longer than
/// this should loop, not raise the bound.
const MAX_WAIT_PIT_TICKS: u32 = 50_000;

/// Free-run the PIT (channel 0, max divisor -- never unmasked at the
/// controller, so this never competes with the real tick source) and busy-poll
/// until `window_ticks` of it have elapsed. The shared core of LAPIC-timer
/// calibration (`arm_lapic_timer`) and the general-purpose `busy_wait_us`
/// below -- both just need "wait roughly this long," timed against a clock
/// Plinth already drives, the same bounded-poll discipline `pci.rs`/`acpi.rs`
/// use for hardware discovery.
unsafe fn busy_wait_pit_ticks(window_ticks: u16) {
    program_pit(19); // PIT_HZ / 19 ~= 62799, the max-divisor-ish slow rate
    let start = read_pit_count();
    let mut spins = 0u32;
    loop {
        // The PIT counts DOWN, so elapsed ticks are start - current; wrapping
        // sub is correct even across the rare boundary case.
        if start.wrapping_sub(read_pit_count()) >= window_ticks {
            break;
        }
        spins += 1;
        if spins >= WAIT_POLL_MAX {
            break; // wedged/absent PIT: fall through having waited less
        }
        core::hint::spin_loop();
    }
}

/// Busy-wait approximately `us` microseconds, timed against the PIT. Used for
/// the INIT-SIPI-SIPI delays (broader hardware, Stage B1): the LAPIC has no
/// general-purpose wait primitive of its own, and at AP-bring-up time nothing
/// else is using the PIT (the tick source by then is either the PIT itself, in
/// which case this just borrows its free-running count between real ticks, or
/// the LAPIC timer, in which case the PIT is otherwise idle). Clamped to
/// `MAX_WAIT_PIT_TICKS` -- a caller wanting longer should call this more than
/// once.
pub fn busy_wait_us(us: u32) {
    let ticks = ((PIT_HZ as u64 * us as u64) / 1_000_000).min(MAX_WAIT_PIT_TICKS as u64) as u16;
    // SAFETY: single-threaded boot-path use; the fixed PIT ports, the same
    // ones `arm`/calibration already drive.
    unsafe { busy_wait_pit_ticks(ticks) };
}

/// Measure how many LAPIC timer counts (at `DIVIDE_BY_16`) elapse during a
/// fixed ~10 ms window timed by the free-running PIT, then program the LAPIC's
/// LVT Timer for a periodic `hz`-Hz tick at `TIMER_VECTOR`. There is no
/// crystal-clock CPUID leaf to trust under `-cpu qemu64`, so calibrating
/// against a clock Plinth already drives (the PIT) is the bounded, no-crate,
/// hand-rolled approach this codebase otherwise uses for hardware discovery
/// (`pci.rs`, `acpi.rs`). One calibration pass, no averaging -- adequate for a
/// preemption tick whose only correctness requirement is "fires periodically,
/// fast enough," not a precise wall clock.
unsafe fn arm_lapic_timer(va: u64, hz: u32) {
    // One-shot LAPIC count-down from the max value, masked (no interrupt
    // while calibrating) so this is side-effect-free if the wait below times
    // out. Program it BEFORE the PIT wait below reprograms PIT channel 0 --
    // the LAPIC count-down and the PIT wait window both need to start at
    // (approximately) the same instant.
    irq::lapic_reg_write(va, TIMER_DIVIDE_CONFIG, DIVIDE_BY_16);
    irq::lapic_reg_write(va, LVT_TIMER, LVT_MASKED);
    irq::lapic_reg_write(va, TIMER_INITIAL_COUNT, 0xFFFF_FFFF);

    busy_wait_pit_ticks(CAL_WINDOW_PIT_TICKS);

    let lapic_per_window = 0xFFFF_FFFFu32 - irq::lapic_reg_read(va, TIMER_CURRENT_COUNT);
    // The window above is ~1/100 s regardless of the requested hz, so scale
    // to the period the caller actually wants.
    let ticks_per_period = ((lapic_per_window as u64 * 100) / hz as u64) as u32;

    irq::lapic_reg_write(va, TIMER_DIVIDE_CONFIG, DIVIDE_BY_16);
    irq::lapic_reg_write(va, TIMER_INITIAL_COUNT, ticks_per_period.max(1));
    irq::lapic_reg_write(va, LVT_TIMER, LVT_PERIODIC | TIMER_VECTOR as u32);
}
