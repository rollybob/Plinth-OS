//! Async block-storage demo (Stage 3): depth made observable.
//!
//! Where blk-user does one blocking read, this issues several reads that overlap
//! on the device through the libos reference executor (a futures executor over
//! the kernel's completion rings), then awaits them all. The kernel posts the
//! completions in whatever order the device finishes them and demuxes each back
//! to its request by the user_data cookie; the executor matches each completion
//! to its future. Correctness is asserted, never transcript-matched (the
//! completion order is the device's to choose): every read must succeed and each
//! request's sector must have landed in its OWN frame.
//!
//! The kernel grants this process a BlockRange over device 0 sectors [0, N).
//! Reading relative sector i is disk sector i, whose ramp byte j is
//! (i + j) & 0xFF (the xtask image fills byte at offset k with (k/512+k%512)).

#![no_std]
#![no_main]

use libos::ring;
use libplinth::{
    sys_exit, sys_frame_alloc, sys_frame_map, sys_write, write_dec, BLK_OK, BLOCK_SLOT, MAP_BASE,
    PAGE_SIZE, SYS_ERR,
};

/// Concurrent reads (one sector each, one frame each). Must match the BlockRange
/// count the kernel grants this demo (main.rs).
const N: usize = 4;

/// Offsets within a sector to spot-check against the ramp.
const PROBES: [usize; 4] = [0, 1, 7, 511];

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_write(b"asyncblk: start\n");

    if !ring::init() {
        sys_write(b"asyncblk: ring init failed\n");
        sys_exit(1);
    }

    // One frame per read: allocate, map, and remember both the cap slot (to name
    // the read's destination) and the VA (to verify the bytes the device DMA'd
    // in). Frames grow up from MAP_BASE, well clear of the ring frames.
    let mut slots = [0u64; N];
    let mut vas = [0u64; N];
    for i in 0..N {
        let slot = sys_frame_alloc();
        if slot == SYS_ERR {
            sys_exit(2);
        }
        let va = MAP_BASE + i as u64 * PAGE_SIZE;
        if sys_frame_map(slot, va) == SYS_ERR {
            sys_exit(3);
        }
        slots[i] = slot;
        vas[i] = va;
    }

    // Issue N reads that overlap: relative sector i -> frame i. They are all
    // enqueued on the first poll and posted in a single doorbell, so they are in
    // flight on the device together; join awaits every one.
    let status = ring::block_on(ring::join([
        ring::read(BLOCK_SLOT, slots[0], 0, 1),
        ring::read(BLOCK_SLOT, slots[1], 1, 1),
        ring::read(BLOCK_SLOT, slots[2], 2, 1),
        ring::read(BLOCK_SLOT, slots[3], 3, 1),
    ]));

    // Assert: every read OK, and each frame holds ITS sector's ramp bytes (so no
    // completion was routed to the wrong frame).
    let mut ok = true;
    for i in 0..N {
        if status[i] != BLK_OK {
            ok = false;
        }
        let buf = vas[i] as *const u8;
        for &j in PROBES.iter() {
            let expect = ((i as u64 + j as u64) & 0xFF) as u8;
            // SAFETY: frame i is mapped at vas[i] and the device DMA'd a sector in.
            let got = unsafe { buf.add(j).read_volatile() };
            if got != expect {
                ok = false;
            }
        }
    }

    if ok {
        sys_write(b"asyncblk: ok (");
        write_dec(N as u64);
        sys_write(b" reads overlapped, each frame verified)\n");
        sys_exit(0);
    } else {
        sys_write(b"asyncblk: FAIL\n");
        sys_exit(4);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
