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

use core::sync::atomic::{AtomicU32, Ordering};

use spin::Mutex;

use crate::capability::{CapObject, RIGHT_READ};
use crate::process;
use crate::scheduler::{self, TrapFrame, GP_RSI};

/// `event_recv` status, returned in rax (the status/payload split: the event
/// rides in rsi, so no event value can be mistaken for an error). `EVENT_OK`
/// means an event was delivered; `EVENT_ERR` a bad slot, wrong cap kind, or a
/// missing read right.
pub const EVENT_OK: u64 = 0;
pub const EVENT_ERR: u64 = 1;

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

    /// Pack into one word (the status/payload split puts the event in a register
    /// for `event_recv`): kind in bits 0..8, code in 8..24, value in 24..32.
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

/// One event source: its ring, plus the single process slot blocked waiting for
/// an event on it (one waiter per source, v1 -- the input library OS is the one
/// reader; a multi-reader fan-out is itself a libOS over this). The producer and
/// consumer never run concurrently (IF=0 in both the IRQ handler and the reader
/// dispatch), so the per-source lock never contends.
struct Source {
    ring: EventRing,
    waiter: Option<usize>,
}

impl Source {
    const fn new() -> Source {
        Source { ring: EventRing::new(), waiter: None }
    }
}

// One source per event device. `[CONST; N]` builds a non-Copy array element in
// a static (mirrors virtio_blk's DEVICES).
const NEW_SOURCE: Mutex<Source> = Mutex::new(Source::new());
static SOURCES: [Mutex<Source>; N_SOURCES] = [NEW_SOURCE; N_SOURCES];

/// Push an event onto `source`'s ring -- the producer half, called from a
/// device IRQ handler. If a reader is blocked on this source, deliver the event
/// and wake it (the wake half of the input path: an IRQ unblocks a waiter, the
/// same primitive a future interrupt-driven `block_read` would reuse). Out-of-
/// range sources are ignored (never panics in an interrupt handler).
pub fn record(source: usize, ev: Event) {
    let Some(slot) = SOURCES.get(source) else {
        return;
    };
    let woken = {
        let mut s = slot.lock();
        s.ring.push(ev);
        // A blocked reader took the ring empty, so the event it gets is the one
        // just pushed; hand it over and clear the waiter.
        s.waiter.take().and_then(|w| s.ring.pop().map(|e| (w, e)))
    };
    if let Some((waiter, e)) = woken {
        scheduler::wake_with(waiter, EVENT_OK, e.pack(), u64::MAX);
    }
}

/// Pop the next event from `source`'s ring, or `None` if empty -- the
/// non-blocking drain (the boot-path selftest uses it; the blocking reader is
/// `event_recv`).
pub fn poll(source: usize) -> Option<Event> {
    SOURCES.get(source).and_then(|s| s.lock().ring.pop())
}

/// True if any source has a process blocked waiting for an event. The scheduler
/// reads this to tell "waiting for external input" (a legitimate idle -- a
/// keystroke can still arrive) apart from an IPC deadlock (no peer can ever
/// come): the former idles, the latter panics.
pub fn any_waiter() -> bool {
    SOURCES.iter().any(|s| s.lock().waiter.is_some())
}

/// The blocking event read, reached from the `int 0x80` dispatch (it needs a
/// resumable trap frame, like the IPC ops -- not the `syscall` fast path).
/// Returns the next event from the source named by the `EventSource` capability
/// at `source_slot`: an event already queued is returned immediately (rax =
/// `EVENT_OK`, the packed event in rsi); an empty source blocks this process
/// (recorded as the source's waiter) until `record` wakes it with an event.
pub fn event_recv(source_slot: u64, frame_ptr: u64) -> u64 {
    let Some(source) = source_for(source_slot) else {
        return EVENT_ERR;
    };
    let Some(slot) = SOURCES.get(source) else {
        return EVENT_ERR;
    };
    {
        // Pop-or-register-waiter under one lock: with IF=0 no IRQ interleaves,
        // so this is atomic against the producer -- a wake can never be lost
        // between "found empty" and "blocked" (the same guarantee the IPC
        // block-time check relies on).
        let mut s = slot.lock();
        if let Some(ev) = s.ring.pop() {
            write_rsi(frame_ptr, ev.pack());
            return EVENT_OK;
        }
        s.waiter = Some(scheduler::current_slot());
    } // drop the lock BEFORE blocking -- block_current never returns here.
    scheduler::block_current(frame_ptr)
}

/// Resolve `slot` in the current process's table to a live event-source id,
/// requiring `RIGHT_READ`. Takes and releases the CURRENT lock here so none is
/// held across a later block. A non-`EventSource` cap (or one without the read
/// right, or naming an unknown source) yields None -- the multiplexing gate.
fn source_for(slot: u64) -> Option<usize> {
    let guard = process::CURRENT.lock();
    let cap = guard.as_ref()?.caps.lookup(slot as usize, RIGHT_READ).ok()?;
    match cap.object {
        CapObject::EventSource { id } if (id as usize) < N_SOURCES => Some(id as usize),
        _ => None,
    }
}

/// Write `val` into the rsi slot of the trap frame at `frame_ptr`, so a
/// fast-path (non-blocking) `event_recv` returns the event there (the stub
/// restores rsi on iretq). Mirrors the IPC payload-in-rsi convention.
fn write_rsi(frame_ptr: u64, val: u64) {
    // SAFETY: frame_ptr is this call's trap frame on the current process's
    // kernel stack; valid for this call.
    unsafe {
        (*(frame_ptr as *mut TrapFrame)).gp[GP_RSI] = val;
    }
}

// --- synthetic injection (test scaffold for the Stage-2 delivery demo) -------
//
// A one-shot synthetic event lets a process blocked on `event_recv` be woken
// deterministically in headless smoke, without a real keypress. The boot path
// arms it; the scheduler delivers it when it would otherwise idle on a blocked
// reader (driving the exact record -> wake path a real IRQ would). Real
// keystrokes replace this in Stage 3; an armed value of 0 means disarmed.

/// Armed synthetic scancode, with bit 8 as the armed flag (so scancode 0 is
/// representable). 0 = disarmed.
static SYNTHETIC: AtomicU32 = AtomicU32::new(0);
const SYNTHETIC_ARMED: u32 = 0x100;

/// Arm a one-shot synthetic keyboard event (for the Stage-2 delivery demo).
pub fn arm_synthetic(scancode: u8) {
    SYNTHETIC.store(SYNTHETIC_ARMED | scancode as u32, Ordering::Relaxed);
}

/// If a synthetic event is armed, deliver it once through `record` (waking any
/// reader blocked on the keyboard) and disarm. Called from the scheduler's
/// idle-on-input path. A no-op when nothing is armed (the real-keystroke case).
pub fn deliver_synthetic() {
    let v = SYNTHETIC.swap(0, Ordering::Relaxed);
    if v & SYNTHETIC_ARMED != 0 {
        record(SOURCE_KEYBOARD, Event::key((v & 0xFF) as u8));
    }
}
