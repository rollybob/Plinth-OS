//! Block-range reclamation demo (D6, slice 4) -- the
//! lending half. The first non-framebuffer lender in the tree, and the reason
//! slice 4 needs one: until now every lender lent the screen, so nothing
//! exercised reclamation for a `BlockRange` or proved the homecoming reservation
//! is not framebuffer-specific.
//!
//! This process is granted a read `BlockRange` over dev 0 sectors `[16, 20)`,
//! reads a sector through it to prove the range is genuinely ours, TRANSFERS it
//! to a child that reads a sector of its own and then FAULTS while holding it,
//! and finally reads a sector through the capability the kernel returned -- to
//! the slot it was lent from.
//!
//! **The load-bearing assertion is `cap_slot == BLOCK_SLOT`.** A lent capability
//! comes home to the slot it left from, because that slot was reserved the moment
//! the loan started (D2(D), slice 2). Slice 2 proved this for a
//! `Framebuffer`; this demo proves the same for a `BlockRange`, which is only
//! reclaimable at all because slice 4 widened `capability::is_reclaimable_kind`
//! to `Framebuffer | BlockRange | EventSource`. Watched failing two ways, each a
//! single line of kernel change reverted:
//!
//! - **Revert the `is_reclaimable_kind` widening.** A `BlockRange` is no longer a
//!   reclaimable kind, so `lend_reserves_home` declines and `release_action`
//!   sends it to `DropSlot`. The death-wake carries `NO_CAP`, the "no landing
//!   slot" exit fires, and smoke goes red. This is the state of the tree before
//!   slice 4.
//! - **Disable the reservation** in `process::revoke_and_unmap_for_lend` (as slice
//!   2's watch does). The range still comes home, but to slot 2 -- the first free
//!   slot -- rather than the reserved BLOCK_SLOT, and the `cap_slot == BLOCK_SLOT`
//!   assertion turns smoke red.
//!
//! Unlike the framebuffer demos there is no mapping and so no two-colour-hash
//! trick: a `BlockRange` is inline data read by `block_read`, the kernel DMAs a
//! fresh sector every call, and there is no stale mapping that could masquerade as
//! a live one. The reserved-slot number is the proof, and the post-reclaim read of
//! a *different* sector (relative 1, disk 17) confirms the returned capability's
//! range arithmetic still works rather than merely occupying a slot.
//!
//! It drives the RAW syscalls (`sys_spawn` + `sys_recv_cap`), not the
//! `spawn_and_wait_cap` helper, for the same reason `fbreclaim-user` does: keeping
//! one lender on the bare kernel path makes a failure attributable to the kernel
//! rather than to the helper library.

#![no_std]
#![no_main]

use libplinth::{
    sys_block_read, sys_exit, sys_frame_alloc, sys_frame_map, sys_recv_cap, sys_spawn, sys_write,
    write_dec, BLK_OK, BLOCK_SLOT, IPC_PEER_DIED, MAP_BASE, NO_CAP, SYS_ERR,
};

/// `blkreclaimchild-user`'s id in the kernel's SPAWNABLE table (appended after
/// `fbreleasechild` at id 6). Positional, mirrored the way `fbreclaim-user`
/// mirrors `FBRECLAIMCHILD_ID = 5`.
const BLKRECLAIMCHILD_ID: u64 = 7;

/// First disk sector of the granted range (dev 0 `[16, 20)`, see main.rs). The
/// ramp disk's byte j of sector s is `(s + j) & 0xFF`, so a relative-0 read is
/// disk sector 16 -> byte 0 == 16, and a relative-1 read is disk sector 17 ->
/// byte 0 == 17. Mirrored in `blkreclaimchild-user`.
const RANGE_START: u64 = 16;

/// Read relative `sector` (one sector) through the range at `slot` into the frame
/// mapped at MAP_BASE, and return its first ramp byte. Exits with `code` if the
/// read fails.
fn read_first_byte(slot: u64, frame: u64, sector: u64, code: u64) -> u64 {
    if sys_block_read(slot, frame, sector, 1) != BLK_OK {
        sys_write(b"blkreclaim: block_read failed\n");
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
    // A frame to receive sectors into, mapped for read-back. Held across the loan
    // -- only the BlockRange is transferred, not this frame.
    let frame = sys_frame_alloc();
    if frame == SYS_ERR {
        sys_write(b"blkreclaim: frame_alloc failed\n");
        sys_exit(1);
    }
    if sys_frame_map(frame, MAP_BASE) == SYS_ERR {
        sys_write(b"blkreclaim: frame_map failed\n");
        sys_exit(2);
    }

    // 1) The range is ours: read relative sector 0 (disk 16) and check the ramp.
    let b0 = read_first_byte(BLOCK_SLOT, frame, 0, 3);
    if b0 != RANGE_START & 0xFF {
        emit_byte(b"blkreclaim: wrong pre-lend ramp byte b0=", b0);
        sys_exit(4);
    }
    emit_byte(b"blkreclaim: lent b0=", b0);

    // 2) Lend the range to a child that will die holding it. The spawn transfer
    //    revokes the capability here and reserves the slot it left (slice 2), so
    //    from here until the child dies this process cannot read through it.
    let handle = sys_spawn(BLKRECLAIMCHILD_ID, BLOCK_SLOT);
    if handle == SYS_ERR {
        sys_write(b"blkreclaim: spawn failed\n");
        sys_exit(5);
    }

    // 3) Wait. The child faults, so the wake is IPC_PEER_DIED, and since the
    //    slice-4 widening a reclaimable BlockRange carries a real landing slot.
    let (status, _msg, cap_slot) = sys_recv_cap(handle);
    if status != IPC_PEER_DIED {
        sys_write(b"blkreclaim: expected a dead child\n");
        sys_exit(6);
    }
    if cap_slot == NO_CAP {
        // The pre-slice-4 answer: a BlockRange was not a reclaimable kind, so it
        // was dropped with the child and the wake reported nothing. Stated loudly
        // rather than exiting 0 and looking like a pass.
        sys_write(b"blkreclaim: no landing slot -- the death-wake reported nothing\n");
        sys_exit(7);
    }
    // The homecoming guarantee, asserted directly (D2(D)): the
    // range returns to the slot it was lent from, because that slot was reserved
    // the moment the loan started. With the reservation disabled the kernel picks
    // the first free slot (slot 2) instead, and this fires.
    if cap_slot != BLOCK_SLOT {
        emit_byte(b"blkreclaim: range did not come home to BLOCK_SLOT, landed at ", cap_slot);
        sys_exit(8);
    }
    emit_byte(b"blkreclaim: child died, range came back at slot ", cap_slot);

    // 4) The range is ours again. Read a DIFFERENT relative sector (1 -> disk 17)
    //    through the returned capability, so a returned-but-broken range is caught:
    //    the arithmetic must still map relative 1 to disk 17.
    let b1 = read_first_byte(cap_slot, frame, 1, 9);
    if b1 != (RANGE_START + 1) & 0xFF {
        emit_byte(b"blkreclaim: wrong post-reclaim ramp byte b0=", b1);
        sys_exit(10);
    }
    emit_byte(b"blkreclaim: reclaimed b0=", b1);

    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
