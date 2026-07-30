//! Framebuffer reclamation demo (Design/cap_reclaim.md D6) -- the dying half.
//!
//! Spawned by `fbreclaim-user`, which TRANSFERS it the whole-screen framebuffer
//! capability and then waits. This process maps the screen, draws through the
//! mapping to prove it really holds it, and then FAULTS while still holding it.
//!
//! It dies by fault rather than by a clean exit on purpose, because that is the
//! case the milestone exists for: a process that exits cleanly can be expected
//! to hand the screen back, and `shellapp-user` already shows that path. The
//! failure this demo is about is the one nobody can be asked to handle -- a
//! crash. Before reclamation, the capability died with the process, and since no
//! syscall mints a framebuffer, nothing in userspace could draw again for the
//! rest of the boot.
//!
//! The draw before the fault is load-bearing, the same way it is in
//! `gfxrevoke-user`. Without it the parent redrawing afterwards would prove only
//! that the parent has *a* framebuffer capability, not that the one it lent out
//! survived the borrower's death -- and a `fb_map` that silently did nothing
//! would pass just as well.

#![no_std]
#![no_main]

use libgfx::Framebuffer;
use libplinth::{sys_exit, sys_write, MAP_BASE, SPAWN_GRANT_SLOT};

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    let fb = match Framebuffer::map(SPAWN_GRANT_SLOT, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(b"fbreclaimchild: map failed\n");
            sys_exit(1);
        }
    };

    // Prove the mapping is live while we hold it. `put_pixel`'s stores are
    // volatile and go straight into the mapped pixels with the kernel off the
    // path, so reaching the next line means these pages really are present and
    // writable here -- this process genuinely has the screen.
    fb.put_pixel(0, 0, 0x20, 0x40, 0x80);
    sys_write(b"fbreclaimchild: holding the screen, now faulting\n");

    // SAFETY: deliberately invalid. Page 0 is unmapped and no fault handler is
    // registered, so the kernel terminates this process here -- while the
    // framebuffer capability is still in its table, which is the whole point.
    unsafe {
        core::ptr::null_mut::<u64>().write_volatile(0xdead_beef);
    }

    // Not reached. Only a safety net: if the fault did not terminate us, the
    // demo's premise is wrong and the parent's result would be meaningless.
    sys_write(b"fbreclaimchild: still alive -- did not fault\n");
    sys_exit(2)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
