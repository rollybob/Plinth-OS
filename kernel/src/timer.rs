//! Legacy PIC (8259) + PIT (8254) timer device -- the periodic interrupt that
//! drives preemptive scheduling.
//!
//! This module owns the hardware: remap the PIC off the exception vectors,
//! program the PIT for a periodic IRQ0, count ticks, and acknowledge the PIC.
//! The interrupt *handler* and the context switch it drives live in
//! `scheduler.rs` (which installs the vector and calls `note_tick` + `eoi`
//! from its `timer_tick`).
//!
//! Interrupt discipline (the basis for a non-preemptible kernel): ring 3
//! runs with interrupts enabled (usermode.rs sets IF in the user RFLAGS),
//! so the timer fires while a user process is on the CPU. Kernel code always
//! runs with interrupts disabled -- syscalls mask IF via SFMask, and the
//! handler is reached through an interrupt gate (which clears IF on entry) --
//! so the kernel is never reentered by the timer and holds no lock across a
//! tick.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::instructions::port::Port;

/// IRQ0 is delivered at this vector once the PIC is remapped above the CPU
/// exception range (which occupies 0..32). Must be >= 32. `scheduler::register`
/// installs the handler here.
pub const TIMER_VECTOR: usize = 0x20;

// 8259 PIC command/data ports, and the end-of-interrupt command.
const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;
const PIC_EOI: u8 = 0x20;

// 8254 PIT: channel-0 data and command ports, and the channel input clock.
const PIT_CH0: u16 = 0x40;
const PIT_CMD: u16 = 0x43;
const PIT_HZ: u32 = 1_193_182;

/// Ticks since the timer was armed. The scheduler bumps it once per IRQ0 and
/// the boot path prints it as proof the timer fired.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Remap the PIC off the exception vectors, program the PIT for a periodic
/// `hz`-Hz IRQ0, and unmask IRQ0 (only). Call once at boot, interrupts off.
pub fn arm(hz: u32) {
    // SAFETY: single-threaded boot; these are the fixed legacy PIC/PIT
    // ports, programmed exactly once before any interrupt is enabled.
    unsafe {
        remap_pic();
        program_pit(hz);
        // Unmask IRQ0 on the master; mask every other line (and the whole
        // slave) -- the timer is the only interrupt source Plinth wants.
        Port::<u8>::new(PIC1_DATA).write(0xFE);
        Port::<u8>::new(PIC2_DATA).write(0xFF);
    }
}

/// Ticks elapsed since `arm`.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Record one timer tick. Called once per IRQ0 from `scheduler::timer_tick`.
pub fn note_tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Acknowledge the interrupt at the master PIC so the next one is delivered.
/// Called from `scheduler::timer_tick` after the tick is accounted for.
pub fn eoi() {
    // SAFETY: PIC1_CMD is the fixed master-PIC command port; writing the EOI
    // command has no effect other than ending the in-service interrupt.
    unsafe {
        Port::<u8>::new(PIC1_CMD).write(PIC_EOI);
    }
}

/// ICW1-4: remap master to 0x20..0x27, slave to 0x28..0x2F, 8086 mode.
unsafe fn remap_pic() {
    let mut c1 = Port::<u8>::new(PIC1_CMD);
    let mut d1 = Port::<u8>::new(PIC1_DATA);
    let mut c2 = Port::<u8>::new(PIC2_CMD);
    let mut d2 = Port::<u8>::new(PIC2_DATA);

    c1.write(0x11); io_wait(); // ICW1: begin init, ICW4 to follow
    c2.write(0x11); io_wait();
    d1.write(0x20); io_wait(); // ICW2: master vector offset 0x20
    d2.write(0x28); io_wait(); // ICW2: slave vector offset 0x28
    d1.write(0x04); io_wait(); // ICW3: slave is wired to master IRQ2
    d2.write(0x02); io_wait(); // ICW3: slave cascade identity
    d1.write(0x01); io_wait(); // ICW4: 8086 mode
    d2.write(0x01); io_wait();
}

/// Program channel 0 as a rate generator (mode 2) with the 16-bit divisor
/// that yields `hz` interrupts per second.
unsafe fn program_pit(hz: u32) {
    let divisor = (PIT_HZ / hz) as u16;
    let mut cmd = Port::<u8>::new(PIT_CMD);
    let mut ch0 = Port::<u8>::new(PIT_CH0);
    cmd.write(0x34); // channel 0, lobyte/hibyte access, mode 2, binary
    ch0.write((divisor & 0xFF) as u8);
    ch0.write((divisor >> 8) as u8);
}

/// A brief settling delay between PIC command bytes, done by writing an
/// unused port. Real 8259s need it between ICW writes; harmless on QEMU.
unsafe fn io_wait() {
    Port::<u8>::new(0x80).write(0u8);
}
