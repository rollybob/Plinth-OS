//! Block-range reclamation demo (D6, slice 4) -- the dying
//! half. The `BlockRange` counterpart to `fbreclaimchild-user`.
//!
//! Spawned by a lender -- `blkreclaim-user` directly, or `blkrelendmid-user` as
//! the grandchild C of the A -> B -> C re-lend chain -- which TRANSFERS it a read
//! capability over a `BlockRange` and then waits. This process reads a sector
//! THROUGH that range to prove it genuinely holds it, and then FAULTS while still
//! holding it.
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
//! would pass just as well. Reading a real sector is what makes "this process
//! genuinely has the range" observable.
//!
//! It is **range-agnostic**: it reads relative sector 0 of whatever range it was
//! handed and prints the ramp byte, rather than checking against a hardcoded
//! value. That is what lets it serve as the dying holder for more than one lender
//! (`blkreclaim-user` lends `[16, 20)` -> byte 16; the re-lend chain lends
//! `[20, 24)` -> byte 20). The caller's boot-log line pins the expected byte, so a
//! wrong range still fails smoke without this crate knowing which range it holds.

#![no_std]
#![no_main]

use libplinth::{
    sys_block_read, sys_exit, sys_frame_alloc, sys_frame_map, sys_write, write_dec, BLK_OK,
    MAP_BASE, SPAWN_GRANT_SLOT, SYS_ERR,
};

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

    // Prove the range is genuinely ours: read relative sector 0 through the
    // TRANSFERRED capability at SPAWN_GRANT_SLOT. A read that reaches the device
    // and lands bytes in our frame means this process really holds the range --
    // the kernel is off the verification path once the bytes are here. The ramp
    // byte printed below is checked by the caller's boot-log expectation (16 for
    // blkreclaim's range, 20 for the re-lend chain's), so a wrong range is caught
    // there without this crate hardcoding which range it holds.
    if sys_block_read(SPAWN_GRANT_SLOT, frame, 0, 1) != BLK_OK {
        sys_write(b"blkreclaimchild: in-range read failed\n");
        sys_exit(3);
    }
    // SAFETY: the frame is mapped at MAP_BASE and the device just DMA'd a sector.
    let b0 = unsafe { (MAP_BASE as *const u8).read_volatile() };
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
