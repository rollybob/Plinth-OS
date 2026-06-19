//! Interrupt-controller seam (the legacy 8259 PIC today).
//!
//! Every line-IRQ touchpoint that is specific to the interrupt controller --
//! remapping it off the CPU exception vectors, masking/unmasking a line, and
//! sending end-of-interrupt -- lives here and nowhere else. Devices (the PIT
//! timer, the i8042 keyboard) drive their own device registers but route every
//! controller operation through this module, so a future move to APIC
//! (Local APIC + I/O APIC, needed for SMP and MSI) reimplements just these four
//! operations over the IOAPIC + LAPIC and nothing above this module changes.
//! See Design/input.md section 4 (interrupt-controller portability).
//!
//! Discipline: the PIC is remapped and fully masked at boot (interrupts off);
//! each device unmasks its own line as it comes up. EOI is per-line so a future
//! slave-PIC line (the PS/2 mouse on IRQ12) acks both PICs.

use x86_64::instructions::port::Port;

// 8259 master/slave command + data ports, and the end-of-interrupt command.
const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;
const PIC_EOI: u8 = 0x20;

/// Vector base the master PIC is remapped to: IRQ `n` (0..8) is delivered at
/// `VECTOR_BASE + n`, the slave (8..16) just above. Must be >= 32 -- the CPU
/// exception range is 0..32. The IDT installs handlers at these vectors.
pub const VECTOR_BASE: u8 = 0x20;

/// Remap both PICs off the exception vectors and mask every line. Call once at
/// boot, interrupts off, before any device unmasks its line.
pub fn init() {
    // SAFETY: single-threaded boot; the fixed legacy PIC ports, programmed
    // exactly once before any interrupt is enabled.
    unsafe {
        remap();
        // Mask everything; each device unmasks its own line in its init.
        Port::<u8>::new(PIC1_DATA).write(0xFF);
        Port::<u8>::new(PIC2_DATA).write(0xFF);
    }
}

/// Unmask IRQ `line` (0..16) so the controller delivers it. A line on the slave
/// PIC (>= 8) also needs the cascade line (IRQ2) unmasked on the master.
pub fn unmask(line: u8) {
    set_mask(line, false);
    if line >= 8 {
        set_mask(2, false); // cascade to the slave
    }
}

/// Mask IRQ `line` so the controller stops delivering it.
#[allow(dead_code)] // the symmetric op; used once the mouse line can be disabled
pub fn mask(line: u8) {
    set_mask(line, true);
}

/// Acknowledge IRQ `line`: EOI the master, and the slave too for a line >= 8.
pub fn eoi(line: u8) {
    // SAFETY: the fixed PIC command ports; an EOI only ends the in-service IRQ.
    unsafe {
        if line >= 8 {
            Port::<u8>::new(PIC2_CMD).write(PIC_EOI);
        }
        Port::<u8>::new(PIC1_CMD).write(PIC_EOI);
    }
}

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
