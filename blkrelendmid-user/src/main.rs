//! Re-lend chain, the INTERMEDIARY (D6 slice 4, step 2 /
//! K-025). Spawned by `blkrelend-user` (A), which transfers it a read `BlockRange`
//! it does NOT own outright -- the range is on loan from A. This process reads a
//! sector to prove it holds the range, then RE-LENDS it to a grandchild
//! (`blkreclaimchild-user`, C) that reads a sector and faults holding it.
//!
//! The point of this crate is what it must NOT observe. Because the range is on
//! loan (its `origin` names A), re-lending it must leave the origin naming A:
//! `capability::lend_reserves_home` declines to reserve here (this process is not
//! the outright owner), and `spawn_scheduled` must PRESERVE the existing origin
//! rather than overwrite it with this process (K-025). So when C dies, the range
//! goes home to A, and this process's wait on C returns `NO_CAP` -- nothing comes
//! back here. If it DID come back here, A's claim was laundered: that is K-025,
//! and this crate's `NO_CAP` assertion is the mirror of A's `BLOCK_SLOT` one.
//!
//! It exits cleanly rather than sending a result, so A's wait on it returns
//! `IPC_PEER_DIED` carrying A's landing slot (the reclaim keyed on A's lender
//! slot, delivered on A's next wake).

#![no_std]
#![no_main]

use libplinth::{
    sys_block_read, sys_exit, sys_frame_alloc, sys_frame_map, sys_recv_cap, sys_spawn, sys_write,
    write_dec, BLK_OK, IPC_PEER_DIED, MAP_BASE, NO_CAP, SPAWN_GRANT_SLOT, SYS_ERR,
};

/// `blkreclaimchild-user`'s id in the SPAWNABLE table -- the grandchild C, reused
/// as the dying holder. It reads a sector through the range at its own
/// SPAWN_GRANT_SLOT and faults, which is exactly what this chain needs at the leaf.
const BLKRECLAIMCHILD_ID: u64 = 7;

/// First disk sector of the lent range (dev 0 `[20, 24)`, mirrored from
/// `blkrelend-user`). A relative-0 read is disk sector 20 -> ramp byte 0 == 20.
const RANGE_START: u64 = 20;

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    let frame = sys_frame_alloc();
    if frame == SYS_ERR {
        sys_write(b"blkrelendmid: frame_alloc failed\n");
        sys_exit(1);
    }
    if sys_frame_map(frame, MAP_BASE) == SYS_ERR {
        sys_write(b"blkrelendmid: frame_map failed\n");
        sys_exit(2);
    }

    // Prove the range genuinely reached us: read relative sector 0 (disk 20)
    // through the TRANSFERRED capability and check the ramp byte.
    if sys_block_read(SPAWN_GRANT_SLOT, frame, 0, 1) != BLK_OK {
        sys_write(b"blkrelendmid: in-range read failed\n");
        sys_exit(3);
    }
    // SAFETY: the frame is mapped at MAP_BASE and the device just DMA'd a sector.
    let b0 = unsafe { (MAP_BASE as *const u8).read_volatile() as u64 };
    if b0 != RANGE_START & 0xFF {
        sys_write(b"blkrelendmid: wrong ramp byte -- range is not ours\n");
        sys_exit(4);
    }

    // Re-lend the range to the grandchild. We are NOT its outright owner (it came
    // from A), so this reserves nothing here and must not become the range's
    // origin -- A stays the root lender.
    let handle = sys_spawn(BLKRECLAIMCHILD_ID, SPAWN_GRANT_SLOT);
    if handle == SYS_ERR {
        sys_write(b"blkrelendmid: re-lend spawn failed\n");
        sys_exit(5);
    }

    // Wait for the grandchild to die holding the range. The wake is IPC_PEER_DIED;
    // the question is the landing slot. It must be NO_CAP -- the range belongs to
    // A and goes home to A, not back to us.
    let (status, _msg, cap_slot) = sys_recv_cap(handle);
    if status != IPC_PEER_DIED {
        sys_write(b"blkrelendmid: expected a dead grandchild\n");
        sys_exit(6);
    }
    if cap_slot == NO_CAP {
        // Correct: the claim survived the hop and the range went home to the root.
        sys_write(b"blkrelendmid: re-lent, range went to root not here\n");
    } else {
        // K-025's failure, from the intermediary's side: the range was laundered
        // back to us. A deterministic, asserted line, so smoke goes red here too.
        sys_write(b"blkrelendmid: LAUNDERED -- range came back to middle at slot ");
        write_dec(cap_slot);
        sys_write(b"\n");
    }

    // Exit cleanly (no send), so A's wait on us returns IPC_PEER_DIED carrying
    // A's landing slot.
    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
