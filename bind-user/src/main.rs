//! Direct-binding slice 3 demo: a library OS drives a bound virtio-blk device's
//! queue itself. It maps the device (notify page + used ring + data buffer) via
//! `bind_device`, rings the doorbell with a plain MMIO store, drains the used ring
//! from its own mapping, and verifies the read the kernel pre-submitted -- with no
//! syscall on the doorbell or completion path. The kernel still writes the
//! descriptor here; the library OS writes its own in slice 4.

#![no_std]
#![no_main]

use libplinth::{sys_bind_device, sys_exit, sys_write, BIND_SLOT, MAP_BASE};

/// The 3-page window `bind_device` maps into us: notify register at BASE, used
/// ring at BASE + 0x1000, data buffer at BASE + 0x2000.
const BASE: u64 = MAP_BASE + 0x8000;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    sys_write(b"bind: ring 3\n");

    let mut qsize: u32 = 0;
    if sys_bind_device(BIND_SLOT, BASE, &mut qsize as *mut u32 as u64) != 0 {
        fail(b"bind: bind_device failed\n");
    }
    if qsize == 0 {
        fail(b"bind: queue size zero\n");
    }

    let notify = BASE as *mut u16;
    let used_idx = (BASE + 0x1000 + 2) as *const u16; // used ring idx is at offset 2
    let data = (BASE + 0x2000) as *const u8;

    // The kernel pre-wrote the descriptor but did not ring, so nothing has
    // completed yet.
    // SAFETY: the used-ring page was just mapped user-accessible.
    let before = unsafe { core::ptr::read_volatile(used_idx) };

    // Ring the doorbell ourselves: a store of the queue index (0) to the mapped
    // notify register. No syscall -- this is the whole point of direct binding.
    // SAFETY: `notify` is the device's mapped, uncached notify register.
    unsafe { core::ptr::write_volatile(notify, 0u16) };

    // Drain: poll the used ring in our OWN mapping until the device advances it.
    let mut spins = 0u64;
    loop {
        // SAFETY: mapped used-ring page.
        if unsafe { core::ptr::read_volatile(used_idx) } != before {
            break;
        }
        spins += 1;
        if spins >= 200_000_000 {
            fail(b"bind: used ring never advanced\n");
        }
        core::hint::spin_loop();
    }

    // The read landed in our data buffer: sector 0 must be the ramp (byte i==i%256).
    let mut i = 0u64;
    while i < 512 {
        // SAFETY: mapped data page holding the 512-byte read.
        if unsafe { core::ptr::read_volatile(data.add(i as usize)) } != (i % 256) as u8 {
            fail(b"bind: data buffer is not the ramp\n");
        }
        i += 1;
    }

    sys_write(b"bind: doorbell rung + used ring drained + sector 0 verified (libos-driven)\n");
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
