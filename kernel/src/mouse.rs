//! PS/2 mouse -- the second input event source (Design/mouse_input.md).
//!
//! Owns the i8042's second port: bring it up alongside the keyboard's port-1
//! init (`keyboard::init` already disables both ports and flushes stale data
//! before this runs), take IRQ12, assemble each 3-byte packet, and push one
//! packed `EVENT_MOUSE_MOVE` event per packet into `input::record` (S1: one
//! event per packet, not one per axis, so a CQ-full drop cannot desync a
//! surviving dx/dy from a dropped button sample). Like the keyboard, the
//! handler never switches processes (input.md D7); a blocked reader wakes at
//! the next scheduler tick.
//!
//! A missing mouse (no port-2 device, e.g. a stripped-down machine type) must
//! not hang boot: `init` treats an ACK timeout as "no mouse," logs it, and
//! leaves IRQ12 masked (S4) -- the same no-silent-hang discipline
//! `keyboard.rs`'s `POLL_MAX` already follows.

use spin::Mutex;
use x86_64::instructions::port::Port;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};

use crate::bkl;
use crate::input::{self, Event};
use crate::irq;

/// IRQ12 is delivered at this vector once `irq::init` remaps the PIC (or
/// programs the I/O APIC) -- the slave-PIC's line 4, already cascaded via
/// IRQ2 (`irq.rs::unmask` handles the cascade for any line >= 8).
const MOUSE_VECTOR: usize = irq::VECTOR_BASE as usize + 12;

// i8042 ports -- the same controller `keyboard.rs` owns port 1 of.
const PS2_DATA: u16 = 0x60;
const PS2_STATUS: u16 = 0x64;
const PS2_CMD: u16 = 0x64;

const STATUS_OUTPUT_FULL: u8 = 1 << 0;
const STATUS_INPUT_FULL: u8 = 1 << 1;

const CMD_READ_CONFIG: u8 = 0x20;
const CMD_WRITE_CONFIG: u8 = 0x60;
const CMD_ENABLE_PORT2: u8 = 0xA8;
/// Prefix: the next data-port write is forwarded to port 2 (the mouse)
/// instead of port 1 (the keyboard).
const CMD_WRITE_PORT2: u8 = 0xD4;

const CFG_PORT2_IRQ: u8 = 1 << 1; // generate IRQ12 on port-2 data
const CFG_PORT2_CLOCK_DISABLE: u8 = 1 << 5; // 1 = port-2 clock disabled

// Standard PS/2 mouse device commands/responses.
const MOUSE_RESET: u8 = 0xFF;
const MOUSE_ENABLE_REPORTING: u8 = 0xF4;
const MOUSE_ACK: u8 = 0xFA;
const MOUSE_SELF_TEST_PASS: u8 = 0xAA;

/// Bound on the i8042 status-poll loops, mirroring `keyboard.rs::POLL_MAX` --
/// a wedged or absent device must not hang boot.
const POLL_MAX: u32 = 100_000;

/// True once the mouse ACKed bring-up. False (logged, not fatal) if port 2
/// never answers.
static PRESENT: Mutex<bool> = Mutex::new(false);

/// Whether the mouse came up. Exposed for the boot path's log line and for
/// the demo-grant decision (main.rs grants the EventSource only if present).
pub fn present() -> bool {
    *PRESENT.lock()
}

/// Install the IRQ12 vector. Called from `interrupts::init` alongside the
/// keyboard, while the IDT is being built.
pub fn register(idt: &mut InterruptDescriptorTable) {
    idt[MOUSE_VECTOR].set_handler_fn(mouse_interrupt);
}

/// Bring up the i8042's second port and unmask IRQ12. Call once at boot,
/// after `keyboard::init`, interrupts off. A minimal-but-honest init,
/// mirroring `keyboard::init`'s shape: enable the port-2 clock + IRQ12 in the
/// config byte, reset the mouse and wait for its self-test pass, then enable
/// data reporting. Leaves IRQ12 masked and `PRESENT` false if any step times
/// out -- a missing mouse is not a boot fault.
pub fn init() {
    // SAFETY: the fixed i8042 ports, programmed once at boot with IF off.
    // Each command/data write waits for the input buffer to drain first;
    // each read waits for the output buffer to fill, both bounded.
    unsafe {
        write_command(CMD_ENABLE_PORT2);

        write_command(CMD_READ_CONFIG);
        let mut cfg = read_data_blocking();
        cfg |= CFG_PORT2_IRQ;
        cfg &= !CFG_PORT2_CLOCK_DISABLE;
        write_command(CMD_WRITE_CONFIG);
        write_data(cfg);

        if !mouse_command(MOUSE_RESET) {
            return; // no ACK -- no mouse on port 2.
        }
        if read_data_blocking() != MOUSE_SELF_TEST_PASS {
            return; // ACKed but failed self-test -- treat as absent.
        }
        // RESET's response is 3 bytes (ACK, self-test pass, device ID -- 0x00
        // for a standard 3-byte mouse); discard the device ID. Left unread,
        // it would sit in the output buffer and be misread as the ACK for
        // the next command (ENABLE_REPORTING), making a present mouse look
        // absent.
        let _device_id = read_data_blocking();
        if !mouse_command(MOUSE_ENABLE_REPORTING) {
            return;
        }
    }
    *PRESENT.lock() = true;
    irq::unmask(12);
}

