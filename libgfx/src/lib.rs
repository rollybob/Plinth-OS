//! libgfx -- a minimal graphics library OS over the framebuffer capability.
//!
//! This is rendering as *policy*: the kernel multiplexes the raw framebuffer (a
//! `Framebuffer` capability + the `fb_map` syscall) and refuses to know anything
//! about pixels, so drawing -- channel order, the test pattern, the determinism
//! hash, and (Stage 3) fonts and text -- lives here, in unprivileged code,
//! exactly the way libfs owns the on-disk layout over `BlockRange` and the two
//! memory policies in `libos` live over raw frame capabilities.
//!
//! Stage 2 provides the mapping + the pixel primitives + a deterministic hash.
//! A bitmap font and text rendering are Stage 3; sub-region grants (two libOSes
//! sharing one screen) are Stage 4 -- both additive over this same surface.

#![no_std]

use libplinth::{sys_fb_map, FB_FMT_BGR, FB_FMT_RGB, FB_FMT_U8, SYS_ERR};

/// Framebuffer geometry, filled by the kernel at map time. Field order and types
/// mirror the kernel's `fb_map` write (five u32s).
#[derive(Clone, Copy)]
#[repr(C)]
pub struct FbInfo {
    pub width: u32,
    pub height: u32,
    /// Pixels per row (>= width); rows are `stride * bytes_per_pixel` bytes apart.
    pub stride: u32,
    pub bytes_per_pixel: u32,
    pub format: u32,
}

/// A mapped framebuffer: its base virtual address plus geometry. Drawing writes
/// straight into the mapped pixels -- the kernel is not on the path.
pub struct Framebuffer {
    base: u64,
    info: FbInfo,
}

impl Framebuffer {
    /// Map the framebuffer capability at `slot` at virtual address `va` (inside
    /// the map window, with room for the whole region). Returns `None` if the
    /// kernel rejected the map (bad slot, not a Framebuffer capability, missing
    /// RIGHT_MAP, or `va`/geometry out of range).
    pub fn map(slot: u64, va: u64) -> Option<Framebuffer> {
        let mut info = FbInfo {
            width: 0,
            height: 0,
            stride: 0,
            bytes_per_pixel: 0,
            format: 0,
        };
        let info_ptr = &mut info as *mut FbInfo as u64;
        if sys_fb_map(slot, va, info_ptr) == SYS_ERR {
            return None;
        }
        Some(Framebuffer { base: va, info })
    }

    pub fn info(&self) -> FbInfo {
        self.info
    }

    /// Byte offset of pixel (x, y) from the framebuffer base.
    #[inline]
    fn offset(&self, x: u32, y: u32) -> u64 {
        (y as u64 * self.info.stride as u64 + x as u64) * self.info.bytes_per_pixel as u64
    }

    /// Write one pixel in the framebuffer's native channel order, zeroing any
    /// padding/alpha byte so the result is fully determined by (r, g, b) -- the
    /// property the determinism hash relies on. Out-of-bounds (x, y) is ignored.
    #[inline]
    pub fn put_pixel(&self, x: u32, y: u32, r: u8, g: u8, b: u8) {
        if x >= self.info.width || y >= self.info.height {
            return;
        }
        let bpp = self.info.bytes_per_pixel;
        let p = (self.base + self.offset(x, y)) as *mut u8;
        let (b0, b1, b2, written) = match self.info.format {
            FB_FMT_RGB => (r, g, b, 3u32),
            FB_FMT_BGR => (b, g, r, 3u32),
            FB_FMT_U8 => (((r as u16 + g as u16 + b as u16) / 3) as u8, 0, 0, 1u32),
            // Unknown layout: write red into the first byte, zero the rest.
            _ => (r, 0, 0, 1u32),
        };
        // SAFETY: (x, y) is in bounds, so [p, p+bpp) lies within the mapped
        // framebuffer. Writes are volatile so the compiler cannot fold the draw
        // loop into a memcpy (the no_std loop-to-memcpy hazard libplinth notes).
        unsafe {
            p.write_volatile(b0);
            if written >= 3 {
                p.add(1).write_volatile(b1);
                p.add(2).write_volatile(b2);
            }
            let mut i = written;
            while i < bpp {
                p.add(i as usize).write_volatile(0);
                i += 1;
            }
        }
    }

