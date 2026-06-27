//! Graphics library-OS demo (Stage 2, Design/display.md): the framebuffer as a
//! capability.
//!
//! The kernel grants this process a Framebuffer capability (at FB_SLOT) and
//! nothing about pixels. The demo:
//!   1. proves a non-framebuffer capability cannot be `fb_map`ped -- even a Frame
//!      capability, which carries RIGHT_MAP, is refused on the *kind* check (the
//!      multiplexing/type guard, the display analogue of BlockRange's check);
//!   2. maps the real framebuffer through libgfx, draws a fixed plaid straight
//!      into the mapped memory, and hashes a 128x128 origin square to serial.
//!
//! The hash is byte-identical to the value Stage 1's kernel-side draw produced,
//! which proves the libOS-through-capability path yields the same pixels the
//! kernel did directly -- with all drawing now unprivileged policy.

#![no_std]
#![no_main]

use libgfx::Framebuffer;
use libplinth::{
    sys_exit, sys_fb_map, sys_frame_alloc, sys_write, write_hex, FB_SLOT, MAP_BASE, SYS_ERR,
};

/// Side of the origin square the hash covers -- the same fixed square the kernel
/// hashed in Stage 1, so the values must match.
const HASH_SIDE: u32 = 128;

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_write(b"gfx: start\n");

    // (1) Negative test. A Frame capability carries RIGHT_MAP (frame_alloc grants
    // it), so this isolates the kind check from the rights check: fb_map must
    // still refuse it, because it is not a Framebuffer capability.
    let frame = sys_frame_alloc();
    if frame == SYS_ERR {
        sys_exit(1);
    }
    let mut scratch = [0u32; 5];
    if sys_fb_map(frame, MAP_BASE, scratch.as_mut_ptr() as u64) != SYS_ERR {
        sys_write(b"gfx: non-framebuffer NOT rejected\n");
        sys_exit(2);
    }
    sys_write(b"gfx: non-framebuffer rejected\n");

    // (2) Map the real framebuffer and draw the plaid: each pixel's colour is a
    // pure function of (x, y), so the drawn bytes are identical every run.
    let fb = match Framebuffer::map(FB_SLOT, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(b"gfx: framebuffer map failed\n");
            sys_exit(3);
        }
    };
    let info = fb.info();
    let mut y = 0u32;
    while y < info.height {
        let mut x = 0u32;
        while x < info.width {
            fb.put_pixel(x, y, x as u8, y as u8, (x ^ y) as u8);
            x += 1;
        }
        y += 1;
    }

    let hash = fb.hash_origin_square(HASH_SIDE);
    sys_write(b"gfx: framebuffer hash ");
    write_hex(hash);
    sys_write(b"\n");
    sys_write(b"gfx: ok\n");

    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
