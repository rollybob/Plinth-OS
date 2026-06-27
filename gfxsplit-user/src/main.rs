//! Sub-region multiplexing demo (Stage 4, Design/display.md) -- the thesis
//! climax. Run as TWO concurrent processes, each granted a DISJOINT horizontal
//! band of the screen (process 0 the top, process 1 the bottom). Each maps its
//! band, fills it with a distinct colour, and draws a label -- confined to its
//! band by the page tables: its address space contains only its rows, so neither
//! can touch the other's pixels. This is the display analogue of two library
//! OSes handed disjoint `BlockRange`s.
//!
//! Reaching the `ok` line proves the draw stayed inside the grant: a write past
//! the band's mapping would fault first (that boundary is exercised on purpose
//! by gfxbound-user). The kernel multiplexed the raw region; the colours, the
//! font, and the layout are all unprivileged policy.

#![no_std]
#![no_main]

use libgfx::Framebuffer;
use libplinth::{sys_exit, sys_write, FB_SLOT, MAP_BASE};

#[no_mangle]
pub extern "C" fn _start(idx: u64) -> ! {
    // The kernel passes this process's scheduler slot in RDI: 0 = top band,
    // 1 = bottom band. Each was granted its own band capability at FB_SLOT.
    let top = idx == 0;

    let fb = match Framebuffer::map(FB_SLOT, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(if top { b"gfxsplit[0]: map failed\n" } else { b"gfxsplit[1]: map failed\n" });
            sys_exit(1);
        }
    };
    let info = fb.info();

    // Distinct colour + label per band, so the composite is visibly two regions.
    let (bg, label): ((u8, u8, u8), &[u8]) = if top {
        ((0x40, 0x10, 0x10), b"PLINTH")
    } else {
        ((0x10, 0x10, 0x40), b"STAGE 4")
    };
    let fg = (0xF0u8, 0xF0u8, 0xF0u8);

    // Fill and label THIS band only -- info.height is the band height, so the
    // draw cannot reach the other band (and libgfx clips to it regardless).
    fb.fill_rect(0, 0, info.width, info.height, bg.0, bg.1, bg.2);
    fb.draw_text(8, 8, label, fg, bg, 3);

    sys_write(if top { b"gfxsplit[0]: ok\n" } else { b"gfxsplit[1]: ok\n" });
    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
