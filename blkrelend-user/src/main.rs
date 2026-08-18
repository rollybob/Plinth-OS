//! Re-lend chain, the ROOT (D6 slice 4, step 2 / K-025).
//! `blkreclaim-user` proved a lent `BlockRange` comes home when its direct
//! borrower dies; this proves it comes home to the RIGHT process when the borrower
//! passes it on and the *grandchild* dies -- the case K-025 got wrong.
//!
//! The chain is A -> B -> C:
//!
//! - **A (this crate)** is granted a read `BlockRange` over dev 0 sectors
//!   `[20, 24)`. It reads a sector to prove the range is its, lends the range to
//!   B via `sys_spawn`, and waits.
//! - **B (`blkrelendmid-user`)** receives the range, reads a sector of its own,
//!   and RE-LENDS it to C -- passing on a capability it does not own outright.
//! - **C (`blkreclaimchild-user`, reused)** receives the range, reads a sector,
//!   and FAULTS holding it.
//!
//! When C dies the kernel reclaims the range to its ROOT lender, named by the
//! capability's `origin`. `origin` must still name **A**, not B: D8 ruled that
//! `origin` is the root lender and survives a hop, and `blkrelendmid` re-lends
//! without being the outright owner, so it must not become the origin. K-025 was
//! the spawn-transfer path (`syscall.rs` `spawn_scheduled`) overwriting `origin`
//! with the caller unconditionally, which laundered A's claim: with the bug, C's
//! death sends the range to B, and A -- the real owner -- is left with nothing.
//!
//! **The load-bearing assertion is here: A gets the range back at BLOCK_SLOT.**
//! The kernel records the landing on the LENDER's own scheduler slot
//! (D7), so A learns it on its next `recv_cap` even though A is
//! blocked on B, not on the C that died. Watched failing by reverting the K-025
//! fix in `spawn_scheduled` (preserve-origin -> unconditional overwrite): the
//! range then homes to B, A's wake carries `NO_CAP`, and the "no landing slot"
//! exit turns smoke red. `blkrelendmid` asserts the mirror image from B's side.
//!
//! Raw syscalls (`sys_spawn` + `sys_recv_cap`), like `fbreclaim`/`blkreclaim`, so
//! a failure stays attributable to the kernel rather than to a helper.

#![no_std]
#![no_main]

use libplinth::{
    sys_block_read, sys_exit, sys_frame_alloc, sys_frame_map, sys_recv_cap, sys_spawn, sys_write,
    write_dec, BLK_OK, BLOCK_SLOT, IPC_PEER_DIED, MAP_BASE, NO_CAP, SYS_ERR,
};

/// `blkrelendmid-user`'s id in the kernel's SPAWNABLE table (appended after
/// `blkreclaimchild` at id 7). Positional; mirrored nowhere else, since only A
/// spawns B.
const BLKRELENDMID_ID: u64 = 8;

/// First disk sector of the granted range (dev 0 `[20, 24)`). Ramp byte j of
/// sector s is `(s + j) & 0xFF`, so a relative-0 read is disk sector 20 -> byte
/// 0 == 20, relative-1 is disk 21 -> 21. Distinct from `blkreclaim`'s `[16, 20)`
/// so the two demos never contend for a range.
const RANGE_START: u64 = 20;

fn read_first_byte(slot: u64, frame: u64, sector: u64, code: u64) -> u64 {
    if sys_block_read(slot, frame, sector, 1) != BLK_OK {
        sys_write(b"blkrelend: block_read failed\n");
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
    let frame = sys_frame_alloc();
    if frame == SYS_ERR {
        sys_write(b"blkrelend: frame_alloc failed\n");
        sys_exit(1);
    }
    if sys_frame_map(frame, MAP_BASE) == SYS_ERR {
        sys_write(b"blkrelend: frame_map failed\n");
        sys_exit(2);
    }

    // 1) The range is ours: read relative sector 0 (disk 20) and check the ramp.
    let b0 = read_first_byte(BLOCK_SLOT, frame, 0, 3);
    if b0 != RANGE_START & 0xFF {
        emit_byte(b"blkrelend: wrong pre-lend ramp byte b0=", b0);
        sys_exit(4);
    }
    emit_byte(b"blkrelend: root lent b0=", b0);

    // 2) Lend the range to B, the intermediary. This reserves A's slot (A is the
    //    outright owner), so the range has a home to come back to.
    let handle = sys_spawn(BLKRELENDMID_ID, BLOCK_SLOT);
    if handle == SYS_ERR {
        sys_write(b"blkrelend: spawn middle failed\n");
        sys_exit(5);
    }

    // 3) Wait for B. B re-lends to C, C dies holding the range, the kernel
    //    reclaims it to the ROOT (A) named by the surviving origin, and B then
    //    exits -- so this wake is IPC_PEER_DIED and carries A's landing slot.
    let (status, _msg, cap_slot) = sys_recv_cap(handle);
    if status != IPC_PEER_DIED {
        sys_write(b"blkrelend: expected the middle to exit\n");
        sys_exit(6);
    }
    if cap_slot == NO_CAP {
        // K-025's failure, from the root's side: the range was laundered to B, so
        // nothing came home to A. Loud, not a silent pass.
        sys_write(b"blkrelend: no landing slot -- the range was laundered, not returned to root\n");
        sys_exit(7);
    }
    // The claim survived the hop (D8): the range came home to the ROOT's reserved
    // slot, not to the intermediary. With the K-025 fix reverted this is NO_CAP
    // above; with the reservation disabled it is some other slot.
    if cap_slot != BLOCK_SLOT {
        emit_byte(b"blkrelend: range did not come home to root BLOCK_SLOT, landed at ", cap_slot);
        sys_exit(8);
    }
    emit_byte(b"blkrelend: grandchild died, range came home to root at slot ", cap_slot);

    // 4) It works: read a DIFFERENT sector (relative 1 -> disk 21) through the
    //    returned capability, proving the reclaimed range's arithmetic is intact.
    let b1 = read_first_byte(cap_slot, frame, 1, 9);
    if b1 != (RANGE_START + 1) & 0xFF {
        emit_byte(b"blkrelend: wrong post-reclaim ramp byte b0=", b1);
        sys_exit(10);
    }
    emit_byte(b"blkrelend: root reclaimed b0=", b1);

    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
