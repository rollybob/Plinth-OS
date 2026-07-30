//! The shell -- the visual userspace skin (D8, Design/display_skin.md): a splash,
//! a home screen of app icons, arrow-key navigation with a selection cursor, and
//! launching.
//!
//! It holds the whole-screen `Framebuffer` (FB_SLOT) and the keyboard
//! `EventSource` (KBD_SLOT). Three icons are shell-drawn "views"; the fourth is a
//! real app the shell `spawn`s, handing it the framebuffer and getting it back
//! over the spawn result channel (D6c) -- the display capability as transferable
//! focus. A scripted scancode sequence (armed kernel-side) drives it
//! deterministically; each fixed frame's top-left square is hashed to serial.
//!
//! All of it -- splash, layout, icons, the launch handoff -- is unprivileged
//! policy over the framebuffer/keyboard capabilities + spawn + IPC. The kernel
//! draws nothing and knows nothing of icons, cursors, or "apps".

#![no_std]
#![no_main]

use libgfx::{text_width, Framebuffer};
use libinput::{read_key, Key, Keymap};
use libplinth::{
    sys_cap_release, sys_exit, sys_recv_cap, sys_spawn, sys_write, write_hex, ABI_VERSION, FB_SLOT,
    IPC_OK, MAP_BASE, NO_CAP, SYS_ERR,
};

/// The keyboard EventSource lands in the slot after the framebuffer (a single-
/// process run mints grants in order: Framebuffer at FB_SLOT = 1, then 2).
const KBD_SLOT: u64 = 2;
/// shellapp's index in the kernel's SPAWNABLE table (see main.rs).
const SHELLAPP_ID: u64 = 3;
const HASH_SIDE: u32 = 128;

/// A 2x2 icon grid; index = row*2 + col. Three views + one real app.
const ICON_LABELS: [&[u8]; 4] = [b"INFO", b"BARS", b"GREET", b"APP"];
const APP_ICON: usize = 3;

const BG: (u8, u8, u8) = (0x10, 0x10, 0x20);
const FG: (u8, u8, u8) = (0xE0, 0xE0, 0xF0);
const BAR: (u8, u8, u8) = (0x28, 0x28, 0x40);
const ICON_BORDER: (u8, u8, u8) = (0x60, 0x60, 0x80);
const SEL_BORDER: (u8, u8, u8) = (0xF0, 0xC0, 0x40);

fn draw_splash(fb: &Framebuffer) {
    let info = fb.info();
    fb.fill_rect(0, 0, info.width, info.height, BG.0, BG.1, BG.2);
    fb.draw_text_centered(info.width / 2, info.height / 2 - 30, b"PLINTH", FG, BG, 6);
    // "VERSION " + libplinth's ABI_VERSION, assembled on the stack -- no_std has
    // no format!. Read from libplinth rather than written out here so the splash
    // cannot fall behind the ABI again, as it did through the v2.8 bump.
    const PREFIX: &[u8] = b"VERSION ";
    let mut banner = [0u8; 24];
    let n = PREFIX.len() + ABI_VERSION.len();
    banner[..PREFIX.len()].copy_from_slice(PREFIX);
    banner[PREFIX.len()..n].copy_from_slice(ABI_VERSION);
    // Well below the hashed top-left square, so the splash hash is unaffected.
    fb.draw_text_centered(info.width / 2, info.height / 2 + 36, &banner[..n], FG, BG, 2);
    // ...which is exactly why the drawn banner needs a second, ASSERTED copy.
    // Nothing in the suite can see a framebuffer region no hash covers, so for
    // the whole of v2.8 the splash could have kept saying 2.7 (it did) with every
    // test green. Emitting the SAME buffer to serial, from the same expression,
    // means `expected_boot_log.txt` pins the version the binaries were actually
    // built against and the two cannot drift apart again.
    sys_write(b"shell: ");
    sys_write(&banner[..n]);
    sys_write(b"\n");
}

