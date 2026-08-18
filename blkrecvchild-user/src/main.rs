//! IPC blocked-sender reclamation (Design/lender_owed.md D6 slice 4 / K-026) --
//! the RECEIVER/dying half. Spawned by `blkipclend-user` (S), which hands it the
//! RECV end of a shared endpoint and then `send_cap`s a `BlockRange` on the SEND
//! end. Because S sends before this process has run, S is a BLOCKED sender when
//! this process receives -- so the give takes `transfer_blocked_to_current`, the
//! path K-026 is about.
//!
//! This process receives the range over IPC, reads a sector THROUGH it to prove it
//! genuinely holds it, and then FAULTS while holding it -- so the kernel must
//! reclaim the range to S, its outright owner and lender. The read-then-fault shape
//! is the same as `blkreclaimchild-user`; the difference is only how the range
//! arrived (an IPC `recv_cap`, not a spawn transfer), which lands it at a slot the
//! kernel chooses rather than `SPAWN_GRANT_SLOT`, so this process reads the slot
//! `recv_cap` reports rather than a fixed one.

#![no_std]
#![no_main]

use libplinth::{
    sys_block_read, sys_exit, sys_frame_alloc, sys_frame_map, sys_recv_cap, sys_write, write_dec,
    BLK_OK, IPC_OK, MAP_BASE, NO_CAP, SPAWN_GRANT_SLOT, SYS_ERR,
};

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    // Receive the range on the RECV endpoint cap handed to us at SPAWN_GRANT_SLOT.
    // The sender is already blocked, so this completes at once and carries the
    // slot the range landed in.
    let (status, _msg, cap_slot) = sys_recv_cap(SPAWN_GRANT_SLOT);
    if status != IPC_OK {
        sys_write(b"blkrecvchild: recv_cap did not deliver a capability\n");
        sys_exit(1);
    }
    if cap_slot == NO_CAP {
        sys_write(b"blkrecvchild: recv_cap carried no capability\n");
        sys_exit(2);
    }

    // A frame to receive the sector into, mapped so we can read the bytes back.
    let frame = sys_frame_alloc();
    if frame == SYS_ERR {
        sys_write(b"blkrecvchild: frame_alloc failed\n");
        sys_exit(3);
    }
    if sys_frame_map(frame, MAP_BASE) == SYS_ERR {
        sys_write(b"blkrecvchild: frame_map failed\n");
        sys_exit(4);
    }

    // Prove the range is genuinely ours: read relative sector 0 through the
    // received capability. The ramp byte printed below is pinned by the caller's
    // boot-log expectation (24 for blkipclend's range), so a wrong range is caught
    // there.
    if sys_block_read(cap_slot, frame, 0, 1) != BLK_OK {
        sys_write(b"blkrecvchild: in-range read failed\n");
        sys_exit(5);
    }
    // SAFETY: the frame is mapped at MAP_BASE and the device just DMA'd a sector.
    let b0 = unsafe { (MAP_BASE as *const u8).read_volatile() };
    sys_write(b"blkrecvchild: holding the range b0=");
    write_dec(b0 as u64);
    sys_write(b", now faulting\n");

    // SAFETY: deliberately invalid. Page 0 is unmapped and no fault handler is
    // registered, so the kernel terminates this process here -- while the received
    // BlockRange is still in its table, which is the whole point.
    unsafe {
        core::ptr::null_mut::<u64>().write_volatile(0xdead_beef);
    }

    sys_write(b"blkrecvchild: still alive -- did not fault\n");
    sys_exit(6)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
