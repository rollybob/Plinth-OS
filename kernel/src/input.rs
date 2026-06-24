//! Input event sources: the producer half of the input path (Design/input.md,
//! Design/event_rings.md).
//!
//! A device (the i8042 keyboard, later a mouse) is an *event source*. Its IRQ
//! handler is the single producer: it calls `record`, which routes the raw
//! `Event` to every ring subscribed to that source (the multishot event-ring
//! path, `rings::deliver_event`) -- posting a completion into each subscriber's
//! CQ and waking an owner parked in `ring_wait`. There is no per-source staging
//! buffer any more (event_rings.md S6): the subscriber's CQ is the buffer, and a
//! source with no subscriber drops the event. Producer and consumer never run
//! concurrently -- the IRQ handler and the kernel reader both run IF=0 under the
//! BKL -- so the routing needs no further locking.
//!
//! Events are raw: a keyboard event carries the scancode byte, not a character.
//! All interpretation (keymaps, make/break beyond the Set-1 bit, extended `0xE0`
//! sequences, line editing) is library-OS policy. The kernel multiplexes the
//! device and ships bytes; it does not own a keymap (D3).

use spin::Mutex;

use crate::capability::{CapObject, RIGHT_READ};
use crate::process;

/// Event kinds.
pub const EVENT_KEY: u8 = 1;
/// A mouse motion+button sample (Design/mouse_input.md S1): one packed event
/// per PS/2 packet, not split per axis, so the CQ-full drop-newest policy
/// (event_rings.md s5) drops a whole packet atomically rather than risking a
/// desynced dx/dy/button triplet with no boundary tag to detect it.
pub const EVENT_MOUSE_MOVE: u8 = 2;

/// One raw input event. Fixed-size and `Copy`; packs into the low 32 bits of a
/// CQ completion's `status` field (event_rings.md s4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    /// Event kind (`EVENT_KEY`, ...).
    pub kind: u8,
    /// Device code: for a key, the raw scancode byte (Set 1).
    pub code: u16,
    /// For a key, the Set-1 make/break convenience bit (1 = press, 0 = release);
    /// reserved for other kinds. Interpretation still belongs to the libOS.
    pub value: u8,
}

impl Event {
    /// A keyboard event from a raw Set-1 scancode byte. The high bit is the
    /// make/break flag in Set 1; we surface it in `value` as a convenience but
    /// leave `code` the raw byte, so the libOS sees exactly what the wire sent.
    pub const fn key(scancode: u8) -> Event {
        Event {
            kind: EVENT_KEY,
            code: scancode as u16,
            value: if scancode & 0x80 == 0 { 1 } else { 0 },
        }
    }

    /// A mouse motion+button event (mouse_input.md S1). `dx`/`dy` are one
    /// PS/2 packet's signed-byte deltas, packed into `code` (dx in the high
    /// byte, dy in the low byte); `buttons` is the current bitmask (bit 0/1/2
    /// = left/right/middle), masked to 7 bits so it never collides with the
    /// CQ's `EVENT_DROPPED` flag at bit 31 (`value`'s bit 7, rings.rs).
    pub const fn mouse_move(dx: i8, dy: i8, buttons: u8) -> Event {
        Event {
            kind: EVENT_MOUSE_MOVE,
            code: ((dx as u8 as u16) << 8) | (dy as u8 as u16),
            value: buttons & 0x7F,
        }
    }

    /// Pack into the low 32 bits of a word: kind in bits 0..8, code in 8..24,
    /// value in 24..32. The kind byte is always nonzero (`EVENT_KEY` = 1,
    /// `EVENT_MOUSE_MOVE` = 2), which the event-ring shim relies on to tell a
    /// real event from a zero subscribe-error status (event_rings.md s5).
    pub const fn pack(self) -> u64 {
        (self.kind as u64) | ((self.code as u64) << 8) | ((self.value as u64) << 24)
    }
}

/// Well-known event sources (Design/input.md s7, mouse_input.md).
pub const SOURCE_KEYBOARD: usize = 0;
pub const SOURCE_MOUSE: usize = 1;
const N_SOURCES: usize = 2;

/// Record an event from `source` -- the producer half, called from a device IRQ
/// handler (and the synthetic scaffold). Routes the event to every ring
/// subscribed to this source, posting it into each CQ and waking a parked owner
/// (`rings::deliver_event`); a source with no subscriber drops the event.
/// Out-of-range sources route to nothing (never panics in an interrupt handler).
pub fn record(source: usize, ev: Event) {
    crate::rings::deliver_event(source, ev.pack() as u32);
}

/// True if any ring holds a live event subscription -- someone a keystroke could
/// wake. The scheduler reads this to treat a process parked in `ring_wait` on an
/// event subscription as a legitimate idle (a key can still arrive), not an IPC
/// deadlock. The block-I/O equivalent is `virtio_blk::any_waiter`.
pub fn any_waiter() -> bool {
    crate::rings::any_event_sub()
}

