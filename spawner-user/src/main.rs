//! Spawn + wait demo (parent half).
//!
//! `spawn` no longer runs a child synchronously nested under the caller; it
//! launches the child as an independent, concurrently scheduled process and
//! returns a handle -- a receive capability on a result channel the kernel set
//! up. The parent collects the child's result with `recv` on that handle, and
//! that recv IS the wait. The child runs alongside this process under the
//! scheduler.

#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_recv, sys_spawn, sys_write, NO_CAP, SYS_ERR};

/// Spawnable child id (see the kernel's SPAWNABLE table).
const WORKER_ID: u64 = 0;

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    // Launch the worker; no capability handed over (NO_CAP). The returned
    // handle is the receive end of the result channel.
    let handle = sys_spawn(WORKER_ID, NO_CAP);
    if handle == SYS_ERR {
        sys_exit(101);
    }
    sys_write(b"spawner: launched worker\n");

    // Wait for the worker's result -- the join is just a recv.
    let result = sys_recv(handle);
    emit(result);
    sys_exit(0)
}

/// Write `spawner: worker returned <v>\n` as one atomic sys_write.
fn emit(v: u64) {
    let mut buf = [0u8; 48];
    let mut len = 0;
    len += put(&mut buf[len..], b"spawner: worker returned ");
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
