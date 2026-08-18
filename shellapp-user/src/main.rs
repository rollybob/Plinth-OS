//! The app a shell launches (D8 skin, D6c --): a real
//! process the home screen `spawn`s and hands the framebuffer to.
//!
//! It maps the transferred `Framebuffer` capability, draws its own screen, hashes
//! it (the determinism proof), then transfers the framebuffer capability BACK to
//! the shell over the spawn result channel and exits. The display capability
//! moves shell -> app -> shell -- a genuine focus handoff built only from
//! `spawn` + IPC capability transfer + `fb_map`, with no new kernel surface.

#![no_std]
#![no_main]

use libgfx::Framebuffer;
use libplinth::{
    sys_exit, sys_send_cap, sys_write, write_hex, ENDPOINT_SLOT, MAP_BASE, SPAWN_GRANT_SLOT,
};

const HASH_SIDE: u32 = 128;

#[no_mangle]
pub extern "C" fn _start(_idx: u64) -> ! {
    sys_write(b"shellapp: start\n");

    let fb = match Framebuffer::map(SPAWN_GRANT_SLOT, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(b"shellapp: map failed\n");
            sys_exit(1);
        }
    };
    let info = fb.info();

    // Draw the app's screen: a solid field + a centered title.
    let bg = (0x08u8, 0x30u8, 0x18u8);
    let fg = (0xF0u8, 0xF0u8, 0xF0u8);
    fb.fill_rect(0, 0, info.width, info.height, bg.0, bg.1, bg.2);
    fb.draw_text_centered(info.width / 2, info.height / 2, b"APP RUNNING", fg, bg, 3);

    let hash = fb.hash_origin_square(HASH_SIDE);
    sys_write(b"shellapp: framebuffer hash ");
    write_hex(hash);
    sys_write(b"\n");

    // Hand the framebuffer back to the shell over the spawn result channel: a
    // capability transfer (sys_send_cap moves the framebuffer cap out of this
    // process and into the shell, which recv_caps it as the join). Blocks until
    // the shell receives; then we exit. The hash above was taken while the
    // framebuffer was still mapped here.
    sys_write(b"shellapp: returning the screen\n");
    sys_send_cap(ENDPOINT_SLOT, 0, SPAWN_GRANT_SLOT);

    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
