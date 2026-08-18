//! Block-range reclamation demo (Design/lender_owed.md D6, slice 4) -- the dying
//! half. The `BlockRange` counterpart to `fbreclaimchild-user`.
//!
//! Spawned by `blkreclaim-user`, which TRANSFERS it a read capability over a
//! `BlockRange` and then waits. This process reads a sector THROUGH that range
//! to prove it genuinely holds it, and then FAULTS while still holding it.
//!
//! It dies by fault rather than by a clean exit for the same reason
//! `fbreclaimchild-user` does: a clean exit could be expected to hand the
//! capability back, and the failure this milestone is about is the one nobody
//! can be asked to handle -- a crash. Before slice 4 widened `is_reclaimable_kind`
//! beyond `Framebuffer`, a `BlockRange` lent to a dying child was simply DROPPED
//! with the child (its `release_action` is `DropSlot`), so the death-wake carried
//! `NO_CAP` and the lender got nothing back.
//!
//! The read before the fault is load-bearing, exactly as the draw is in
//! `fbreclaimchild-user`. Without it the parent re-reading afterwards would prove
//! only that the parent holds *a* `BlockRange`, not that the one it lent out
//! survived the borrower's death -- and a `block_read` that silently did nothing
//! would pass just as well. Reading a real sector and checking its ramp byte is
//! what makes "this process genuinely has the range" observable.

#![no_std]
#![no_main]

use libplinth::{
    sys_block_read, sys_exit, sys_frame_alloc, sys_frame_map, sys_write, write_dec, BLK_OK,
    MAP_BASE, SPAWN_GRANT_SLOT, SYS_ERR,
};

/// First disk sector of the range `blkreclaim-user` lends (dev 0, `[16, 20)`).
/// The ramp disk's byte j of sector s is `(s + j) & 0xFF` (xtask `block_image`),
/// so a relative-offset-0 read is disk sector 16 and byte 0 must be 16. Mirrored
/// from `blkreclaim-user`'s `RANGE_START`; the two must agree on what a correct
/// read looks like.
const RANGE_START: u64 = 16;

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    // A frame to receive the sector into, mapped so we can read the bytes back.
    let frame = sys_frame_alloc();
    if frame == SYS_ERR {
        sys_write(b"blkreclaimchild: frame_alloc failed\n");
        sys_exit(1);
    }
    if sys_frame_map(frame, MAP_BASE) == SYS_ERR {
        sys_write(b"blkreclaimchild: frame_map failed\n");
        sys_exit(2);
    }

    // Prove the range is genuinely ours: read relative sector 0 (disk sector 16)
    // through the TRANSFERRED capability at SPAWN_GRANT_SLOT and check the ramp
    // byte. A read that reaches the device and returns the right byte means this
    // process really holds the range -- the kernel is off the verification path
    // once the bytes are in our frame.
    if sys_block_read(SPAWN_GRANT_SLOT, frame, 0, 1) != BLK_OK {
        sys_write(b"blkreclaimchild: in-range read failed\n");
        sys_exit(3);
    }
    // SAFETY: the frame is mapped at MAP_BASE and the device just DMA'd a sector.
    let b0 = unsafe { (MAP_BASE as *const u8).read_volatile() };
    if b0 as u64 != RANGE_START & 0xFF {
        sys_write(b"blkreclaimchild: wrong ramp byte -- range is not ours\n");
        sys_exit(4);
    }
    sys_write(b"blkreclaimchild: holding the range b0=");
    write_dec(b0 as u64);
    sys_write(b", now faulting\n");

    // SAFETY: deliberately invalid. Page 0 is unmapped and no fault handler is
    // registered, so the kernel terminates this process here -- while the
    // BlockRange capability is still in its table, which is the whole point.
    unsafe {
        core::ptr::null_mut::<u64>().write_volatile(0xdead_beef);
    }

    // Not reached. Only a safety net: if the fault did not terminate us, the
    // demo's premise is wrong and the parent's result would be meaningless.
    sys_write(b"blkreclaimchild: still alive -- did not fault\n");
    sys_exit(5)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
