//! Preemptive-scheduler demo process.
//!
//! The kernel launches several copies of this binary as independent
//! processes and round-robins them under the timer; each prints its id and a
//! counter in a loop. Interleaved lines in the boot log are preemption made
//! visible. A single process's OWN lines are always in program order -- only
//! the cross-process interleaving is nondeterministic.
//!
//! The scheduler passes this process's id in rdi at entry (see ABI.md), so
//! `_start` takes it as its first C-ABI argument.
//!
//! Each line is formatted into a local buffer and emitted with ONE sys_write.
//! That matters: a syscall is atomic against preemption (the kernel runs with
//! interrupts off), but ring-3 code between syscalls is not. Emitting a line
//! as several writes would let another process's output splice into the
//! middle of this one's line. One write per line keeps every line intact.

#![no_std]
#![no_main]

use libplinth::{sys_exit, sys_write};

/// Iterations each process prints before exiting.
const ITER: u64 = 6;

/// Busywork between prints, so a timer tick is likely to land mid-loop and
/// the round-robin interleaving actually shows up in the log. Volatile, so
/// the optimizer cannot delete the loop. Tuned for visible interleaving under
/// this machine's TCG QEMU; the kernel is correct for any value.
const BUSYWORK: u64 = 1_500_000;

#[no_mangle]
pub extern "C" fn _start(id: u64) -> ! {
    let mut n = 0;
    while n < ITER {
        emit(id, n);
        let mut spin = 0u64;
        while spin < BUSYWORK {
            // SAFETY: reading a local's own address; purely to defeat the
            // optimizer so the delay loop survives.
            unsafe { core::ptr::read_volatile(&spin) };
            spin += 1;
        }
        n += 1;
    }
    sys_exit(0)
}

/// Format `spin[<id>] <n>\n` into a stack buffer and write it atomically.
fn emit(id: u64, n: u64) {
    let mut buf = [0u8; 32];
    let mut len = 0;
    len += put(&mut buf[len..], b"spin[");
    len += put_dec(&mut buf[len..], id);
    len += put(&mut buf[len..], b"] ");
    len += put_dec(&mut buf[len..], n);
    len += put(&mut buf[len..], b"\n");
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
