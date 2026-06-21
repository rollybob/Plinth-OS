//! The single big kernel lock (Stage B2, design D4 --
//! Design/broader_hardware.md section 5.3).
//!
//! One lock, acquired as the first action of every kernel-entry dispatch
//! body (the timer tick, the syscall/IPC dispatchers, the fault handlers,
//! the device-completion IRQ handlers) and released immediately before
//! that body either returns to its caller normally, or hands off to ring 3
//! (iretq/sysretq), or to the idle loop's deliberate `sti; hlt` wait. Until
//! Stage B2.3 wakes a second core, this is uniprocessor-equivalent to the
//! "single CPU, IF=0" discipline most of those call sites already document
//! in comments -- B2.1 only narrows what that comment means, from "the
//! whole kernel" to "while the lock is held."
//!
//! Manual acquire/release, not RAII: several of the bodies above reach a
//! genuine longjmp (`sched_resume`, `sched_return_to_kernel`,
//! `kernel_resume`, `resume_user_trap`, `deliver_fault_handler`) several
//! call frames deep, and none of those run destructors on the way out --
//! the same reason `syscall.rs`'s `CURRENT`/`FRAME_ALLOC` guards are always
//! manually scoped to end before such a call, rather than relying on
//! `Drop`. The lock is released at the small number of actual divergence
//! points (see scheduler.rs::resume_process/switch_to_next/
//! idle_until_runnable, process.rs::exit_current, fault.rs::resume/
//! page_fault_dispatch) instead of at every call site that might lead to
//! one -- the lock simply stays held across the intervening, already-
//! BKL-safe call chain.

use spin::Mutex;

static BKL: Mutex<()> = Mutex::new(());

/// Acquire the kernel lock. Call as the first action of a kernel-entry
/// dispatch body. Must be paired with exactly one `release()` call before
/// the body's control flow leaves the kernel (returns to ring 3, idles, or
/// returns normally to its non-dispatch caller).
pub fn acquire() {
    // `forget` discards the guard without running its Drop (which would
    // unlock immediately) -- ownership of "locked" is tracked by the
    // acquire/release call pairing itself, not by a live guard value.
    //
    // Production build: a plain blocking `lock()`. The `bench` build instead
    // splits the acquire into an uncontended fast path and an instrumented
    // contended slow path (below) to measure how often the lock is actually
    // contended under multi-core load -- see `cargo xtask bench`.
    #[cfg(not(feature = "bench"))]
    core::mem::forget(BKL.lock());

    #[cfg(feature = "bench")]
    bench_stats::acquire_instrumented();
}

/// Release the kernel lock acquired by a prior `acquire()`.
///
/// # Safety
/// The caller must currently hold the lock (a prior `acquire()` with no
/// matching `release()` yet) on this same core, and must not call this
/// twice for one `acquire()`.
pub unsafe fn release() {
    BKL.force_unlock();
}

/// BKL contention instrumentation, compiled in only by the `bench` build
/// (`cargo xtask bench`). Per-core counters reached by `core_id()`; each core
/// only ever writes its own slot, and they are read back (`report`) after the
/// workload has quiesced. Relaxed atomics suffice -- these are statistics, not
/// a synchronisation mechanism, and the BKL itself orders everything that
/// matters. Zero cost in the production build: the whole module is cfg'd out.
#[cfg(feature = "bench")]
mod bench_stats {
    use super::BKL;
    use crate::percpu::{core_id, MAX_CORES};
    use core::fmt::Write;
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

    static ACQUIRES: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];
    static CONTENDED: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];
    static WAIT_CYCLES: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];

    /// Acquire the BKL, recording whether it was contended (held by another
    /// core) and, if so, how many cycles this core spun before getting it.
    pub fn acquire_instrumented() {
        // SAFETY: `acquire` is only ever called as the first action of a
        // kernel-entry dispatch body, which on any core runs strictly after
        // that core's `percpu::init`, so `GS_BASE` is set and `core_id()` is
        // valid here.
        let c = unsafe { core_id() };
        match BKL.try_lock() {
            // Uncontended: got it on the first try. Forget the guard (manual
            // release pairing, like the production path).
            Some(guard) => core::mem::forget(guard),
            // Contended: another core holds it. Count the collision and time
            // the blocking acquire that follows.
            None => {
                CONTENDED[c].fetch_add(1, Relaxed);
                // SAFETY: rdtsc is a baseline x86_64 instruction, always valid.
                let start = unsafe { core::arch::x86_64::_rdtsc() };
                core::mem::forget(BKL.lock());
                let waited = unsafe { core::arch::x86_64::_rdtsc() }.wrapping_sub(start);
                WAIT_CYCLES[c].fetch_add(waited, Relaxed);
            }
        }
        ACQUIRES[c].fetch_add(1, Relaxed);
    }

    /// Zero every counter. Call right before the measured workload so the
    /// report reflects only it, not the boot/demo BKL traffic before it.
    pub fn reset() {
        for i in 0..MAX_CORES {
            ACQUIRES[i].store(0, Relaxed);
            CONTENDED[i].store(0, Relaxed);
            WAIT_CYCLES[i].store(0, Relaxed);
        }
    }

    /// Print the per-core and aggregate contention figures. `ppm` is contended
    /// acquisitions per million (10_000 ppm = 1%); `avg_wait_cyc` is the mean
    /// cycles a contended acquire spent spinning. `xtask bench` greps these.
    pub fn report(w: &mut dyn Write) {
        let (mut tot_acq, mut tot_con, mut tot_cyc) = (0u64, 0u64, 0u64);
        for i in 0..MAX_CORES {
            let acq = ACQUIRES[i].load(Relaxed);
            if acq == 0 {
                continue; // a core that never entered the kernel during the run
            }
            let con = CONTENDED[i].load(Relaxed);
            let cyc = WAIT_CYCLES[i].load(Relaxed);
            let ppm = con.saturating_mul(1_000_000) / acq;
            let avg = if con > 0 { cyc / con } else { 0 };
            let _ = writeln!(
                w,
                "plinth: bkl bench: core {i} acquires={acq} contended={con} ({ppm} ppm) avg_wait_cyc={avg}"
            );
            tot_acq += acq;
            tot_con += con;
            tot_cyc += cyc;
        }
        let ppm = if tot_acq > 0 { tot_con.saturating_mul(1_000_000) / tot_acq } else { 0 };
        let avg = if tot_con > 0 { tot_cyc / tot_con } else { 0 };
        let _ = writeln!(
            w,
            "plinth: bkl bench: TOTAL acquires={tot_acq} contended={tot_con} ({ppm} ppm) avg_wait_cyc={avg}"
        );
    }
}

/// Re-exported so `main.rs`'s `bench` path can drive the measurement.
#[cfg(feature = "bench")]
pub use bench_stats::{report as bench_report, reset as bench_reset};
