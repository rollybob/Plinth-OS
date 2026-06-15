//! RPC demo: call / reply over a synchronous endpoint.
//!
//! The kernel launches a server (id 0) and a client (id 1) over one endpoint,
//! granting the server RIGHT_RECV and the client RIGHT_SEND -- directional
//! rights, so neither can do the other's half. The client `call`s with a
//! request and blocks for the reply; the server `recv`s the request together
//! with a one-shot reply capability, computes a response, and `reply`s to that
//! exact caller. Note the server never holds a send right on the endpoint --
//! the reply capability alone authorizes its answer.

#![no_std]
#![no_main]

use libplinth::{sys_call, sys_exit, sys_recv_cap, sys_reply, sys_write, ENDPOINT_SLOT};

/// Number of requests exchanged.
const N: u64 = 3;
/// The server's response is the request plus this, so a reply is
/// distinguishable from the request.
const RESP_OFFSET: u64 = 1000;

#[no_mangle]
pub extern "C" fn _start(id: u64) -> ! {
    if id == 0 {
        server();
    } else {
        client();
    }
    sys_exit(0)
}

fn server() {
    let mut i = 0;
    while i < N {
        let (req, reply_cap) = sys_recv_cap(ENDPOINT_SLOT);
        sys_reply(reply_cap, req + RESP_OFFSET);
        emit_one(b"server: served ", req);
        i += 1;
    }
}

fn client() {
    let mut i = 0;
    while i < N {
        let resp = sys_call(ENDPOINT_SLOT, i);
        emit_two(b"client: call ", i, b" got ", resp);
        i += 1;
    }
}

/// `<prefix><v>\n`, one atomic write.
fn emit_one(prefix: &[u8], v: u64) {
    let mut buf = [0u8; 48];
    let mut len = 0;
    len += put(&mut buf[len..], prefix);
    len += put_dec(&mut buf[len..], v);
    len += put(&mut buf[len..], b"\n");
    sys_write(&buf[..len]);
}

/// `<p1><a><p2><b>\n`, one atomic write.
fn emit_two(p1: &[u8], a: u64, p2: &[u8], b: u64) {
    let mut buf = [0u8; 64];
    let mut len = 0;
    len += put(&mut buf[len..], p1);
    len += put_dec(&mut buf[len..], a);
    len += put(&mut buf[len..], p2);
    len += put_dec(&mut buf[len..], b);
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
