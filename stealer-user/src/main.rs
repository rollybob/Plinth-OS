//! Work-stealing demo (parent half, Design/smp_scaling.md S4).
//!
//! Forces the imbalance a work-steal resolves, and asserts the workers all
//! complete. `spawn` is non-blocking and homes each child to the SPAWNING
//! core, so this parent spawns every worker FIRST -- they pile onto this core's
//! run queue while the other cores sit idle -- and only THEN joins them. (A
//! sequential spawn-then-wait per child, like spawner-user, would never pile
//! up: each child would run and finish before the next was even created, so no
//! core would ever have a spare runnable process for an idle core to steal.)
//!
//! Completion is asserted here (every join returns IPC_OK); the cross-core
//! MOVE -- the one fact only stealing produces -- is asserted by the kernel's
//! steal counter, printed by the boot path (see main.rs's steal demo). Each
//! worker also prints its own "stealwork[id] done" line.

#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_recv, sys_spawn, sys_write, IPC_OK, NO_CAP, SYS_ERR};

/// Spawnable child id of stealwork-user (see the kernel's SPAWNABLE table).
const STEALWORK_ID: u64 = 2;

/// Workers to spawn. The kernel caps the process table at MAX_PROCESSES = 4, so
/// this parent (one slot) plus 3 workers fills it exactly -- the strongest
/// imbalance the table allows: every other slot homed to this one core.
const WORKERS: usize = 3;

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    // Spawn all workers up front (non-blocking) so they pile up before any are
    // joined.
    let mut handles = [0u64; WORKERS];
    let mut i = 0;
    while i < WORKERS {
        let h = sys_spawn(STEALWORK_ID, NO_CAP);
        if h == SYS_ERR {
            sys_write(b"stealer: spawn failed\n");
            sys_exit(101);
        }
        handles[i] = h;
        i += 1;
    }

    // Join every worker -- recv on its handle IS the wait; IPC_OK means it ran
    // to completion and reported back.
    let mut joined = 0u64;
    i = 0;
    while i < WORKERS {
        let (status, _result) = sys_recv(handles[i]);
        if status == IPC_OK {
            joined += 1;
        }
        i += 1;
    }

    emit(joined);
    sys_exit(0)
}

/// Write `stealer: joined <n> workers\n` as one atomic sys_write.
fn emit(n: u64) {
    let mut buf = [0u8; 48];
    let mut len = 0;
    len += put(&mut buf[len..], b"stealer: joined ");
    len += put_dec(&mut buf[len..], n);
    len += put(&mut buf[len..], b" workers\n");
    sys_write(&buf[..len]);
}

/// Copy `src` into `dst`, returning the number of bytes written.
fn put(dst: &mut [u8], src: &[u8]) -> usize {
    let mut i = 0;
    while i < src.len() {
        dst[i] = src[i];
        i += 1;
    }
    src.len()
}

/// Write `v` in decimal into `dst`, returning the number of digits written.
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
