//! Work-stealing demo (worker half, Design/smp_scaling.md S4).
//!
//! `stealer-user` spawns several copies of this worker back to back. Because
//! `spawn` homes a child to the SPAWNING core, they all pile onto one core's
//! run queue while the other cores sit idle -- exactly the imbalance a
//! work-steal resolves (the scheduler's idle cores pull these off the busy
//! core's array). Each worker does a fixed slice of CPU busywork (so it is
//! still runnable when an idle core comes looking, and so the parallelism is
//! real rather than instant), prints one line, reports completion to its
//! parent over the result channel the kernel set up at spawn (ENDPOINT_SLOT),
//! and exits.
//!
//! The scheduler passes this process's id in rdi at entry (see ABI.md).

#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_send, sys_write, ENDPOINT_SLOT};

/// CPU busywork iterations before reporting. Volatile so the optimizer cannot
/// delete the loop. Sized like spin-user's: long enough that an idle core has
/// time to steal this worker before its home core finishes it. The scheduler
/// is correct for any value; this only affects how reliably a steal is forced.
const BUSYWORK: u64 = 1_500_000;

#[no_mangle]
pub extern "C" fn _start(id: u64) -> ! {
    let mut spin = 0u64;
    while spin < BUSYWORK {
        // SAFETY: reading a local's own address, purely to defeat the optimizer
        // so the delay loop survives.
        unsafe { core::ptr::read_volatile(&spin) };
        spin += 1;
    }
    emit(id);
    // Report completion on the channel the kernel set up at spawn; the parent's
    // recv on the matching handle collects it (and is the join).
    sys_send(ENDPOINT_SLOT, id);
    sys_exit(0)
}

/// Write `stealwork[<id>] done\n` as one atomic sys_write (a line emitted as
/// several writes could be spliced by another process's output mid-line).
fn emit(id: u64) {
    let mut buf = [0u8; 32];
    let mut len = 0;
    len += put(&mut buf[len..], b"stealwork[");
    len += put_dec(&mut buf[len..], id);
    len += put(&mut buf[len..], b"] done\n");
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
