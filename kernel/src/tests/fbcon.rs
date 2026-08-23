//! Framebuffer console blitter tests (D11 step 4).
//!
//! The blitter draws into a real GOP framebuffer on a serial-less machine,
//! which QEMU (always serial) never forces, so these tests do what the gfx
//! demos do: render into a fixed buffer and assert an exact FNV-1a hash of the
//! result. A known-good hash pins the pixels; the distinct-glyph test proves
//! the hash actually covers the drawing rather than passing on a blank frame.
//!
//! The surface is a static byte array, not a mapped framebuffer, so the test
//! exercises the identical pixel math without any display hardware.

use super::TestCtx;
use crate::fbcon::{FbConsole, Surface};
use crate::framebuffer::FMT_RGB;
use crate::serial;
use crate::test_assert;
use core::fmt::Write;

const W: u32 = 64;
const H: u32 = 24;
const BPP: u8 = 4;
const N: usize = (W * H) as usize * BPP as usize;

/// Backing store for the fake framebuffer. The suite is single-threaded, so a
/// shared static is safe as long as each test clears before drawing.
static mut FB: [u8; N] = [0; N];

fn surface() -> Surface {
    Surface {
        base: core::ptr::addr_of_mut!(FB) as *mut u8,
        width: W,
        height: H,
        stride: W,
        bytes_per_pixel: BPP,
        format: FMT_RGB,
    }
}

/// FNV-1a over the whole buffer, the same hash the gfx demos assert on.
fn hash_fb() -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let base = core::ptr::addr_of!(FB) as *const u8;
    let mut h = FNV_OFFSET;
    for i in 0..N {
        // SAFETY: i < N, and base points at FB, which is exactly N bytes.
        let byte = unsafe { base.add(i).read() };
        h ^= byte as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

fn report(name: &str, h: u64) {
    let mut s = serial::init();
    let _ = writeln!(s, "fbcon: {name} hash {h:#018x}");
}

/// Read the red channel of pixel (x, y). With FMT_RGB the R byte is first, so
/// it is 0xAA on a foreground pixel and 0x00 on background -- enough to check
/// where a glyph's lit pixels landed.
fn pixel_r(x: u32, y: u32) -> u8 {
    let offset = (y as usize * W as usize + x as usize) * BPP as usize;
    let base = core::ptr::addr_of!(FB) as *const u8;
    // SAFETY: offset < N for x < W, y < H.
    unsafe { base.add(offset).read() }
}

pub fn renders_known_string(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut con = FbConsole::with_cursor(surface(), 0, 0);
    con.clear();
    let _ = write!(con, "PLINTH");
    let h = hash_fb();
    report("known_string", h);
    test_assert!(
        h == 0x3332_9483_fcc7_0cc5,
        "framebuffer render hash changed -- inspect the glyph blit"
    );
    Ok(())
}

/// Correctness, not just determinism: check that a known glyph's lit pixels
/// land where the font says, with the most-significant bit on the left. Glyph
/// 'I' row 0 is 0x3C = 0b0011_1100, so columns 2..=5 are foreground and columns
/// 0, 1, 6, 7 are background.
pub fn blit_places_pixels(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut con = FbConsole::with_cursor(surface(), 0, 0);
    con.clear();
    let _ = write!(con, "I");
    test_assert!(pixel_r(0, 0) == 0x00, "col 0 row 0 should be background");
    test_assert!(pixel_r(1, 0) == 0x00, "col 1 row 0 should be background");
    test_assert!(pixel_r(2, 0) == 0xAA, "col 2 row 0 should be foreground");
    test_assert!(pixel_r(5, 0) == 0xAA, "col 5 row 0 should be foreground");
    test_assert!(pixel_r(6, 0) == 0x00, "col 6 row 0 should be background");
    Ok(())
}

pub fn distinct_glyphs_distinct_hash(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut con = FbConsole::with_cursor(surface(), 0, 0);
    con.clear();
    let _ = write!(con, "A");
    let ha = hash_fb();

    let mut con = FbConsole::with_cursor(surface(), 0, 0);
    con.clear();
    let _ = write!(con, "B");
    let hb = hash_fb();

    test_assert!(
        ha != hb,
        "distinct glyphs produced identical frames -- the hash does not cover the blit"
    );
    Ok(())
}

pub fn wrap_and_scroll_stable(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    // 64px / 8px = 8 columns, 24px / 8px = 3 rows. 32 characters wrap onto a
    // fourth line, whose newline forces exactly one scroll.
    let mut con = FbConsole::with_cursor(surface(), 0, 0);
    con.clear();
    let _ = write!(con, "ABCDEFGHIJKLMNOPQRSTUVWXYZ012345");
    let h = hash_fb();
    report("wrap_scroll", h);
    test_assert!(
        h == 0x14e3_92fb_9b16_9d07,
        "wrap/scroll render hash changed -- inspect newline/scroll"
    );
    Ok(())
}
