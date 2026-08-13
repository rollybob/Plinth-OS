//! cap_release demo: a capability slot, once released, is genuinely reusable.
//!
//! This is the regression test for the 2026-06-27 crash (Design/cap_release.md).
//! A capability table is a fixed 16 slots with no heap behind it, and until ABI
//! v2.8 a process had no way to give one back. Every `spawn` mints a RECV
//! capability -- the wait handle -- into the caller's table, and after the join
//! that handle is dead weight nobody could remove. The shell leaked one slot per
//! app launch and its ninth-or-so launch failed with the table full.
//!
//! The loop below runs the same spawn -> join -> release cycle
//! `ROUNDS` times, which is comfortably more than the system can have in flight
//! at once. Without the release it cannot get through; with it, every round
//! reuses what the previous round gave back. Nothing here is graphical and
//! the child is silent, so the whole demo costs two lines of boot log.
//!
//! **What actually binds is the endpoint pool, not the capability table**, and
//! this comment said otherwise until 2026-08-10. Measured, by deleting the
//! release below and reading the round it dies on: **round 8**, with fifteen cap
//! slots still free. `MAX_ENDPOINTS` is 8 (`kernel/src/ipc.rs`) and every spawn
//! creates one for its result channel, so a leaked handle -- which is what keeps
//! that endpoint referenced -- exhausts the pool long before the 16-slot table it
//! also occupies. The demo is unaffected and still catches the leak; only the
//! explanation was wrong. See assumption A-13 in Design/known_bugs.md.
//!
//! Worth being precise about, because the 06-27 crash this guards against WAS a
//! table-full failure. This demo cannot actually reach a full table -- the pool
//! runs out first -- so it catches the regression by a different limit than the
//! one that originally broke. `ROUNDS` is past both, so the protection holds
//! either way.
//!
//! **Which pool binds is a consequence of two constants, and both are
//! placeholders.** `MAX_ENDPOINTS` (8) and `MAX_CAPS` (16) are kernel-side and
//! neither is exported here, so the `ROUNDS` below is a literal chosen against
//! numbers this crate cannot see. If the endpoint pool ever grows past `ROUNDS`,
//! this demo passes while testing nothing -- green, asserting nothing, the exact
//! failure class A-13 was filed for. `lender_owed.md` D9 rules that these round
//! counts be derived from the kernel constants rather than copied; until that
//! lands, treat this number as owed a re-measurement whenever either limit moves.
//!
//! Note what is NOT being proved: the frame path was always releasable (the
//! retired `frame_free` did that). What is new is releasing a non-frame
//! capability -- an `Endpoint` -- and spawn handles are the only way a user
//! process gets one repeatedly.

#![no_std]
#![no_main]

use libplinth::{
    sys_cap_release, sys_exit, sys_recv, sys_spawn, sys_write, IPC_OK, NO_CAP, SYS_ERR,
};

/// quietworker's index in the kernel's SPAWNABLE table (see kernel main.rs).
const QUIETWORKER_ID: u64 = 4;

/// Round-trips to run. A leaking build dies at round 8 (measured -- the eight
/// endpoints, see the module docs), and would die by round 15 on the capability
/// table even if the pool were bottomless. Twenty clears both and keeps the demo
/// quick.
const ROUNDS: u64 = 20;

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    let mut round = 0u64;
    while round < ROUNDS {
        // Each spawn mints a fresh RECV handle into this table.
        let handle = sys_spawn(QUIETWORKER_ID, NO_CAP);
        if handle == SYS_ERR {
            // The failure the leak used to produce: no free slot for the handle.
            emit(b"caprelease: spawn failed at round ", round);
            sys_exit(2);
        }
        // The join. After it the channel has no further use.
        let (status, _value) = sys_recv(handle);
        if status != IPC_OK {
            emit(b"caprelease: join failed at round ", round);
            sys_exit(3);
        }
        // Give the slot back. This is the whole point of the demo.
        if sys_cap_release(handle) != 0 {
            emit(b"caprelease: release failed at round ", round);
            sys_exit(4);
        }
        round += 1;
    }

    // Releasing an empty slot must fail: release is not idempotent, and a
    // double release would otherwise look like success while the second call
    // did nothing.
    if sys_cap_release(0xFFFF) != SYS_ERR {
        sys_write(b"caprelease: releasing a bad slot should have failed\n");
        sys_exit(5);
    }

    emit_line(b"caprelease: ", ROUNDS, b" spawn round-trips, slots reused\n");
    sys_exit(0)
}

/// Write `<prefix><v>\n` as one atomic sys_write. Same shape as spawner-user's
/// `emit`: one write per line, so concurrent processes cannot interleave
/// mid-line in the serial log.
fn emit(prefix: &[u8], v: u64) {
    emit_line(prefix, v, b"\n");
}

/// Write `<prefix><v><suffix>` as one atomic sys_write.
fn emit_line(prefix: &[u8], v: u64, suffix: &[u8]) {
    let mut buf = [0u8; 96];
    let mut len = 0;
    len += put(&mut buf[len..], prefix);
    len += put_dec(&mut buf[len..], v);
    len += put(&mut buf[len..], suffix);
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
