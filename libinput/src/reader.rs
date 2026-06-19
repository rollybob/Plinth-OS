//! The line reader: assemble a line of input from raw key events, decoding them
//! with a `Keymap`. Bare-target only -- it issues `event_recv` syscalls, so it
//! is excluded from the host `cargo test` build (which exercises the pure
//! keymap), along with the libplinth dependency.

use libplinth::{event_code, event_kind, event_value, sys_event_recv, EVENT_OK};

use crate::keymap::{Key, Keymap};

/// Read a line from the EventSource at `source_slot` into `buf`, returning the
/// number of bytes read (the line, without the terminating Enter). Blocks on
/// each key. Characters past `buf.len()` are dropped; Backspace erases the last.
/// This is libOS policy over the kernel's raw `event_recv` -- no echo, no
/// history; a richer line discipline is a matter of more code here, not in the
/// kernel.
pub fn read_line(source_slot: u64, buf: &mut [u8]) -> usize {
    let mut keymap = Keymap::new();
    let mut len = 0;
    loop {
        let (status, ev) = sys_event_recv(source_slot);
        if status != EVENT_OK {
            continue;
        }
        match keymap.feed(event_kind(ev), event_code(ev), event_value(ev)) {
            Key::Enter => return len,
            Key::Backspace => len = len.saturating_sub(1),
            Key::Char(c) => {
                if len < buf.len() {
                    buf[len] = c;
                    len += 1;
                }
            }
            Key::None => {}
        }
    }
}
