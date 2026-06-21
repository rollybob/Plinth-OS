//! BKL-contention micro-benchmark process (kernel feature `bench`).
//!
//! A kernel-entry workload with a tunable amount of userspace work between
//! entries. Each iteration calls the cheapest syscall that still takes the big
//! kernel lock -- `cpu_charge(CPU_CAP_SLOT, 0)`, which never overdraws (so the
//! process is never terminated), allocates nothing, and prints nothing -- then
//! does `BUSYWORK` volatile spin iterations before the next.
//!
//! `BUSYWORK` is set at build time from the `PLINTH_BENCH_WORK` env var (see
//! build.rs), which is what makes the residency sweep possible:
//!   - `BUSYWORK = 0`  -> the pure kernel-entry hammer: ~100% kernel residency,
//!     the WORST CASE for BKL contention.
//!   - larger values  -> real work between syscalls, dropping kernel residency
//!     toward what a realistic workload looks like, where the kernel is entered
//!     occasionally rather than constantly.
//!
//! The kernel launches `MAX_PROCESSES` copies across all cores; sweeping
//! `BUSYWORK` and watching the contention rate fall is what answers whether
//! splitting the lock (roadmap item B3) is justified at realistic residency.
//!
//! Built and run only by `cargo xtask bench`; never part of the normal boot.

#![no_std]
#![no_main]

use libplinth::{sys_cpu_charge, sys_exit, CPU_CAP_SLOT};

/// Kernel entries per process. `MAX_PROCESSES` * this = total BKL acquisitions
/// sampled per run. Kept modest so a run stays within the QEMU watchdog even at
/// high `BUSYWORK` (where each iteration is much longer).
const ITER: u64 = 20_000;

/// Userspace spin iterations between kernel entries, baked in from
/// `PLINTH_BENCH_WORK` (default 0). Parsed in a const context, so it must be a
/// hand-rolled decimal parser -- `str::parse` is not const.
const BUSYWORK: u64 = match option_env!("PLINTH_BENCH_WORK") {
    Some(s) => parse_u64(s),
    None => 0,
};

/// Const decimal parser: read the leading digit run of `s`. Non-digits stop it.
const fn parse_u64(s: &str) -> u64 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut v = 0u64;
    while i < b.len() {
        let d = b[i];
        if d >= b'0' && d <= b'9' {
            v = v * 10 + (d - b'0') as u64;
            i += 1;
        } else {
            break;
        }
    }
    v
}

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    let mut n = 0;
    while n < ITER {
        // Charge 0: one ring-3 -> kernel -> ring-3 round trip = one BKL
        // acquire, with no in-kernel work to amplify the lock hold time.
        sys_cpu_charge(CPU_CAP_SLOT, 0);
        // Userspace work between entries. Volatile so the optimizer cannot
        // delete the loop (the same reason spin-user's busywork is volatile).
        let mut s = 0u64;
        while s < BUSYWORK {
            unsafe { core::ptr::read_volatile(&s) };
            s += 1;
        }
        n += 1;
    }
    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
