//! Framebuffer revocation demo (D7) -- the negative case.
//!
//! A mapping is not authority. This process maps its framebuffer band, writes
//! through the mapping to prove it is live, RELEASES the capability that
//! justified it, and then writes to the same address again -- and is
//! #PF-terminated for it.
//!
//! The order is the whole demo. A process that only faults after the release
//! would pass this test even if `fb_map` had silently done nothing, because an
//! address that was never mapped faults just as well. The first write is what
//! gives the second one meaning: the same store, to the same address, succeeds
//! while the capability is held and faults once it is gone. That gap is the
//! invariant -- access does not outlive authority -- and no positive demo can
//! show it, because the old behaviour (a mapping that outlives its capability)
//! passes every positive demo unchanged.
//!
//! Compare `gfxbound-user`, the other negative: there the grant is intact and
//! the process reaches outside it in *space*. Here the process stays exactly
//! inside its grant and reaches outside it in *time*.

#![no_std]
#![no_main]

use libgfx::Framebuffer;
use libplinth::{sys_cap_release, sys_exit, sys_write, FB_SLOT, MAP_BASE};

#[no_mangle]
pub extern "C" fn _start(_idx: u64) -> ! {
    let fb = match Framebuffer::map(FB_SLOT, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(b"gfxrevoke: map failed\n");
            sys_exit(1);
        }
    };

    // Prove the mapping is live BEFORE giving the capability up. put_pixel
    // stores straight into the mapped pixels with the kernel off the path, and
    // its writes are volatile, so reaching the next line means these pages are
    // genuinely present and writable in this address space. Pixel (0, 0) is at
    // MAP_BASE exactly -- the same byte the post-release store below targets.
    fb.put_pixel(0, 0, 0xFF, 0xFF, 0xFF);
    sys_write(b"gfxrevoke: mapped and drew through it\n");

    // Give the authority up. Per D1, losing a Framebuffer capability tears its
    // mapping down, so this unmaps the band as a side effect -- that side
    // effect is the thing under test.
    if sys_cap_release(FB_SLOT) != 0 {
        sys_write(b"gfxrevoke: release failed\n");
        sys_exit(2);
    }
    sys_write(b"gfxrevoke: released the capability, writing anyway\n");

    // SAFETY: this is intentionally a fault. It targets MAP_BASE, the byte the
    // put_pixel above wrote successfully; releasing the capability unmapped it,
    // so the store takes a #PF that the kernel turns into termination.
    unsafe {
        (MAP_BASE as *mut u8).write_volatile(0xFF);
    }

    // Not reached. If it ever is, the mapping outlived the capability and the
    // invariant this demo exists to prove has been broken -- so say so loudly
    // rather than exiting 0 and looking like a pass.
    sys_write(b"gfxrevoke: NOT revoked -- mapping outlived its capability\n");
    sys_exit(3)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