/// Resolve `slot` in the current process's table to a live event-source id,
/// requiring `RIGHT_READ` -- the multiplexing gate. A non-`EventSource` cap (or
/// one without the read right, or naming an unknown source) yields None. Called
/// from the `RING_OP_EVENT_SUB` drain (rings.rs) to gate a subscription. Takes
/// and releases the CURRENT lock here, so none is held across a later block.
pub(crate) fn source_for(slot: u64) -> Option<usize> {
    let guard = process::current().lock();
    let cap = guard.as_ref()?.caps.lookup(slot as usize, RIGHT_READ).ok()?;
    match cap.object {
        CapObject::EventSource { id } if (id as usize) < N_SOURCES => Some(id as usize),
        _ => None,
    }
}

// --- synthetic injection (test scaffold for the input demos) -----------------
//
// A synthetic scancode SEQUENCE lets a process parked in `ring_wait` on an event
// subscription be driven deterministically in headless smoke, without real
// keypresses. The boot path arms a sequence; the scheduler delivers the next
// scancode each time it would otherwise idle on a blocked reader (driving the
// exact record -> route -> wake path a real IRQ would, one scancode per wake).
// Real keystrokes replace this entirely -- they arrive via the keyboard IRQ
// during the same idle. The sequence is the stimulus a scripted `sendkey` smoke
// would otherwise provide.

const MAX_SYNTHETIC: usize = 16;

struct Synthetic {
    codes: [u8; MAX_SYNTHETIC],
    len: usize,
    pos: usize,
}

impl Synthetic {
    const fn new() -> Synthetic {
        Synthetic { codes: [0; MAX_SYNTHETIC], len: 0, pos: 0 }
    }
}

static SYNTHETIC: Mutex<Synthetic> = Mutex::new(Synthetic::new());

/// Arm a synthetic scancode sequence (capped at MAX_SYNTHETIC). Each
/// `deliver_synthetic` records the next one; once exhausted, delivery is a
/// no-op. Used by the input demos to script a key sequence deterministically.
pub fn arm_synthetic(scancodes: &[u8]) {
    let mut s = SYNTHETIC.lock();
    let n = scancodes.len().min(MAX_SYNTHETIC);
    s.codes[..n].copy_from_slice(&scancodes[..n]);
    s.len = n;
    s.pos = 0;
}

/// Record the next armed synthetic scancode (if any) through `record`, waking a
/// reader parked in `ring_wait` on the keyboard subscription. Called from the
/// scheduler's idle-on-input path. A no-op once the sequence is exhausted (the
/// real-keystroke case, where the keyboard IRQ is the producer instead).
pub fn deliver_synthetic() {
    let next = {
        let mut s = SYNTHETIC.lock();
        if s.pos < s.len {
            let sc = s.codes[s.pos];
            s.pos += 1;
            Some(sc)
        } else {
            None
        }
    };
    if let Some(sc) = next {
        record(SOURCE_KEYBOARD, Event::key(sc));
    }
}

// --- synthetic mouse injection (test scaffold, mouse_input.md S3) -----------
//
// Mirrors the keyboard's synthetic scaffold above: QEMU's HMP monitor has no
// `sendkey`-clean equivalent for relative mouse motion deterministic enough to
// script into the permanent smoke, so a scripted packet sequence drives the
// exact record -> route -> wake path a real IRQ12 packet would.

const MAX_SYNTHETIC_MOUSE: usize = 16;

struct SyntheticMouse {
    packets: [(i8, i8, u8); MAX_SYNTHETIC_MOUSE],
    len: usize,
    pos: usize,
}

impl SyntheticMouse {
    const fn new() -> SyntheticMouse {
        SyntheticMouse { packets: [(0, 0, 0); MAX_SYNTHETIC_MOUSE], len: 0, pos: 0 }
    }
}

static SYNTHETIC_MOUSE: Mutex<SyntheticMouse> = Mutex::new(SyntheticMouse::new());

/// Arm a synthetic `(dx, dy, buttons)` packet sequence (capped at
/// MAX_SYNTHETIC_MOUSE). Each `deliver_synthetic_mouse` records the next one;
/// once exhausted, delivery is a no-op. Used by the mouse demo to script
/// motion deterministically.
pub fn arm_synthetic_mouse(packets: &[(i8, i8, u8)]) {
    let mut s = SYNTHETIC_MOUSE.lock();
    let n = packets.len().min(MAX_SYNTHETIC_MOUSE);
    s.packets[..n].copy_from_slice(&packets[..n]);
    s.len = n;
    s.pos = 0;
}

/// Record the next armed synthetic mouse packet (if any) through `record`,
/// waking a reader parked in `ring_wait` on the mouse subscription. Called
/// from the scheduler's idle-on-input path alongside `deliver_synthetic`. A
/// no-op once the sequence is exhausted (the real-mouse case, where IRQ12 is
/// the producer instead).
pub fn deliver_synthetic_mouse() {
    let next = {
        let mut s = SYNTHETIC_MOUSE.lock();
        if s.pos < s.len {
            let p = s.packets[s.pos];
            s.pos += 1;
            Some(p)
        } else {
            None
        }
    };
    if let Some((dx, dy, buttons)) = next {
        record(SOURCE_MOUSE, Event::mouse_move(dx, dy, buttons));
    }
}
