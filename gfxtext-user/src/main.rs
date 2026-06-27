//! Text rendering + an input-driven frame (Stage 3, Design/display.md).
//!
//! The kernel grants this process the whole-screen Framebuffer capability (at
//! FB_SLOT) AND the keyboard EventSource (at slot 2). It clears the screen,
//! draws a fixed "PLINTH" title, then reads a line through the keyboard and
//! echoes it on-screen -- raw scancodes decoded by libinput (the keymap libOS),
//! glyphs rendered by libgfx, all unprivileged policy over the two capabilities.
//! The kernel knows nothing about fonts or keymaps.
//!
//! A scripted scancode sequence (armed kernel-side) drives the input, so the
//! rendered frame -- and the hash of its top-left square -- are deterministic,
//! the same discipline the keyboard input smoke uses.

#![no_std]
#![no_main]

use libgfx::Framebuffer;
use libinput::read_line;
use libplinth::{sys_exit, sys_write, write_hex, FB_SLOT, MAP_BASE};

/// The keyboard EventSource lands in the slot after the Framebuffer: a single-
/// process run mints its grants in order (Framebuffer at FB_SLOT = 1, then the
/// EventSource at 2).
const KBD_SLOT: u64 = 2;

/// Side of the origin square the determinism hash covers; both the title and the
/// echoed line are drawn inside it.
const HASH_SIDE: u32 = 128;

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_write(b"gfxtext: start\n");

    let fb = match Framebuffer::map(FB_SLOT, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(b"gfxtext: map failed\n");
            sys_exit(1);
        }
    };
    let info = fb.info();

    let bg = (0x10u8, 0x10u8, 0x28u8);
    let fg = (0xE0u8, 0xE0u8, 0xF0u8);

    // Clear the screen, then draw a fixed title.
    fb.fill_rect(0, 0, info.width, info.height, bg.0, bg.1, bg.2);
    fb.draw_text(4, 4, b"PLINTH", fg, bg, 2);

    // Read a scripted line through the keyboard and echo it on-screen. The font
    // folds case, so the lowercase the keymap emits renders as uppercase.
    let mut buf = [0u8; 64];
    let n = read_line(KBD_SLOT, &mut buf);
    fb.draw_text(4, 24, &buf[..n], fg, bg, 2);

    sys_write(b"gfxtext: line '");
    sys_write(&buf[..n]);
    sys_write(b"'\n");

    // The determinism proof: a known frame yields a known hash (Design/display.md
    // D6), now over text rendered through the capability.
    let hash = fb.hash_origin_square(HASH_SIDE);
    sys_write(b"gfxtext: framebuffer hash ");
    write_hex(hash);
    sys_write(b"\n");
    sys_write(b"gfxtext: ok\n");

    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
