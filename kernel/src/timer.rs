//! PIT (8254) periodic timer -- the interrupt that drives preemptive
//! scheduling.
//!
//! This module owns the timer *device*: program the PIT for a periodic IRQ0
//! and count ticks. Every interrupt-*controller* operation (the remap, the
//! unmask, the EOI) goes through the `irq` seam, so this module is unaware of
//! whether a PIC or an APIC delivers IRQ0. The interrupt *handler* and the
//! context switch it drives live in `scheduler.rs` (which installs the vector
//! and calls `note_tick` + `irq::eoi` from its `timer_tick`).
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

/// IRQ0 is delivered at this vector once `irq::init` remaps the PIC off the CPU
/// exception range. `scheduler::register` installs the handler here.
pub const TIMER_VECTOR: usize = irq::VECTOR_BASE as usize; // IRQ0 -> VECTOR_BASE + 0

// 8254 PIT: channel-0 data and command ports, and the channel input clock.
const PIT_CH0: u16 = 0x40;
const PIT_CMD: u16 = 0x43;
const PIT_HZ: u32 = 1_193_182;

/// Ticks since the timer was armed. The scheduler bumps it once per IRQ0 and
/// the boot path prints it as proof the timer fired.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Program the PIT for a periodic `hz`-Hz IRQ0 and unmask IRQ0. The interrupt
/// controller must already be initialised (`irq::init`). Call once at boot,
/// interrupts off.
pub fn arm(hz: u32) {
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