    /// FNV-1a hash over the `side` x `side` square at the origin, read back
    /// through the mapping byte by byte (via the row stride, so it is
    /// resolution-independent). This is the determinism proof (Design/display.md
    /// D6): a known draw yields a known hash, asserted on the serial console.
    /// `side` is clamped to the screen.
    pub fn hash_origin_square(&self, side: u32) -> u64 {
        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        let hw = if side < self.info.width { side } else { self.info.width };
        let hh = if side < self.info.height { side } else { self.info.height };
        let bpp = self.info.bytes_per_pixel as u64;
        let mut hash = FNV_OFFSET;
        let mut y = 0u32;
        while y < hh {
            let mut x = 0u32;
            while x < hw {
                let p = (self.base + self.offset(x, y)) as *const u8;
                let mut i = 0u64;
                while i < bpp {
                    // SAFETY: (x, y) in bounds; [p, p+bpp) is mapped.
                    let byte = unsafe { p.add(i as usize).read_volatile() };
                    hash ^= byte as u64;
                    hash = hash.wrapping_mul(FNV_PRIME);
                    i += 1;
                }
                x += 1;
            }
            y += 1;
        }
        hash
    }

    /// Fill a `w` x `h` rectangle at (x, y) with a solid colour. Out-of-bounds
    /// pixels are clipped by put_pixel.
    pub fn fill_rect(&self, x: u32, y: u32, w: u32, h: u32, r: u8, g: u8, b: u8) {
        let mut yy = y;
        while yy < y.saturating_add(h) {
            let mut xx = x;
            while xx < x.saturating_add(w) {
                self.put_pixel(xx, yy, r, g, b);
                xx += 1;
            }
            yy += 1;
        }
    }

    /// Draw one glyph at (x, y), magnified `scale`x, filling the whole cell --
    /// foreground for set bits, background for clear ones -- so the rendered
    /// region is fully determined (no leftover pixels show through the text,
    /// which the determinism hash relies on).
    pub fn draw_char(&self, x: u32, y: u32, c: u8, fg: (u8, u8, u8), bg: (u8, u8, u8), scale: u32) {
        let rows = glyph(c);
        let mut ry = 0u32;
        while ry < FONT_H {
            let bits = rows[ry as usize];
            let mut rx = 0u32;
            while rx < FONT_W {
                let on = (bits >> (7 - rx)) & 1 != 0;
                let (r, g, b) = if on { fg } else { bg };
                // Magnify the bit into a scale x scale block.
                let mut sy = 0u32;
                while sy < scale {
                    let mut sx = 0u32;
                    while sx < scale {
                        self.put_pixel(x + rx * scale + sx, y + ry * scale + sy, r, g, b);
                        sx += 1;
                    }
                    sy += 1;
                }
                rx += 1;
            }
            ry += 1;
        }
    }

    /// Draw `text` left to right from (x, y), magnified `scale`x, advancing one
    /// cell (FONT_W * scale) per byte. No wrapping; lowercase folds to uppercase
    /// (the font is uppercase Latin + digits + minimal punctuation).
    pub fn draw_text(&self, x: u32, y: u32, text: &[u8], fg: (u8, u8, u8), bg: (u8, u8, u8), scale: u32) {
        let mut cx = x;
        for &c in text {
            self.draw_char(cx, y, c, fg, bg, scale);
            cx += FONT_W * scale;
        }
    }
}

/// 8x8 glyph cell dimensions.
pub const FONT_W: u32 = 8;
pub const FONT_H: u32 = 8;

