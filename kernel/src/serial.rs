//! COM1 serial output. All kernel diagnostics go to the UART; QEMU routes
//! it to stdout via `-serial stdio`.

use uart_16550::SerialPort;

const COM1: u16 = 0x3F8;

/// Initialise COM1 and return the port handle.
pub fn init() -> SerialPort {
    // SAFETY: COM1 at 0x3F8 is the standard first serial port on x86.
    // Callers each get an independent handle to the same device; writes
    // may interleave but never fault.
    let mut port = unsafe { SerialPort::new(COM1) };
    port.init();
    port
}
