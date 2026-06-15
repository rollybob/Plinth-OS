//! Capability-transfer / zero-copy IPC demo.
//!
//! The kernel launches two copies over one shared endpoint. The producer
//! (id 0) allocates a frame, writes a pattern into it, and `send`s the frame
//! *capability* to the consumer -- which moves ownership: the kernel revokes
//! the cap from the producer and unmaps it there, then mints it into the
//! consumer. The consumer (id 1) maps the received frame and reads the
//! pattern the producer wrote -- the same physical frame, never copied. This
//! is the shared-frame bulk path the synchronous message word is too small to
//! carry.

#![no_std]
#![no_main]

use libplinth::{
    sys_exit, sys_frame_alloc, sys_frame_map, sys_recv_cap, sys_send_cap, sys_write, ENDPOINT_SLOT,
    MAP_BASE, NO_CAP, SYS_ERR,
};

/// The data the producer writes and the consumer must read back.
const PATTERN: u64 = 12345;
/// Where each side maps the frame (its own address space; same VA is fine).
const VA: u64 = MAP_BASE + 0x5000;
/// A marker word sent alongside the capability (unused beyond demonstrating
/// that a word and a cap travel together).
const TAG: u64 = 1;

#[no_mangle]
pub extern "C" fn _start(id: u64) -> ! {
    if id == 0 {
        producer();
    } else {
        consumer();
    }
    sys_exit(0)
}

fn producer() {
    let slot = sys_frame_alloc();
    if slot == SYS_ERR {
        fail(b"share: producer frame_alloc failed\n");
    }
    if sys_frame_map(slot, VA) != 0 {
        fail(b"share: producer frame_map failed\n");
    }
    // SAFETY: VA was just mapped read-write for this process.
    unsafe {
        (VA as *mut u64).write_volatile(PATTERN);
    }
    sys_write(b"share: producer sent frame\n");
    // Transfer the frame capability with the message; the kernel unmaps it
    // from us as part of the move.
    sys_send_cap(ENDPOINT_SLOT, TAG, slot);
}

fn consumer() {
    let (_tag, cap_slot) = sys_recv_cap(ENDPOINT_SLOT);
    if cap_slot == NO_CAP {
        fail(b"share: consumer received no capability\n");
    }
    if sys_frame_map(cap_slot, VA) != 0 {
        fail(b"share: consumer frame_map failed\n");
    }
    // SAFETY: cap_slot names the frame the producer filled and just handed us;
    // we mapped it read-write above.
    let value = unsafe { (VA as *const u64).read_volatile() };
    emit_got(value);
}

/// Write `share: consumer got <v>\n` as one atomic sys_write.
fn emit_got(v: u64) {
    let mut buf = [0u8; 48];
    let mut len = 0;
    len += put(&mut buf[len..], b"share: consumer got ");
    len += put_dec(&mut buf[len..], v);
    len += put(&mut buf[len..], b"\n");
    sys_write(&buf[..len]);
}

fn put(dst: &mut [u8], src: &[u8]) -> usize {
    let mut i = 0;
    while i < src.len() {
        dst[i] = src[i];
        i += 1;
    }
    src.len()
}

fn put_dec(dst: &mut [u8], mut v: u64) -> usize {
    if v == 0 {
        dst[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 20];
    let mut i = 0;
    while v > 0 {
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    let mut j = 0;
    while j < i {
        dst[j] = tmp[i - 1 - j];
        j += 1;
    }
    i
}

fn fail(msg: &[u8]) -> ! {
    sys_write(msg);
    sys_exit(1)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
