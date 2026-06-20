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
    core::mem::forget(BKL.lock());
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
