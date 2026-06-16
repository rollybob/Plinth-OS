//! Spawn + wait demo (parent half), including crash reaping.
//!
//! `spawn` no longer runs a child synchronously nested under the caller; it
//! launches the child as an independent, concurrently scheduled process and
//! returns a handle -- a receive capability on a result channel the kernel set
//! up. The parent collects the child's result with `recv` on that handle, and
//! that recv IS the wait (`spawn_and_wait` packages the two).
//!
//! This parent waits on two children: one that sends a result and exits
//! cleanly, and one (`faultchild`) that faults before sending. The second wait
//! must NOT hang -- the kernel's death-time reaping wakes it with
//! `IPC_PEER_DIED`, so the parent observes the crash and continues. Both result
//! endpoints are reclaimed regardless, so a crashed child leaks nothing.

#![no_std]
#![no_main]

use libplinth::{spawn_and_wait, sys_exit, sys_write, IPC_OK, IPC_PEER_DIED, NO_CAP};

/// Spawnable child ids (see the kernel's SPAWNABLE table).
const WORKER_ID: u64 = 0;
const FAULT_WORKER_ID: u64 = 1;

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    // 1) A worker that sends its result back -- the normal join.
    let (status, result) = spawn_and_wait(WORKER_ID, NO_CAP);
    if status != IPC_OK {
        sys_exit(102);
    }
    emit(result);

    // 2) A worker that faults before sending. The wait must be woken by the
    // kernel's reaping (IPC_PEER_DIED), not left blocked forever.
    let (status, _) = spawn_and_wait(FAULT_WORKER_ID, NO_CAP);
    if status == IPC_PEER_DIED {
        sys_write(b"spawner: dead child reaped\n");
    } else {
        sys_write(b"spawner: expected a dead child -- reaping FAILED\n");
        sys_exit(103);
    }

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
