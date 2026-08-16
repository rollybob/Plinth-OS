//! Framebuffer voluntary-release demo (cap_release-on-reserved) -- the lender.
//!
//! The polite counterpart to `fbreclaim-user`. That demo proves a lent screen
//! survives the borrower's DEATH; this one proves it comes home when the borrower
//! politely RELEASES it with `cap_release`. This process is granted the whole
//! screen, draws and hashes a frame, transfers the framebuffer capability to a
//! child that releases it, and then draws and hashes a second frame through the
//! capability that came home.
//!
//! Both frames are the whole demo. The first proves the screen was genuinely ours
//! before we lent it; the second proves it is ours again afterwards. Before the
//! cap_release-on-reserved ruling (2026-08-15) the second is unreachable, not
//! merely different: a voluntary release unmapped and DROPPED the borrowed
//! capability instead of returning it, so our reserved slot was stranded empty
//! and the re-map below failed. A crashing borrower returned the screen while a
//! well-behaved one did not -- backwards, and the same "green but leaking
//! reserved slot" class the slice-2 fix closed, reached through the release verb.
//!
//! Unlike `fbreclaim-user`, no landing slot travels on the wake. A death routes
//! the capability to a first-free slot the parent cannot predict, so ABI v2.9 has
//! the death-wake carry it; a release routes it to the very slot it was lent
//! from, which the lender already knows (that is what the reservation is for). So
//! this process simply re-maps FB_SLOT -- the slot it lent -- and the child's
//! clean DONE signal is only a rendezvous, not the bearer of a slot.

#![no_std]
#![no_main]

use libgfx::Framebuffer;
use libplinth::{
    sys_exit, sys_recv_cap, sys_spawn, sys_write, write_hex, FB_SLOT, IPC_OK, MAP_BASE, SYS_ERR,
};

/// `fbreleasechild-user`'s id in the kernel's SPAWNABLE table (append-only; id 6).
const FBRELEASECHILD_ID: u64 = 6;

/// Side of the hashed square. 128 to match every other framebuffer demo (gfx,
/// gfxtext, shell, fbreclaim), so the numbers are comparable by eye in the log.
const HASH_SIDE: u32 = 128;

fn emit_hash(tag: &[u8], fb: &Framebuffer) {
    sys_write(tag);
    write_hex(fb.hash_origin_square(HASH_SIDE));
    sys_write(b"\n");
}

/// Paint a deterministic frame: a flat field plus a marker pixel at the origin,
/// so the hash is fixed and a lost mapping cannot masquerade as a matching one.
/// The two colours match `fbreclaim-user`'s exactly, so the hashes are directly
/// comparable between the death demo and this release demo.
fn paint(fb: &Framebuffer, r: u8, g: u8, b: u8) {
    let info = fb.info();
    fb.fill_rect(0, 0, info.width, info.height, r, g, b);
    fb.put_pixel(0, 0, 0xFF, 0xFF, 0xFF);
}

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    // 1) The screen is ours: map it, draw, hash.
    let fb = match Framebuffer::map(FB_SLOT, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(b"fbrelease: initial map failed\n");
            sys_exit(1);
        }
    };
    paint(&fb, 0x10, 0x10, 0x18);
    emit_hash(b"fbrelease: lent hash ", &fb);

    // 2) Lend FB_SLOT to a child that will RELEASE it. The spawn transfer revokes
    //    the capability here and unmaps it with the authority (fb_mapping D1), and
    //    RESERVES FB_SLOT for the return -- from here until the child hands it
    //    back this process cannot draw at all.
    let handle = sys_spawn(FBRELEASECHILD_ID, FB_SLOT);
    if handle == SYS_ERR {
        sys_write(b"fbrelease: spawn failed\n");
        sys_exit(2);
    }

    // 3) Wait for the child. It releases the borrowed screen -- which comes home
    //    to our reserved FB_SLOT during its life -- then signals a clean DONE.
    let (status, _msg, _cap) = sys_recv_cap(handle);
    if status != IPC_OK {
        sys_write(b"fbrelease: child did not signal a clean release\n");
        sys_exit(3);
    }
    // Only a rendezvous -- the child says it released. Whether the screen actually
    // came home is not known until the re-map below, so do not claim it yet.
    sys_write(b"fbrelease: child released the screen\n");

    // 4) Re-map OUR OWN reserved slot. The released capability came home to
    //    exactly the slot we lent it from, so no landing slot from a wake is
    //    needed. This map succeeds only if the capability came home; on the
    //    pre-ruling kernel FB_SLOT is stranded empty and it fails. Draw a
    //    DIFFERENT frame so the second hash cannot coincide with the first.
    let fb = match Framebuffer::map(FB_SLOT, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(b"fbrelease: remap after release failed -- screen did not come home\n");
            sys_exit(5);
        }
    };
    paint(&fb, 0x60, 0x48, 0x30);
    emit_hash(b"fbrelease: returned hash ", &fb);

    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
