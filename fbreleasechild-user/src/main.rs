//! Framebuffer voluntary-release demo (cap_release-on-reserved) -- the borrower.
//!
//! Spawned by `fbrelease-user`, which TRANSFERS it the whole-screen framebuffer
//! capability and then waits. This process maps the screen, draws through the
//! mapping to prove it really holds it, and then VOLUNTARILY RELEASES it with
//! `cap_release` -- the polite counterpart to `fbreclaimchild`, which faults.
//!
//! The two children are the two ways a borrower can stop holding a lent screen.
//! `fbreclaimchild` dies; the kernel's death path reclaims the capability to the
//! lender. This one lives and hands it back on purpose. Before the
//! cap_release-on-reserved ruling (2026-08-15), a voluntary release unmapped and
//! DROPPED the capability, stranding the lender's reserved slot and bricking the
//! borrowed screen -- so a well-behaved borrower was worse for the lender than a
//! crashing one. Now the release routes the capability home exactly as a death
//! does, and the lender finds it back in the slot it lent from.
//!
//! The draw before the release is load-bearing, the same way it is in
//! `fbreclaimchild` and `gfxrevoke-user`: without it, the parent redrawing
//! afterwards would prove only that the parent has *a* framebuffer capability,
//! not that the one it lent out came home when this process released it.

#![no_std]
#![no_main]

use libgfx::Framebuffer;
use libplinth::{sys_cap_release, sys_exit, sys_send, sys_write, ENDPOINT_SLOT, MAP_BASE, SPAWN_GRANT_SLOT};

/// Sent to the parent once the screen has been released home, so the parent
/// wakes on a clean rendezvous rather than on peer-death. The value is a marker;
/// the parent's proof is that it can re-map the slot it lent from, not this.
const DONE: u64 = 1;

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    let fb = match Framebuffer::map(SPAWN_GRANT_SLOT, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(b"fbreleasechild: map failed\n");
            sys_exit(1);
        }
    };

    // Prove the mapping is live while we hold it -- a volatile store straight into
    // the mapped pixels, kernel off the path. Reaching the next line means these
    // pages really are present and writable here: this process genuinely has the
    // screen.
    fb.put_pixel(0, 0, 0x20, 0x40, 0x80);
    sys_write(b"fbreleasechild: holding the screen, now releasing\n");

    // Voluntarily give the borrowed screen back. On the fixed kernel this sends
    // the capability home to the lender's reserved slot (and unmaps it here); on
    // the old kernel it unmapped and dropped it, so the lender's slot stayed
    // stranded and its re-map below failed. Release BEFORE signalling the parent,
    // so the capability is home by the time the parent wakes and re-maps.
    if sys_cap_release(SPAWN_GRANT_SLOT) != 0 {
        sys_write(b"fbreleasechild: release failed\n");
        sys_exit(2);
    }
    sys_write(b"fbreleasechild: released the screen, exiting\n");

    // Tell the parent the screen is home. The kernel gave every spawned child a
    // send capability to its parent's result channel at ENDPOINT_SLOT (see
    // grantee-user); the parent is waiting on the matching receive end.
    sys_send(ENDPOINT_SLOT, DONE);
    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
