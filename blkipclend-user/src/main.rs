//! IPC blocked-sender reclamation (Design/lender_owed.md D6 slice 4 / K-026).
//! The `blkreclaim`/`blkrelend` chains lend over `sys_spawn`; this one lends over
//! IPC `send_cap`, and forces the give to take the **blocked-sender** branch --
//! the one path K-026 left un-reserved.
//!
//! K-026: a capability transfer has two give-side paths, chosen by who reached the
//! rendezvous first. `transfer_current_to_blocked` (sender running, receiver
//! already waiting) always reserved a homecoming slot; `transfer_blocked_to_current`
//! (sender blocked first, receiver arrives later) did not, until `6b68dd5`. Arrival
//! order is a scheduling accident and must not decide whether a loan is reservable.
//!
//! This process (the SENDER, S) owns a read `BlockRange` over dev 0 `[24, 28)`
//! outright. It:
//!
//!   1. spawns the receiver R (`blkrecvchild-user`), handing it the RECV end of a
//!      shared endpoint the kernel granted us, and
//!   2. immediately `send_cap`s the range on the SEND end.
//!
//! Because a spawned child is homed to the spawning core, R cannot run until we
//! yield -- so our `send_cap` reaches the endpoint with **no receiver waiting** and
//! blocks. R then runs, `recv_cap`s, and the give takes `transfer_blocked_to_current`
//! with us as the blocked sender. That is the path under test, reached
//! deterministically rather than by racing the scheduler.
//!
//! R reads a sector to prove it holds the range, then faults. The range must come
//! home to OUR reserved slot -- `BLOCK_SLOT`, the slot we lent from -- exactly as it
//! would had we lent over spawn. We learn the landing on R's death-wake, delivered
//! through the spawn result handle (the reclaim is keyed on our lender slot, D7).
//!
//! **Load-bearing assertion: the range comes home at `BLOCK_SLOT`.** Watched failing
//! by reverting `6b68dd5` (`scheduler::revoke_from_blocked` back to plain
//! `revoke_and_unmap`): the blocked-sender give reserves nothing, the range comes
//! home to a first-free slot instead, and `cap_slot == BLOCK_SLOT` turns smoke red.
//! Reaching this at all also depends on slice 4's widening -- a `BlockRange` was not
//! reclaimable before it.

#![no_std]
#![no_main]

use libplinth::{
    sys_block_read, sys_exit, sys_frame_alloc, sys_frame_map, sys_recv_cap, sys_send_cap,
    sys_spawn, sys_write, write_dec, BLK_OK, BLOCK_SLOT, IPC_PEER_DIED, MAP_BASE, NO_CAP, SYS_ERR,
};

/// `blkrecvchild-user`'s id in the SPAWNABLE table (appended after
/// `blkrelendmid` at id 8).
const BLKRECVCHILD_ID: u64 = 9;

/// The kernel grants this process three capabilities, minted into consecutive
/// slots after the CPU budget: the `BlockRange` at slot 1 (`BLOCK_SLOT`), a SEND
/// cap to the shared endpoint at slot 2, and a RECV cap to the same endpoint at
/// slot 3. We keep SEND and hand RECV to the child.
const SEND_EP_SLOT: u64 = 2;
const RECV_EP_SLOT: u64 = 3;

/// First disk sector of the granted range (dev 0 `[24, 28)`, clear of every other
/// demo's range). Ramp byte j of sector s is `(s + j) & 0xFF`, so relative 0 is
/// disk 24 -> byte 24, relative 1 is disk 25 -> byte 25.
const RANGE_START: u64 = 24;

/// Message word sent alongside the capability; the receiver ignores it.
const TAG: u64 = 0;

