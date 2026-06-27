//! Sub-region boundary demo (Stage 4, Design/display.md) -- the negative case.
//!
//! Granted a single horizontal band, this process deliberately writes one row
//! PAST its band: the byte just after its mapping, which is the next band's first
//! row and is NOT mapped in this address space. The kernel takes the page fault
//! and terminates the process -- the display analogue of BlockRange's
//! "out-of-range rejected", except enforced structurally by paging rather than a
//! per-call check (the kernel is off the framebuffer write path). The rest of
//! boot continues; a band holder cannot reach pixels it was not granted.

#![no_std]
#![no_main]

use libgfx::Framebuffer;
use libplinth::{sys_exit, sys_write, FB_SLOT, MAP_BASE};

#[no_mangle]
pub extern "C" fn _start(_idx: u64) -> ! {
    let fb = match Framebuffer::map(FB_SLOT, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(b"gfxbound: map failed\n");
            sys_exit(1);
        }
    };
    let info = fb.info();

    // One byte past the mapped band: the first row of the NEXT band, never mapped
    // into this process. The band was mapped at MAP_BASE for height*stride*bpp
    // bytes, so this address is the first unmapped page above it.
    let band_bytes =
        info.height as u64 * info.stride as u64 * info.bytes_per_pixel as u64;
    let past = MAP_BASE + band_bytes;

    sys_write(b"gfxbound: writing past my band\n");
    // SAFETY: this is intentionally a fault -- `past` is outside the grant, so it
    // is unmapped, and the write takes a #PF the kernel turns into termination.
    unsafe {
        (past as *mut u8).write_volatile(0xFF);
    }

    // Not reached: the write above faults and the kernel terminates this process.
    sys_write(b"gfxbound: NOT rejected\n");
    sys_exit(2)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
