//! Asynchronous input events: the per-source ring buffers a device IRQ pushes
//! into and a reader drains.
//!
//! This is the kernel half of the input path (Design/input.md). A device (the
//! i8042 keyboard, later a mouse) is an *event source*; each source owns a
//! bounded ring of raw `Event` records. The device's IRQ handler is the single
//! producer (`record`); a reader is the single consumer (`poll`, and in a later
//! stage the blocking `event_recv`). Producer and consumer never run
//! concurrently -- both touch the ring with IF=0 (the IRQ handler is reached
//! through an interrupt gate; the reader runs in the IF=0 kernel) -- so the
//! per-source lock never contends; it is here for clarity, not arbitration.
//!
//! Events are raw: a keyboard event carries the scancode byte, not a character.
//! All interpretation (keymaps, make/break beyond the Set-1 bit, extended `0xE0`
//! sequences, line editing) is library-OS policy. The kernel multiplexes the
//! device and ships bytes; it does not own a keymap (D3).

use spin::Mutex;

/// Event kinds. Only keyboard exists today; the tag is here so a mouse (move,
/// button) rides the same record and ring without a kernel-surface change.
pub const EVENT_KEY: u8 = 1;

/// One raw input event. Fixed-size and `Copy` so a ring is a plain array and
/// the record packs into a single register for `event_recv` later.
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
    /// The zero event, for const ring initialisation.
    pub const EMPTY: Event = Event { kind: 0, code: 0, value: 0 };

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

    /// Pack into one word (status/payload split puts this in a register for the
    /// later `event_recv`): kind in bits 0..8, code in 8..24, value in 24..32.
    // event_recv (Stage 2) is its boot-path consumer; exercised now by the tests.
    #[allow(dead_code)]
    pub const fn pack(self) -> u64 {
        (self.kind as u64) | ((self.code as u64) << 8) | ((self.value as u64) << 24)
    }
}

/// Ring capacity per source. A small power of two -- far more than a human
/// typist outruns between scheduler ticks; one frame would be wild overkill.
pub const RING_CAP: usize = 32;

/// A bounded single-producer / single-consumer event ring. On overflow it drops
/// the *newest* event (the in-flight prefix the reader already holds stays
/// coherent) and bumps a sticky `dropped` count the reader can observe (D5).
/// Pure data -- no device, no lock -- so it is unit-tested directly.
pub struct EventRing {
    buf: [Event; RING_CAP],
    head: usize, // next to pop
    len: usize,
    dropped: u32,
}

impl EventRing {
    pub const fn new() -> EventRing {
        EventRing { buf: [Event::EMPTY; RING_CAP], head: 0, len: 0, dropped: 0 }
    }

    /// Enqueue `ev`, or drop it (and count the drop) if the ring is full.
    pub fn push(&mut self, ev: Event) {
        if self.len == RING_CAP {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        let tail = (self.head + self.len) % RING_CAP;
        self.buf[tail] = ev;
        self.len += 1;
    }

    /// Dequeue the oldest event, or `None` if empty.
    pub fn pop(&mut self) -> Option<Event> {
        if self.len == 0 {
            return None;
        }
        let ev = self.buf[self.head];
        self.head = (self.head + 1) % RING_CAP;
        self.len -= 1;
        Some(ev)
    }

    // The following are exercised by the ring tests; their boot-path consumer is
    // the reader (`event_recv`, Stage 2), which observes drops and queue state.

    /// Read and clear the dropped-event count since the last call.
    #[allow(dead_code)]
    pub fn take_dropped(&mut self) -> u32 {
        core::mem::take(&mut self.dropped)
    }

    /// Events currently queued.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Well-known event sources. The keyboard is source 0; a mouse would be 1.
pub const SOURCE_KEYBOARD: usize = 0;
const N_SOURCES: usize = 1;

// One ring per source. `[CONST; N]` is how a non-Copy array element is built in
// a static (mirrors virtio_blk's DEVICES).
const NEW_RING: Mutex<EventRing> = Mutex::new(EventRing::new());
static SOURCES: [Mutex<EventRing>; N_SOURCES] = [NEW_RING; N_SOURCES];

/// Push an event onto `source`'s ring -- the producer half, called from a
/// device IRQ handler. Out-of-range sources are ignored (never panics in an
/// interrupt handler).
pub fn record(source: usize, ev: Event) {
    if let Some(ring) = SOURCES.get(source) {
        ring.lock().push(ev);
    }
}

/// Pop the next event from `source`'s ring, or `None` if empty -- the consumer
/// half. The blocking reader (`event_recv`) arrives in the next stage; this is
/// the non-blocking drain the boot-path proof uses.
pub fn poll(source: usize) -> Option<Event> {
    SOURCES.get(source).and_then(|ring| ring.lock().pop())
}
