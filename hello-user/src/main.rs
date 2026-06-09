//! End-to-end exercise of the whole syscall surface from ring 3:
//! write, frame_alloc, frame_map (at an address THIS PROCESS chooses),
//! a volatile round-trip through the mapped frame, frame_free, exit.

#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_frame_alloc, sys_frame_free, sys_frame_map, sys_write, SYS_ERR};

/// Where this process chooses to map its frame -- the kernel has no
/// opinion as long as it is inside the user mapping window.
const MAP_VA: u64 = libplinth::MAP_BASE + 0x4000;

const PATTERN: u64 = 0xc0de_f00d_0000_0042;

#[link_section = ".text.entry"]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    sys_write(b"hello: ring 3\n");

    let slot = sys_frame_alloc();
    if slot == SYS_ERR {
        fail(b"hello: frame_alloc failed\n");
    }
    if sys_frame_map(slot, MAP_VA) != 0 {
        fail(b"hello: frame_map failed\n");
    }

    // Volatile, so the accesses actually hit the freshly mapped frame.
    // SAFETY: the kernel just mapped MAP_VA read-write for this process.
    unsafe {
        let p = MAP_VA as *mut u64;
        p.write_volatile(PATTERN);
        if p.read_volatile() != PATTERN {
            fail(b"hello: readback mismatch\n");
        }
        // The frame must arrive zeroed past our write -- no data leaks
        // from previous owners.
        if p.add(1).read_volatile() != 0 {
            fail(b"hello: frame not zeroed\n");
        }
    }
    sys_write(b"hello: frame mapped and writable\n");

    if sys_frame_free(slot) != 0 {
        fail(b"hello: frame_free failed\n");
    }
    // The capability is gone; using it again must fail.
    if sys_frame_map(slot, MAP_VA) != SYS_ERR {
        fail(b"hello: revoked capability still usable\n");
    }
    sys_write(b"hello: done\n");
    sys_exit(0)
}

fn fail(msg: &[u8]) -> ! {
    sys_write(msg);
    sys_exit(1)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