/// Rectangle (x, y, w, h) of icon `idx` in a 2x2 grid centered on screen.
fn icon_rect(w: u32, h: u32, idx: usize) -> (u32, u32, u32, u32) {
    let iw = 200u32;
    let ih = 120u32;
    let gap = 48u32;
    let grid_w = iw * 2 + gap;
    let grid_h = ih * 2 + gap;
    let ox = (w - grid_w) / 2;
    let oy = (h - grid_h) / 2 + 30;
    let col = (idx % 2) as u32;
    let row = (idx / 2) as u32;
    (ox + col * (iw + gap), oy + row * (ih + gap), iw, ih)
}

/// Paint one icon cell: the box, its border, and its label.
///
/// The cell is refilled before the border is drawn, and `draw_border` paints
/// just *inside* the rectangle, so a previously-thicker selection border is
/// fully erased. That makes this pixel-identical to what `draw_home` draws for
/// the same icon -- which is what lets a selection move repaint two cells
/// instead of the whole screen.
fn draw_icon(fb: &Framebuffer, sw: u32, sh: u32, idx: usize, selected: bool) {
    let (x, y, w, h) = icon_rect(sw, sh, idx);
    fb.fill_rect(x, y, w, h, BAR.0, BAR.1, BAR.2);
    let (border, t) = if selected { (SEL_BORDER, 4) } else { (ICON_BORDER, 2) };
    fb.draw_border(x, y, w, h, t, border.0, border.1, border.2);
    let label = ICON_LABELS[idx];
    let lw = text_width(label, 3);
    fb.draw_text(x + (w - lw) / 2, y + h / 2 - 12, label, FG, BAR, 3);
}

/// Move the selection highlight by repainting only the two cells whose borders
/// change.
///
/// A full `draw_home` here would clear the entire screen and repaint it on every
/// arrow press. There is no back buffer -- drawing goes straight to the scanned-
/// out framebuffer -- so that full-screen clear is visible as a flash. The icons
/// are disjoint from each other, from the title bar, and from the bottom hint,
/// so repainting just these two cells leaves the screen in exactly the state a
/// full `draw_home` would have produced.
fn move_selection(fb: &Framebuffer, prev: usize, sel: usize) {
    if prev == sel {
        return;
    }
    let info = fb.info();
    draw_icon(fb, info.width, info.height, prev, false);
    draw_icon(fb, info.width, info.height, sel, true);
}

fn draw_home(fb: &Framebuffer, sel: usize) {
    let info = fb.info();
    fb.fill_rect(0, 0, info.width, info.height, BG.0, BG.1, BG.2);
    // Title bar (lands in the hashed top-left square).
    fb.fill_rect(0, 0, info.width, 40, BAR.0, BAR.1, BAR.2);
    fb.draw_text(8, 8, b"PLINTH HOME", FG, BAR, 3);
    let mut i = 0;
    while i < 4 {
        draw_icon(fb, info.width, info.height, i, i == sel);
        i += 1;
    }
    // Controls hint along the bottom. It sits well below the hashed top-left
    // square, so it does not affect the determinism hash.
    fb.draw_text_centered(
        info.width / 2,
        info.height - 48,
        b"ARROWS MOVE   ENTER OPEN   Q QUIT",
        FG,
        BG,
        2,
    );
}

fn draw_view(fb: &Framebuffer, label: &[u8]) {
    let info = fb.info();
    let bg = (0x18u8, 0x10u8, 0x28u8);
    fb.fill_rect(0, 0, info.width, info.height, bg.0, bg.1, bg.2);
    fb.draw_text_centered(info.width / 2, info.height / 2 - 24, label, FG, bg, 4);
    fb.draw_text_centered(info.width / 2, info.height / 2 + 28, b"BACKSPACE TO RETURN", FG, bg, 2);
}

fn emit_hash(tag: &[u8], fb: &Framebuffer) {
    sys_write(tag);
    write_hex(fb.hash_origin_square(HASH_SIDE));
    sys_write(b"\n");
}

