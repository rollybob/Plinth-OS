//! Framebuffer text console backend (D11 step 4).
//!
//! The glyph blitter that lets the kernel speak on a machine with no serial
//! port. It owns *layout* -- cursor advance, line wrap, scroll -- over a raw
//! linear framebuffer, drawing each character from the shared `libfont` glyph
//! table. That split is deliberate and is the whole licence for the kernel
//! touching a pixel: `libfont` answers "which pixels make an A" (data), the
//! console decides where the A goes (the diagnostic policy the kernel is
//! allowed to own precisely because no library OS exists in that window). See
//! Design/real_hardware.md, the D11 ruling.
//!
//! This module is the decision-independent core: a surface descriptor, a
//! cursor, and the pixel math, holding no global state and taking no lock, so
//! the same code renders identically to a real GOP framebuffer and to a test
//! buffer. The boot-time surface capture, the lock/panic model, and the
//! pre-grant drawing window (D11 rule 4) are integration decisions handled
//! where the global console state lives, not here.

use crate::framebuffer::{FMT_BGR, FMT_RGB, FMT_U8};
use core::fmt::{self, Write};
use libfont::{glyph, FONT_H, FONT_W};

/// Diagnostic text colour: a light grey on black. Chosen for legibility, not
/// meaning -- the console carries no colour semantics.
const FG: (u8, u8, u8) = (0xAA, 0xAA, 0xAA);
const BG: (u8, u8, u8) = (0x00, 0x00, 0x00);

/// A linear framebuffer to draw into: a raw base pointer plus the geometry
/// needed to address a pixel. Mirrors `framebuffer::FbRegion` but carries the
/// kernel-mapped virtual base rather than the physical one, because this side
/// writes pixels.
#[derive(Clone, Copy)]
pub(crate) struct Surface {
    /// Kernel-virtual base of the mapped framebuffer.
    pub base: *mut u8,
    pub width: u32,
    pub height: u32,
    /// Pixels per row (>= width); rows are `stride * bytes_per_pixel` apart.
    pub stride: u32,
    pub bytes_per_pixel: u8,
    /// One of `framebuffer::FMT_*`.
    pub format: u8,
}

/// A framebuffer text console: a surface and a cursor, advancing left-to-right
/// and top-to-bottom with wrap and scroll. Construct one, write to it, drop it;
/// it keeps no state beyond the cursor, so a caller that wants a persistent
/// cursor holds the console, not this module.
pub(crate) struct FbConsole {
    surface: Surface,
    /// Cursor top-left, in pixels.
    cx: u32,
    cy: u32,
}

impl FbConsole {
    /// A console over `surface` resuming at a given cursor. The global console
    /// keeps its cursor in atomics and rebuilds a short-lived `FbConsole` per
    /// write, so the fallback path never holds a lock -- a panic handler can
    /// draw without risking deadlock against an interrupted normal write.
    pub(crate) fn with_cursor(surface: Surface, cx: u32, cy: u32) -> Self {
        FbConsole { surface, cx, cy }
    }

    /// The cursor after the last write, to be stored back into the global state.
    pub(crate) fn cursor(&self) -> (u32, u32) {
        (self.cx, self.cy)
    }

    /// Paint the whole surface the background colour.
    pub(crate) fn clear(&mut self) {
        for y in 0..self.surface.height {
            for x in 0..self.surface.width {
                self.put_pixel(x, y, BG);
            }
        }
        self.cx = 0;
        self.cy = 0;
    }

    fn write_byte(&mut self, b: u8) {
        match b {
            b'\n' => self.newline(),
            b'\r' => self.cx = 0,
            _ => {
                // Wrap before drawing so a glyph never straddles the right edge.
                if self.cx + FONT_W > self.surface.width {
                    self.newline();
                }
                self.blit_glyph(b);
                self.cx += FONT_W;
            }
        }
    }

    fn newline(&mut self) {
        self.cx = 0;
        if self.cy + 2 * FONT_H > self.surface.height {
            // The next line would run past the bottom: scroll one line up and
            // keep the cursor on the (now cleared) last line.
            self.scroll_line();
        } else {
            self.cy += FONT_H;
        }
    }

    /// Draw one glyph at the cursor. MSB of each row is the leftmost pixel; the
    /// eighth row is libfont's inter-line spacing and is drawn like any other.
    fn blit_glyph(&mut self, c: u8) {
        let rows = glyph(c);
        for (dy, bits) in rows.iter().enumerate() {
            for dx in 0..FONT_W {
                let on = (bits >> (7 - dx)) & 1 != 0;
                let colour = if on { FG } else { BG };
                self.put_pixel(self.cx + dx, self.cy + dy as u32, colour);
            }
        }
    }

    /// Scroll the image up by one text line (`FONT_H` rows) and clear the row
    /// exposed at the bottom. A whole-framebuffer move per line is wasteful but
    /// this is a diagnostic path, not a hot one.
    fn scroll_line(&mut self) {
        let row_bytes = self.surface.stride as usize * self.surface.bytes_per_pixel as usize;
        let shift = FONT_H as usize * row_bytes;
        let total = self.surface.height as usize * row_bytes;
        // SAFETY: both ranges lie inside the mapped framebuffer
        // [base, base+total); `copy` handles the overlap (it is memmove, not
        // memcpy). `total - shift` is non-negative because a scroll only fires
        // once at least two text lines fit, so height >= 2*FONT_H > FONT_H.
        unsafe {
            core::ptr::copy(
                self.surface.base.add(shift),
                self.surface.base,
                total - shift,
            );
        }
        // Clear the newly exposed bottom line.
        let cleared_top = self.surface.height - FONT_H;
        for y in cleared_top..self.surface.height {
            for x in 0..self.surface.width {
                self.put_pixel(x, y, BG);
            }
        }
        self.cy = cleared_top;
    }

    /// Write one pixel, packing the colour for the surface's format. Out-of-
    /// bounds coordinates are ignored so a glyph near an edge cannot fault.
    fn put_pixel(&mut self, x: u32, y: u32, (r, g, b): (u8, u8, u8)) {
        if x >= self.surface.width || y >= self.surface.height {
            return;
        }
        let bpp = self.surface.bytes_per_pixel as usize;
        let offset = (y as usize * self.surface.stride as usize + x as usize) * bpp;
        let mut px = [0u8; 4];
        match self.surface.format {
            FMT_RGB => {
                px[0] = r;
                px[1] = g;
                px[2] = b;
            }
            FMT_BGR => {
                px[0] = b;
                px[1] = g;
                px[2] = r;
            }
            // Single 8-bit channel: fold to luminance. The three fixed-point
            // weights (~0.299/0.587/0.114) sum to 256, so the shift is exact.
            FMT_U8 => {
                let lum = (r as u32 * 77 + g as u32 * 150 + b as u32 * 29) >> 8;
                px[0] = lum as u8;
            }
            // Unknown layout (FMT_OTHER): refuse to guess at channel order.
            _ => return,
        }
        // SAFETY: `offset + bpp <= stride*height*bpp`, the mapped framebuffer
        // length, since x < width <= stride and y < height. `bpp` is 1..=4.
        unsafe {
            let dst = self.surface.base.add(offset);
            for (i, &byte) in px.iter().take(bpp).enumerate() {
                dst.add(i).write_volatile(byte);
            }
        }
    }
}

impl Write for FbConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            self.write_byte(b);
        }
        Ok(())
    }
}
