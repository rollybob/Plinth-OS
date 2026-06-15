//! Synchronous-IPC demo: a pinger and a ponger rendezvous over one endpoint.
//!
//! The kernel launches two copies of this binary as scheduled processes and
//! grants both a capability to the same endpoint (at ENDPOINT_SLOT). The id
//! the scheduler passes in rdi selects the role: id 0 pings, id 1 pongs.
//!
//! Each round: the pinger sends `i` and waits for a reply; the ponger
//! receives `i`, sends back `i + 100`, and the pinger reads it. The two
//! processes' lines interleave in the boot log -- but a single process's are
//! always in program order, and the exchanged values prove the rendezvous
//! actually moved the right data (the smoke test checks both).
//!
//! Each line is one atomic sys_write so preemption can never splice another
//! process's output into the middle of a line.

#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_recv, sys_send, sys_write, ENDPOINT_SLOT};

/// Rounds each side performs before exiting.
const ROUNDS: u64 = 4;

/// Value the ponger adds, so a reply is distinguishable from the request.
const PONG_OFFSET: u64 = 100;

#[no_mangle]
pub extern "C" fn _start(id: u64) -> ! {
    if id == 0 {
        pinger();
    } else {
        ponger();
    }
    sys_exit(0)
}

fn pinger() {
    let mut i = 0;
    while i < ROUNDS {
        sys_send(ENDPOINT_SLOT, i);
        let reply = sys_recv(ENDPOINT_SLOT);
        emit(b"ping", i, reply);
        i += 1;
    }
}

fn ponger() {
    let mut i = 0;
    while i < ROUNDS {
        let msg = sys_recv(ENDPOINT_SLOT);
        sys_send(ENDPOINT_SLOT, msg + PONG_OFFSET);
        emit(b"pong", i, msg);
        i += 1;
    }
}

/// Write `<tag> <i> got <v>\n` as a single, un-splittable sys_write.
fn emit(tag: &[u8], i: u64, v: u64) {
    let mut buf = [0u8; 48];
    let mut len = 0;
    len += put(&mut buf[len..], tag);
    len += put(&mut buf[len..], b" ");
    len += put_dec(&mut buf[len..], i);
    len += put(&mut buf[len..], b" got ");
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

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
