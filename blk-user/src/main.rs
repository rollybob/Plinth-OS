//! Block-storage demo (Stage 2): the exokernel multiplexing surface in action.
//!
//! The kernel grants this process a BlockRange capability naming a sub-range of
//! the disk (at BLOCK_SLOT). The process allocates and maps a frame, reads a
//! sector through the range capability -- verifying the bytes against the known
//! image -- and then deliberately reads one sector PAST its range, which the
//! kernel must reject. That rejection is the multiplexing guarantee: a
//! BlockRange holder cannot reach blocks outside its grant.

#![no_std]
#![no_main]

use libplinth::{
    sys_block_read, sys_exit, sys_frame_alloc, sys_frame_map, sys_write, write_dec, BLK_E_RANGE,
    BLK_OK, BLOCK_SLOT, MAP_BASE, SYS_ERR,
};

/// Sectors the granted range spans (kernel grants count = 4). Reading at this
/// relative offset is one past the end -- the out-of-range probe.
const RANGE_COUNT: u64 = 4;

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_write(b"blk: start\n");

    // A frame to receive the block into, mapped so we can read the bytes back.
    let frame = sys_frame_alloc();
    if frame == SYS_ERR {
        sys_exit(1);
    }
    let va = MAP_BASE;
    if sys_frame_map(frame, va) == SYS_ERR {
        sys_exit(2);
    }

    // In-range read: sector offset 0 of our granted range, one sector.
    if sys_block_read(BLOCK_SLOT, frame, 0, 1) != BLK_OK {
        sys_write(b"blk: in-range read failed\n");
        sys_exit(3);
    }

    // Verify against the image ramp (disk byte at absolute offset i is
    // (sector + i_within_sector) & 0xFF). Our range starts at disk sector 1, so
    // a relative-offset-0 read is disk sector 1 -> byte j == (1 + j) & 0xFF.
    let buf = va as *const u8;
    // SAFETY: the frame is mapped at `va` and the device just DMA'd a sector in.
    let (b0, b1, b5) = unsafe {
        (
            buf.read_volatile(),
            buf.add(1).read_volatile(),
            buf.add(5).read_volatile(),
        )
    };
    sys_write(b"blk: read ok b0=");
    write_dec(b0 as u64);
    sys_write(b" b1=");
    write_dec(b1 as u64);
    sys_write(b" b5=");
    write_dec(b5 as u64);
    sys_write(b"\n");

    // Out-of-range read: offset RANGE_COUNT is one sector past our grant. The
    // kernel must reject it with BLK_E_RANGE -- the multiplexing guarantee.
    if sys_block_read(BLOCK_SLOT, frame, RANGE_COUNT, 1) == BLK_E_RANGE {
        sys_write(b"blk: out-of-range rejected\n");
    } else {
        sys_write(b"blk: out-of-range NOT rejected\n");
        sys_exit(4);
    }

    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