/// Send a command byte to the mouse through the 0xD4 port-2 prefix and wait
/// for its ACK. Returns false on timeout (treated as "no mouse").
unsafe fn mouse_command(cmd: u8) -> bool {
    write_command(CMD_WRITE_PORT2);
    write_data(cmd);
    read_data_blocking() == MOUSE_ACK
}

/// One PS/2 mouse packet, assembled one byte per IRQ12. Byte 0's bit 3 is
/// always 1 on the wire -- a cheap resync check: a byte seen at packet
/// position 0 without it is dropped rather than accepted as the start of a
/// new packet, so a framing slip (e.g. an IRQ missed during a brief BKL
/// contention spike) resynchronizes on the next genuine packet rather than
/// decoding garbage.
pub(crate) struct Packet {
    bytes: [u8; 3],
    pos: usize,
}

impl Packet {
    pub(crate) const fn new() -> Packet {
        Packet { bytes: [0; 3], pos: 0 }
    }

    /// Feed one byte. Returns the decoded `(dx, dy, buttons)` once a full
    /// packet is assembled, `None` while still accumulating (or resyncing).
    pub(crate) fn push(&mut self, byte: u8) -> Option<(i8, i8, u8)> {
        if self.pos == 0 && byte & 0x08 == 0 {
            return None; // not a valid byte 0 -- drop, stay at pos 0.
        }
        self.bytes[self.pos] = byte;
        self.pos += 1;
        if self.pos < 3 {
            return None;
        }
        self.pos = 0;
        let flags = self.bytes[0];
        let buttons = flags & 0x07;
        let dx = decode_axis(self.bytes[1], flags & 0x10 != 0);
        let dy = decode_axis(self.bytes[2], flags & 0x20 != 0);
        Some((dx, dy, buttons))
    }
}

/// Decode one PS/2 axis from its magnitude byte and 9th (sign) bit. The true
/// value is a 9-bit two's complement (-256..255); clamped to i8 (-128..127)
/// per mouse_input.md S1 -- the packed `Event` has no room for more, and a
/// single packet's delta exceeding +-127 counts is not reached at normal
/// tracking rates. The 9-bit overflow flag (byte 0 bits 6/7) is deliberately
/// not consulted: clamping already bounds the result, so the extra precision
/// it would report is not worth a fourth bit-test here.
pub(crate) fn decode_axis(magnitude: u8, sign: bool) -> i8 {
    let v: i16 = if sign { magnitude as i16 - 256 } else { magnitude as i16 };
    v.clamp(i8::MIN as i16, i8::MAX as i16) as i8
}

static PACKET: Mutex<Packet> = Mutex::new(Packet::new());

/// IRQ12 handler: read one byte, feed the packet assembler, and record a
/// decoded packet via `input::record` (a no-op if nothing has subscribed to
/// `SOURCE_MOUSE` yet -- the same "no subscriber drops" behaviour the
/// keyboard path already has). BKL (D4): acquired/released around the body --
/// `input::record` can call `scheduler::wake_with`, which touches the
/// scheduler table.
extern "x86-interrupt" fn mouse_interrupt(_frame: InterruptStackFrame) {
    bkl::acquire();
    // SAFETY: reached only on IRQ12 with IF=0; reading the i8042 data port
    // consumes the pending byte (and lets the controller deliver the next).
    let byte = unsafe { Port::<u8>::new(PS2_DATA).read() };
    if let Some((dx, dy, buttons)) = PACKET.lock().push(byte) {
        input::record(input::SOURCE_MOUSE, Event::mouse_move(dx, dy, buttons));
    }
    irq::eoi(12);
    unsafe { bkl::release() };
}

// --- bounded i8042 access helpers (identical discipline to keyboard.rs) -----

unsafe fn status() -> u8 {
    Port::<u8>::new(PS2_STATUS).read()
}

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

unsafe fn read_data_blocking() -> u8 {
    let mut spins = 0u32;
    while status() & STATUS_OUTPUT_FULL == 0 && spins < POLL_MAX {
        spins += 1;
        core::hint::spin_loop();
    }
    Port::<u8>::new(PS2_DATA).read()
}