#[no_mangle]
pub extern "C" fn _start(_idx: u64) -> ! {
    sys_write(b"shell: start\n");

    // Rebindable: the mapping is torn down whenever the capability leaves this
    // table, so every launch round-trip ends with a fresh `map` (ABI v2.8 fix,
    // Design/fb_mapping.md D1/D3).
    let mut fb = match Framebuffer::map(FB_SLOT, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(b"shell: map failed\n");
            sys_exit(1);
        }
    };

    draw_splash(&fb);
    emit_hash(b"shell: splash hash ", &fb);

    let mut sel = 0usize;
    draw_home(&fb, sel);
    emit_hash(b"shell: home hash ", &fb);

    // The framebuffer capability's CURRENT slot. It starts at FB_SLOT, but each
    // launch round-trip MOVES it: `sys_spawn` transfers it out (freeing FB_SLOT,
    // into which the wait handle is then minted), and the app hands it back via
    // `recv_cap`, which mints it into the next free slot -- NOT back at FB_SLOT.
    // So we must remember where it landed and transfer THAT slot next time, or a
    // second launch would hand the app the stale wait-handle instead.
    let mut fb_slot = FB_SLOT;

    let mut keymap = Keymap::new();
    loop {
        match read_key(KBD_SLOT, &mut keymap) {
            // 2x2 grid: up/down toggle the row, left/right toggle the column.
            Key::Up | Key::Down => {
                let prev = sel;
                sel ^= 2;
                move_selection(&fb, prev, sel);
            }
            Key::Left | Key::Right => {
                let prev = sel;
                sel ^= 1;
                move_selection(&fb, prev, sel);
            }
            Key::Enter => {
                if sel == APP_ICON {
                    // D6c: spawn the app and hand it the framebuffer; the spawn
                    // transfer revokes+unmaps our framebuffer here.
                    sys_write(b"shell: launching app\n");
                    let handle = sys_spawn(SHELLAPP_ID, fb_slot);
                    if handle == SYS_ERR {
                        sys_write(b"shell: spawn failed\n");
                        sys_exit(2);
                    }
                    // Join: the app draws, then transfers the framebuffer
                    // capability back over this channel before exiting. Getting a
                    // capability back (cap_slot != NO_CAP) is the authority
                    // returning -- the shell -> app -> shell handoff completing.
                    let (status, _msg, cap_slot) = sys_recv_cap(handle);
                    if status != IPC_OK || cap_slot == NO_CAP {
                        sys_write(b"shell: app did not return the screen\n");
                        sys_exit(3);
                    }
                    // The framebuffer came back at cap_slot, not FB_SLOT; track it
                    // so the next launch transfers the right capability.
                    fb_slot = cap_slot;
                    // The join is over, so the wait handle names an endpoint whose
                    // child is gone -- dead weight in a 16-slot table. Release it,
                    // or every launch costs a slot permanently and the ninth or so
                    // spawn fails with the table full (the 2026-06-27 crash; there
                    // was no way to say this before ABI v2.8's cap_release).
                    // Release AFTER the recv, not before: the handle is what we
                    // received on, and the returned framebuffer has already landed
                    // at cap_slot, so freeing this slot cannot disturb it.
                    if sys_cap_release(handle) != 0 {
                        sys_write(b"shell: releasing the spent wait handle failed\n");
                        sys_exit(4);
                    }
                    // Re-map before drawing. The spawn transfer took the
                    // framebuffer mapping down with the capability, so the old
                    // mapping is gone and touching it would fault -- which is
                    // the point: this shell can draw because it holds the
                    // capability and mapped it, not because a page-table entry
                    // happened to survive the handoff (Design/fb_mapping.md D3).
                    fb = match Framebuffer::map(fb_slot, MAP_BASE) {
                        Some(fb) => fb,
                        None => {
                            sys_write(b"shell: remap after launch failed\n");
                            sys_exit(5);
                        }
                    };
                    draw_home(&fb, sel);
                    emit_hash(b"shell: back home hash ", &fb);
                } else {
                    // A shell-drawn view; any key but the kernel-scripted
                    // Backspace would also work -- the demo scripts Backspace.
                    draw_view(&fb, ICON_LABELS[sel]);
                    emit_hash(b"shell: view hash ", &fb);
                    loop {
                        if let Key::Backspace = read_key(KBD_SLOT, &mut keymap) {
                            draw_home(&fb, sel);
                            break;
                        }
                    }
                }
            }
            Key::Char(b'q') | Key::Char(b'Q') => {
                // No farewell frame: the kernel terminates QEMU shortly after
                // this exit (isa-debug-exit is attached on every path since
                // 2026-07-25), so anything drawn here would be on screen for
                // microseconds.
                sys_write(b"shell: quit\n");
                sys_exit(0);
            }
            _ => {}
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
