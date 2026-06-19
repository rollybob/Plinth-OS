//! US-layout Set-1 scancode decoding with shift state -- the keymap as
//! library-OS policy. The kernel ships raw scancodes; turning them into
//! characters and line edits lives here, in unprivileged code. Pure (no
//! syscalls), so it is host-unit-tested like libfs's archive parser.

/// Event kind for a key (matches the kernel/libplinth ABI value, kept local so
/// the keymap depends on nothing -- it is host-tested without libplinth).
const EVENT_KEY: u8 = 1;

/// The line-editing meaning of a key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A printable ASCII byte.
    Char(u8),
    /// The Enter/Return key.
    Enter,
    /// Backspace.
    Backspace,
    /// No line-editing effect: a modifier, a key release, or an unmapped key.
    None,
}

// Set-1 make scancodes for the keys we name specially.
const SC_LSHIFT: u8 = 0x2A;
const SC_RSHIFT: u8 = 0x36;
const SC_ENTER: u8 = 0x1C;
const SC_BACKSPACE: u8 = 0x0E;

/// A US Set-1 keymap with shift-modifier tracking. Feed it raw key events; it
/// yields characters and line edits.
pub struct Keymap {
    shift: bool,
}

impl Keymap {
    pub const fn new() -> Keymap {
        Keymap { shift: false }
    }

    /// Decode one raw key event `(kind, code, value)` -- the unpacked fields of
    /// a packed event from `event_recv` -- into a line-editing action. Tracks
    /// shift across calls; ignores key releases (except for shift) and unmapped
    /// keys.
    pub fn feed(&mut self, kind: u8, code: u16, value: u8) -> Key {
        if kind != EVENT_KEY {
            return Key::None;
        }
        let key = (code & 0x7F) as u8; // base scancode (strip the Set-1 break bit)
        let press = value == 1;

        // Shift is the only modifier today; track it on both make and break.
        if key == SC_LSHIFT || key == SC_RSHIFT {
            self.shift = press;
            return Key::None;
        }
        // Releases of ordinary keys have no line-editing effect.
        if !press {
            return Key::None;
        }
        match key {
            SC_ENTER => Key::Enter,
            SC_BACKSPACE => Key::Backspace,
            _ => match decode(key) {
                // Shift uppercases letters; digits/space are unaffected by this
                // minimal map (a full shifted-symbol table is libOS polish).
                Some(ch) if self.shift && ch.is_ascii_lowercase() => Key::Char(ch - 32),
                Some(ch) => Key::Char(ch),
                None => Key::None,
            },
        }
    }
}

/// Unshifted character for a Set-1 make scancode, or None for keys this minimal
/// keymap does not name (function keys, navigation, the symbol row, ...).
fn decode(key: u8) -> Option<u8> {
    Some(match key {
        0x02 => b'1', 0x03 => b'2', 0x04 => b'3', 0x05 => b'4', 0x06 => b'5',
        0x07 => b'6', 0x08 => b'7', 0x09 => b'8', 0x0A => b'9', 0x0B => b'0',
        0x10 => b'q', 0x11 => b'w', 0x12 => b'e', 0x13 => b'r', 0x14 => b't',
        0x15 => b'y', 0x16 => b'u', 0x17 => b'i', 0x18 => b'o', 0x19 => b'p',
        0x1E => b'a', 0x1F => b's', 0x20 => b'd', 0x21 => b'f', 0x22 => b'g',
        0x23 => b'h', 0x24 => b'j', 0x25 => b'k', 0x26 => b'l',
        0x2C => b'z', 0x2D => b'x', 0x2E => b'c', 0x2F => b'v', 0x30 => b'b',
        0x31 => b'n', 0x32 => b'm',
        0x39 => b' ',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build the (kind, code, value) triples event_recv would deliver.
    fn make(sc: u8) -> (u8, u16, u8) {
        (EVENT_KEY, sc as u16, 1)
    }
    fn brk(sc: u8) -> (u8, u16, u8) {
        (EVENT_KEY, (sc as u16) | 0x80, 0)
    }
    fn feed(km: &mut Keymap, e: (u8, u16, u8)) -> Key {
        km.feed(e.0, e.1, e.2)
    }

    #[test]
    fn letters_and_enter() {
        let mut km = Keymap::new();
        assert_eq!(feed(&mut km, make(0x23)), Key::Char(b'h'));
        assert_eq!(feed(&mut km, make(0x17)), Key::Char(b'i'));
        assert_eq!(feed(&mut km, make(0x1C)), Key::Enter);
    }

    #[test]
    fn shift_uppercases_letters_then_releases() {
        let mut km = Keymap::new();
        assert_eq!(feed(&mut km, make(0x2A)), Key::None); // shift down
        assert_eq!(feed(&mut km, make(0x23)), Key::Char(b'H')); // 'H'
        assert_eq!(feed(&mut km, brk(0x2A)), Key::None); // shift up
        assert_eq!(feed(&mut km, make(0x17)), Key::Char(b'i')); // back to 'i'
    }

    #[test]
    fn shift_does_not_affect_digits() {
        let mut km = Keymap::new();
        feed(&mut km, make(0x2A)); // shift down
        assert_eq!(feed(&mut km, make(0x02)), Key::Char(b'1')); // not '!' in this map
    }

    #[test]
    fn backspace_and_releases_and_unmapped() {
        let mut km = Keymap::new();
        assert_eq!(feed(&mut km, make(0x0E)), Key::Backspace);
        assert_eq!(feed(&mut km, brk(0x23)), Key::None); // a key release
        assert_eq!(feed(&mut km, make(0x3B)), Key::None); // F1, unmapped
        assert_eq!(km.feed(99, 0x23, 1), Key::None); // not a key event
    }
}
