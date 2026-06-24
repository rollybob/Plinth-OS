//! Block write demo (Design/block_write.md): the write half of the ring ABI.
//!
//! The kernel grants this process a BlockRange over device 0 sectors [8, 12),
//! minted with RIGHT_WRITE instead of every other block demo's RIGHT_READ --
//! the multiplexing guarantee now gates the write direction. The demo fills a
//! frame with a fixed pattern, writes it out through `ring::write`, then reads
//! the same range back into a SEPARATE (freshly zeroed) frame through
//! `ring::read` and asserts the bytes match the pattern -- not the disk's
//! original ramp content -- proving the write actually reached the device
//! rather than the read-back merely returning stale or unwritten data.

#![no_std]
#![no_main]

use libos::ring;
use libplinth::{sys_exit, sys_frame_alloc, sys_frame_map, sys_write, BLK_OK, BLOCK_SLOT, MAP_BASE, PAGE_SIZE, SYS_ERR};

/// Sectors written/read back. Must match the BlockRange count the kernel
/// grants this demo (main.rs).
const COUNT: u64 = 4;

/// Offsets within a sector to spot-check, mirroring asyncblk-user's probe
/// style (a few bytes per sector rather than a full-buffer compare).
const PROBES: [usize; 4] = [0, 1, 7, 511];

/// A fixed fill pattern, deliberately unrelated to the disk image's ramp
/// formula (byte k of sector s is (s+k)&0xFF) so a read-back that returns the
/// original ramp content -- i.e. the write never happened -- is reliably
/// distinguishable from a successful round-trip.
fn pattern(offset: usize) -> u8 {
    (offset as u8).wrapping_mul(7).wrapping_add(0x11)
}

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_write(b"blkwrite: start\n");

    if !ring::init() {
        sys_write(b"blkwrite: ring init failed\n");
        sys_exit(1);
    }

    // Write frame: fill with the pattern before issuing the write.
    let w_slot = sys_frame_alloc();
    if w_slot == SYS_ERR {
        sys_exit(2);
    }
    let w_va = MAP_BASE;
    if sys_frame_map(w_slot, w_va) == SYS_ERR {
        sys_exit(3);
    }
    for k in 0..(COUNT as usize * 512) {
        // SAFETY: w_va is this process's freshly mapped frame, k is within the
        // COUNT*512 <= PAGE_SIZE bytes it covers.
        unsafe { (w_va as *mut u8).add(k).write_volatile(pattern(k)) };
    }

    let write_status = ring::block_on(ring::write(BLOCK_SLOT, w_slot, 0, COUNT));
    if write_status != BLK_OK {
        sys_write(b"blkwrite: write FAILED\n");
        sys_exit(4);
    }

    // Read-back frame: a SEPARATE, freshly allocated (zeroed) frame, so a
    // match against the pattern cannot be explained by the write frame's own
    // contents still sitting there unread.
    let r_slot = sys_frame_alloc();
    if r_slot == SYS_ERR {
        sys_exit(5);
    }
    let r_va = MAP_BASE + PAGE_SIZE;
    if sys_frame_map(r_slot, r_va) == SYS_ERR {
        sys_exit(6);
    }

    let read_status = ring::block_on(ring::read(BLOCK_SLOT, r_slot, 0, COUNT));
    if read_status != BLK_OK {
        sys_write(b"blkwrite: read-back FAILED\n");
        sys_exit(7);
    }

    // Assert: every probed offset in every sector matches the pattern that was
    // written, not the disk's original ramp content.
    let mut ok = true;
    for sector in 0..COUNT as usize {
        for &j in PROBES.iter() {
            let k = sector * 512 + j;
            let expect = pattern(k);
            // SAFETY: r_va is mapped and the device DMA'd COUNT*512 bytes in.
            let got = unsafe { (r_va as *const u8).add(k).read_volatile() };
            if got != expect {
                ok = false;
            }
        }
    }

    if ok {
        sys_write(b"blkwrite: ok (write+read-back verified)\n");
        sys_exit(0);
    } else {
        sys_write(b"blkwrite: FAIL (read-back did not match pattern)\n");
        sys_exit(8);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