fn read_first_byte(slot: u64, frame: u64, sector: u64, code: u64) -> u64 {
    if sys_block_read(slot, frame, sector, 1) != BLK_OK {
        sys_write(b"blkipclend: block_read failed\n");
        sys_exit(code);
    }
    // SAFETY: the frame is mapped at MAP_BASE and the device just DMA'd a sector.
    unsafe { (MAP_BASE as *const u8).read_volatile() as u64 }
}

fn emit_byte(tag: &[u8], b: u64) {
    sys_write(tag);
    write_dec(b);
    sys_write(b"\n");
}

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    // 1) Spawn the receiver, handing it the RECV end of the shared endpoint. It is
    //    homed to this core, so it cannot run until we block in the send below.
    let handle = sys_spawn(BLKRECVCHILD_ID, RECV_EP_SLOT);
    if handle == SYS_ERR {
        sys_write(b"blkipclend: spawn receiver failed\n");
        sys_exit(1);
    }

    // 2) Send the range on the SEND end. No receiver is waiting (the child has not
    //    run yet), so this blocks -- and when the child receives, the give takes
    //    the blocked-sender branch (transfer_blocked_to_current). That reserves
    //    BLOCK_SLOT if K-026 is fixed, and revokes it either way.
    let sstatus = sys_send_cap(SEND_EP_SLOT, TAG, BLOCK_SLOT);
    if sstatus == u64::MAX {
        sys_write(b"blkipclend: send_cap failed\n");
        sys_exit(2);
    }

    // 3) Allocate the read frame AFTER the send, on purpose -- this is what makes
    //    the reservation observable. The send just revoked the range from
    //    BLOCK_SLOT. If K-026 reserved that slot, this frame skips it (install
    //    passes over a reserved slot) and the range reclaims home to the reserved
    //    BLOCK_SLOT. If the reservation is missing (the bug), BLOCK_SLOT is merely
    //    free, so this frame CLAIMS it, and the reclaimed range is forced to a
    //    different slot -- which the assertion below catches. Allocating before the
    //    send would leave BLOCK_SLOT free in both cases and the check would pass
    //    vacuously (the A-12 trap: a green demo that tests nothing).
    let frame = sys_frame_alloc();
    if frame == SYS_ERR {
        sys_write(b"blkipclend: frame_alloc failed\n");
        sys_exit(3);
    }
    if sys_frame_map(frame, MAP_BASE) == SYS_ERR {
        sys_write(b"blkipclend: frame_map failed\n");
        sys_exit(4);
    }

    // 4) The send has completed (the child took the range). Wait for the child to
    //    die holding it: the death-wake on the spawn result handle carries the slot
    //    the range came home to.
    let (status, _msg, cap_slot) = sys_recv_cap(handle);
    if status != IPC_PEER_DIED {
        sys_write(b"blkipclend: expected the receiver to die holding the range\n");
        sys_exit(5);
    }
    if cap_slot == NO_CAP {
        sys_write(b"blkipclend: no landing slot -- the blocked-sender lend was not reclaimed\n");
        sys_exit(6);
    }
    // The homecoming guarantee for the blocked-sender path: the range returns to
    // the slot it was lent from, because that slot was reserved when the child
    // received it -- regardless of arrival order. With K-026 reverted the frame
    // above took BLOCK_SLOT and this lands elsewhere.
    if cap_slot != BLOCK_SLOT {
        emit_byte(b"blkipclend: range did not come home to BLOCK_SLOT, landed at ", cap_slot);
        sys_exit(7);
    }
    emit_byte(b"blkipclend: receiver died, range came home to sender at slot ", cap_slot);

    // 5) Read a sector through the returned capability, proving the reclaimed range
    //    still works (relative 0 -> disk 24 -> ramp byte 24).
    let b0 = read_first_byte(cap_slot, frame, 0, 8);
    if b0 != RANGE_START & 0xFF {
        emit_byte(b"blkipclend: wrong post-reclaim ramp byte b0=", b0);
        sys_exit(9);
    }
    emit_byte(b"blkipclend: reclaimed b0=", b0);

    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
