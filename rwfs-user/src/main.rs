//! Read-write filesystem demo (Design/readwrite_fs.md S6): create, read,
//! delete, and reuse over the new `librwfs` library OS.
//!
//! The kernel grants this process a BlockRange over device 0 sectors
//! [32, 96), RIGHT_READ | RIGHT_WRITE -- a round-tripping cap needs both
//! rights regardless of which operation feels "primary" (the lesson
//! blkwrite-user's build caught). The demo formats the range fresh (S5),
//! creates two files, reads each back, deletes one, creates a third sized to
//! need exactly the freed run, and proves the bitmap actually reclaimed that
//! space (not just hid it) by checking the new file landed at the exact
//! sector the deleted one held -- then re-verifies the surviving file is
//! untouched by the whole delete/reuse cycle.

#![no_std]
#![no_main]

use libos::ring;
use libplinth::{sys_exit, sys_frame_alloc, sys_frame_map, sys_write, BLOCK_SLOT, MAP_BASE, SYS_ERR};
use librwfs::format::Mount;

/// Sectors the granted range spans. Must match the kernel's grant
/// (kernel/src/main.rs).
const RANGE_SECTORS: u64 = 64;

/// Sized to need exactly one sector, the same as a.txt's "hello" (5 bytes) --
/// so c.txt's create can only be satisfied by a.txt's freed run or later free
/// space, and the first-fit allocator's lowest-index behavior makes the
/// sector check below a real proof of reclaim, not a coincidence.
const C_LEN: usize = 500;

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_write(b"rwfs: start\n");

    if !ring::init() {
        sys_write(b"rwfs: ring init failed\n");
        sys_exit(1);
    }

    let frame = sys_frame_alloc();
    if frame == SYS_ERR {
        sys_exit(2);
    }
    let va = MAP_BASE;
    if sys_frame_map(frame, va) == SYS_ERR {
        sys_exit(3);
    }

    let mut fs = match Mount::format(BLOCK_SLOT, frame, va, RANGE_SECTORS) {
        Ok(m) => m,
        Err(_) => {
            sys_write(b"rwfs: format FAILED\n");
            sys_exit(4);
        }
    };
    sys_write(b"rwfs: formatted\n");

    if fs.create(b"a.txt", b"hello").is_err() {
        sys_write(b"rwfs: create a.txt FAILED\n");
        sys_exit(5);
    }
    if fs.create(b"b.txt", b"world!!").is_err() {
        sys_write(b"rwfs: create b.txt FAILED\n");
        sys_exit(6);
    }

    let mut buf = [0u8; 64];
    let n = match fs.read(b"a.txt", &mut buf) {
        Ok(n) => n,
        Err(_) => {
            sys_write(b"rwfs: read a.txt FAILED\n");
            sys_exit(7);
        }
    };
    if &buf[..n] != b"hello" {
        sys_write(b"rwfs: a.txt content MISMATCH\n");
        sys_exit(8);
    }
    let n = match fs.read(b"b.txt", &mut buf) {
        Ok(n) => n,
        Err(_) => {
            sys_write(b"rwfs: read b.txt FAILED\n");
            sys_exit(9);
        }
    };
    if &buf[..n] != b"world!!" {
        sys_write(b"rwfs: b.txt content MISMATCH\n");
        sys_exit(10);
    }
    sys_write(b"rwfs: a.txt and b.txt verified\n");

    let (a_sector, _) = match fs.stat(b"a.txt") {
        Ok(s) => s,
        Err(_) => {
            sys_write(b"rwfs: stat a.txt FAILED\n");
            sys_exit(11);
        }
    };

    if fs.delete(b"a.txt").is_err() {
        sys_write(b"rwfs: delete a.txt FAILED\n");
        sys_exit(12);
    }
    if fs.read(b"a.txt", &mut buf).is_ok() {
        sys_write(b"rwfs: a.txt still readable after delete\n");
        sys_exit(13);
    }
    sys_write(b"rwfs: a.txt deleted\n");

    let c_data = [0x5Au8; C_LEN];
    if fs.create(b"c.txt", &c_data).is_err() {
        sys_write(b"rwfs: create c.txt FAILED\n");
        sys_exit(14);
    }
    let (c_sector, _) = match fs.stat(b"c.txt") {
        Ok(s) => s,
        Err(_) => {
            sys_write(b"rwfs: stat c.txt FAILED\n");
            sys_exit(15);
        }
    };
    if c_sector != a_sector {
        sys_write(b"rwfs: c.txt did NOT reuse a.txt's freed sector\n");
        sys_exit(16);
    }
    sys_write(b"rwfs: bitmap reclaim verified (c.txt reused a.txt's sector)\n");

    let mut cbuf = [0u8; C_LEN];
    let n = match fs.read(b"c.txt", &mut cbuf) {
        Ok(n) => n,
        Err(_) => {
            sys_write(b"rwfs: read c.txt FAILED\n");
            sys_exit(17);
        }
    };
    if cbuf[..n] != c_data[..] {
        sys_write(b"rwfs: c.txt content MISMATCH\n");
        sys_exit(18);
    }
    let n = match fs.read(b"b.txt", &mut buf) {
        Ok(n) => n,
        Err(_) => {
            sys_write(b"rwfs: re-read b.txt FAILED\n");
            sys_exit(19);
        }
    };
    if &buf[..n] != b"world!!" {
        sys_write(b"rwfs: b.txt content MISMATCH after reuse cycle\n");
        sys_exit(20);
    }

    sys_write(b"rwfs: ok (create/read/delete/reuse verified)\n");
    sys_exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
