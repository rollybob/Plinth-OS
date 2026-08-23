//! COM1 serial output. All kernel diagnostics go to the UART; QEMU routes
//! it to stdout via `-serial stdio`.
//!
//! `probe`/`try_init` detect whether a 16550 actually decodes COM1 before the
//! kernel commits to it as its only output device (D11 step 2). The existing
//! callers still reach the port through `init`, which assumes a UART is
//! present; routing the 12 call sites through a console that honours a `None`
//! probe is D11 step 3 and is deliberately not done here.

use uart_16550::SerialPort;
use x86_64::instructions::port::Port;

const COM1: u16 = 0x3F8;

/// Offset of the 16550 scratch register from the port base. It carries no
/// device function beyond holding a byte -- which is exactly what makes it the
/// safe register to test whether anything decodes the port at all.
const SCRATCH: u16 = 7;

/// Two sentinels, not one: a floating or stuck-high bus can echo a single
/// written byte by accident, but not two independent patterns in a row.
const SENTINELS: [u8; 2] = [0x55, 0xAA];

/// True iff a 16550 decodes the register block at `base`, tested by writing
/// each sentinel to the scratch register and reading it back. A port with no
/// device reads back open-bus 0xFF and fails the round-trip.
///
/// `pub(crate)` rather than private only so the test harness can aim it at a
/// base this machine does not wire and confirm the probe reports absence --
/// see `tests::serial_probe`.
pub(crate) fn probe_at(base: u16) -> bool {
    // SAFETY: byte I/O on the scratch register (base+7) of a 16550 register
    // block. The scratch register has no device function, so writing it cannot
    // change UART state or emit a byte; on a base with no device the accesses
    // are inert. `base` is only ever a standard COM I/O base.
    unsafe {
        let mut scratch: Port<u8> = Port::new(base + SCRATCH);
        for &s in &SENTINELS {
            scratch.write(s);
            if scratch.read() != s {
                return false;
            }
        }
        true
    }
}

/// True iff a UART is present at COM1.
pub fn probe() -> bool {
    probe_at(COM1)
}

/// Initialise COM1 and return the port handle.
///
/// Assumes a UART is present -- the pre-D11 contract, kept intact so the 12
/// existing callers compile unchanged. D11 step 3 replaces these calls with a
/// console that selects its backend from `try_init`.
pub fn init() -> SerialPort {
    init_com1()
}

fn init_com1() -> SerialPort {
    // SAFETY: COM1 at 0x3F8 is the standard first serial port on x86.
    // Callers each get an independent handle to the same device; writes
    // may interleave but never fault.
    let mut port = unsafe { SerialPort::new(COM1) };
    port.init();
    port
}
