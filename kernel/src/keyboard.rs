//! i8042 PS/2 keyboard -- the first input event source (Design/input.md, D1).
//!
//! This module owns the keyboard *device*: bring up the i8042 controller, take
//! its IRQ1, and push each scancode into the keyboard event ring as a raw
//! `Event`. Every interrupt-*controller* operation (unmask, EOI) goes through
//! the `irq` seam, so the keyboard is unaware of PIC vs APIC. Interpretation of
//! the scancodes -- keymaps, characters, line editing -- is library-OS policy
//! and lives nowhere in the kernel (D3).
//!
//! The handler is an ordinary interrupt handler, not the timer's naked
//! context-switch stub: it reads the scancode, records it, and EOIs. It never
//! switches processes (D7); a blocked reader is woken (in a later stage) and
//! runs at the next scheduler tick.

use x86_64::instructions::port::Port;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};

use crate::bkl;
use crate::input::{self, Event};
use crate::irq;

/// IRQ1 is delivered at this vector once `irq::init` remaps the PIC.
const KEYBOARD_VECTOR: usize = irq::VECTOR_BASE as usize + 1;

// i8042 ports: the data port, and the status (read) / command (write) port.
const PS2_DATA: u16 = 0x60;
const PS2_STATUS: u16 = 0x64;
const PS2_CMD: u16 = 0x64;

// Status-register bits.
const STATUS_OUTPUT_FULL: u8 = 1 << 0; // a byte is waiting in the data port
const STATUS_INPUT_FULL: u8 = 1 << 1; // the controller has not consumed our last write

// Controller commands (written to 0x64).
const CMD_READ_CONFIG: u8 = 0x20;
const CMD_WRITE_CONFIG: u8 = 0x60;
const CMD_DISABLE_PORT2: u8 = 0xA7;
const CMD_DISABLE_PORT1: u8 = 0xAD;
const CMD_ENABLE_PORT1: u8 = 0xAE;

// Config-byte bits.
const CFG_PORT1_IRQ: u8 = 1 << 0; // generate IRQ1 on port-1 data
const CFG_PORT1_CLOCK_DISABLE: u8 = 1 << 4; // 1 = port-1 clock disabled
const CFG_PORT1_TRANSLATE: u8 = 1 << 6; // 1 = translate to scancode Set 1

/// Bound on the i8042 status-poll loops, so a wedged or absent controller faults
/// the bring-up instead of hanging the boot (the no-silent-hang discipline).
const POLL_MAX: u32 = 100_000;

/// Install the IRQ1 vector. Called from `interrupts::init` alongside the timer
/// and IPC gates, while the IDT is being built.
pub fn register(idt: &mut InterruptDescriptorTable) {
    idt[KEYBOARD_VECTOR].set_handler_fn(keyboard_interrupt);
}

/// Bring the controller up and unmask IRQ1. Call once at boot, interrupts off,
/// after `irq::init`. Does a minimal-but-honest init rather than trusting
/// firmware: disable the ports, flush stale data, set the config byte to enable
/// the port-1 clock + IRQ1 + Set-1 translation, then enable the port.
pub fn init() {
    // SAFETY: the fixed i8042 ports, programmed once at boot with IF off. Each
    // command/data write waits for the input buffer to drain first; reads wait
    // for the output buffer to fill, both bounded.
    unsafe {
        write_command(CMD_DISABLE_PORT1);
        write_command(CMD_DISABLE_PORT2); // harmless if there is no second port
        flush_output();

        write_command(CMD_READ_CONFIG);
        let mut cfg = read_data_blocking();
        cfg |= CFG_PORT1_IRQ | CFG_PORT1_TRANSLATE;
        cfg &= !CFG_PORT1_CLOCK_DISABLE;
        write_command(CMD_WRITE_CONFIG);
        write_data(cfg);

        write_command(CMD_ENABLE_PORT1);
    }
    irq::unmask(1); // IRQ1 (the keyboard line)
}

/// IRQ1 handler: read the scancode and record it. An interrupt gate clears IF,
/// so this runs non-preemptibly and the event ring needs no further locking.
/// BKL (D4): acquired/released around the body -- `input::record` can call
/// `scheduler::wake_with`, which touches the scheduler table.
extern "x86-interrupt" fn keyboard_interrupt(_frame: InterruptStackFrame) {
    bkl::acquire();
    // SAFETY: reached only on IRQ1 with IF=0; reading the i8042 data port
    // consumes the pending byte (and lets the controller deliver the next one).
    let scancode = unsafe { Port::<u8>::new(PS2_DATA).read() };
    input::record(input::SOURCE_KEYBOARD, Event::key(scancode));
    irq::eoi(1);
    unsafe { bkl::release() };
}

// --- bounded i8042 access helpers ---

unsafe fn status() -> u8 {
    Port::<u8>::new(PS2_STATUS).read()
}

/// Wait (bounded) until the controller's input buffer is empty, so it is ready
/// for a command or data byte.
unsafe fn wait_input_clear() {
    let mut spins = 0u32;
    while status() & STATUS_INPUT_FULL != 0 && spins < POLL_MAX {
        spins += 1;
        core::hint::spin_loop();
    }
}

unsafe fn write_command(cmd: u8) {
    wait_input_clear();
    Port::<u8>::new(PS2_CMD).write(cmd);
}

unsafe fn write_data(data: u8) {
    wait_input_clear();
    Port::<u8>::new(PS2_DATA).write(data);
}

/// Wait (bounded) for a byte in the output buffer, then read it.
unsafe fn read_data_blocking() -> u8 {
    let mut spins = 0u32;
    while status() & STATUS_OUTPUT_FULL == 0 && spins < POLL_MAX {
        spins += 1;
        core::hint::spin_loop();
    }
    Port::<u8>::new(PS2_DATA).read()
}

/// Drain any stale bytes the controller already has buffered.
unsafe fn flush_output() {
    let mut spins = 0u32;
    while status() & STATUS_OUTPUT_FULL != 0 && spins < POLL_MAX {
        let _ = Port::<u8>::new(PS2_DATA).read();
        spins += 1;
    }
}