/// The 8x8 rows for character `c` (lowercase folds to uppercase). MSB is the
/// leftmost pixel; the eighth row is left blank as inter-line spacing. Unknown
/// characters render blank. This is a small clean-room font -- uppercase Latin,
/// digits, and a handful of punctuation, enough for labels and echoed input;
/// lowercase glyphs and a wider symbol set are libOS polish (Stage 5+), not a
/// kernel concern.
fn glyph(c: u8) -> [u8; 8] {
    let c = if c.is_ascii_lowercase() { c - 32 } else { c };
    match c {
        b' ' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        b'0' => [0x3C, 0x66, 0x6E, 0x76, 0x66, 0x66, 0x3C, 0x00],
        b'1' => [0x18, 0x38, 0x18, 0x18, 0x18, 0x18, 0x7E, 0x00],
        b'2' => [0x3C, 0x66, 0x06, 0x0C, 0x30, 0x60, 0x7E, 0x00],
        b'3' => [0x3C, 0x66, 0x06, 0x1C, 0x06, 0x66, 0x3C, 0x00],
        b'4' => [0x0C, 0x1C, 0x3C, 0x6C, 0x7E, 0x0C, 0x0C, 0x00],
        b'5' => [0x7E, 0x60, 0x7C, 0x06, 0x06, 0x66, 0x3C, 0x00],
        b'6' => [0x1C, 0x30, 0x60, 0x7C, 0x66, 0x66, 0x3C, 0x00],
        b'7' => [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x30, 0x30, 0x00],
        b'8' => [0x3C, 0x66, 0x66, 0x3C, 0x66, 0x66, 0x3C, 0x00],
        b'9' => [0x3C, 0x66, 0x66, 0x3E, 0x06, 0x0C, 0x38, 0x00],
        b'A' => [0x18, 0x3C, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x00],
        b'B' => [0x7C, 0x66, 0x66, 0x7C, 0x66, 0x66, 0x7C, 0x00],
        b'C' => [0x3C, 0x66, 0x60, 0x60, 0x60, 0x66, 0x3C, 0x00],
        b'D' => [0x78, 0x6C, 0x66, 0x66, 0x66, 0x6C, 0x78, 0x00],
        b'E' => [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x7E, 0x00],
        b'F' => [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x60, 0x00],
        b'G' => [0x3C, 0x66, 0x60, 0x6E, 0x66, 0x66, 0x3C, 0x00],
        b'H' => [0x66, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x00],
        b'I' => [0x3C, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00],
        b'J' => [0x1E, 0x0C, 0x0C, 0x0C, 0x0C, 0x6C, 0x38, 0x00],
        b'K' => [0x66, 0x6C, 0x78, 0x70, 0x78, 0x6C, 0x66, 0x00],
        b'L' => [0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x7E, 0x00],
        b'M' => [0x63, 0x77, 0x7F, 0x6B, 0x63, 0x63, 0x63, 0x00],
        b'N' => [0x66, 0x76, 0x7E, 0x7E, 0x6E, 0x66, 0x66, 0x00],
        b'O' => [0x3C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00],
        b'P' => [0x7C, 0x66, 0x66, 0x7C, 0x60, 0x60, 0x60, 0x00],
        b'Q' => [0x3C, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x0E, 0x00],
        b'R' => [0x7C, 0x66, 0x66, 0x7C, 0x78, 0x6C, 0x66, 0x00],
        b'S' => [0x3C, 0x66, 0x60, 0x3C, 0x06, 0x66, 0x3C, 0x00],
        b'T' => [0x7E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00],
        b'U' => [0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00],
        b'V' => [0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x18, 0x00],
        b'W' => [0x63, 0x63, 0x63, 0x6B, 0x7F, 0x77, 0x63, 0x00],
        b'X' => [0x66, 0x66, 0x3C, 0x18, 0x3C, 0x66, 0x66, 0x00],
        b'Y' => [0x66, 0x66, 0x66, 0x3C, 0x18, 0x18, 0x18, 0x00],
        b'Z' => [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x7E, 0x00],
        b'.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00],
        b',' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x30],
        b':' => [0x00, 0x18, 0x18, 0x00, 0x00, 0x18, 0x18, 0x00],
        b'!' => [0x18, 0x18, 0x18, 0x18, 0x18, 0x00, 0x18, 0x00],
        b'?' => [0x3C, 0x66, 0x06, 0x0C, 0x18, 0x00, 0x18, 0x00],
        b'-' => [0x00, 0x00, 0x00, 0x7E, 0x00, 0x00, 0x00, 0x00],
        b'/' => [0x06, 0x0C, 0x0C, 0x18, 0x30, 0x30, 0x60, 0x00],
        _ => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    }
}
