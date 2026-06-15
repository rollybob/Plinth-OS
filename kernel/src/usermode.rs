//! Synchronous user-mode execution.
//!
//! Plinth runs one user process at a time, to completion -- no scheduler,
//! no timer. enter_user() saves the kernel's callee-saved state and rsp,
//! then iretq's into ring 3. Control returns through kernel_resume(),
//! called from exactly two places: the exit syscall, and the exception
//! handlers when a user process faults. kernel_resume restores the saved
//! kernel stack and "returns" from enter_user with the value passed to it
//! -- the setjmp/longjmp shape, with the kernel as the jump target.
//!
//! The interrupt frame (on a fault) and the syscall stack (on exit) are
//! simply abandoned; with a single CPU and no nesting there is nothing on
//! them the kernel ever needs again.

use core::arch::global_asm;

/// kernel_resume value meaning "the process faulted" (exception handlers).
/// Exit codes are masked to 32 bits in the exit syscall, so this cannot
/// collide with a real exit code.
pub const EXIT_FAULTED: u64 = 0xFFFF_FFFF_FFFF_FFFE;

/// kernel_resume value meaning "the process overdrew its CPU budget"
/// (cpu_charge with no budget left). Like EXIT_FAULTED, it sits above the
/// 32-bit exit-code range and cannot collide with a real exit code.
pub const EXIT_OUT_OF_BUDGET: u64 = 0xFFFF_FFFF_FFFF_FFFD;

/// Kernel rsp at the moment enter_user committed to ring 3. Written by
/// enter_user_asm, read by kernel_resume. Single CPU, no reentrancy.
#[no_mangle]
static mut KERNEL_SAVED_RSP: u64 = 0;

global_asm!(
    r#"
.global enter_user_asm
enter_user_asm:
    // rdi = user entry point, rsi = user stack pointer
    push rbx
    push rbp
    push r12
    push r13
    push r14
    push r15
    mov [rip + KERNEL_SAVED_RSP], rsp

    // iretq frame, pushed SS / RSP / RFLAGS / CS / RIP.
    // Selectors are asserted against the GDT in gdt::init.
    // RFLAGS = 0x202: the reserved bit plus IF -- ring 3 runs with
    // interrupts ENABLED, so the PIT timer fires while a user process is on
    // the CPU. The kernel still runs with IF clear (SFMask on syscalls,
    // interrupt gates on traps), so it is never reentered by the timer.
    mov rax, 0x1b
    push rax
    push rsi
    push 0x202
    mov rax, 0x23
    push rax
    push rdi
    iretq

.global kernel_resume
kernel_resume:
    // rdi = value enter_user returns. Restores the stack saved above and
    // returns to enter_user's caller as if enter_user returned normally.
    mov rsp, [rip + KERNEL_SAVED_RSP]
    mov rax, rdi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    ret
"#
);

extern "C" {
    fn enter_user_asm(entry: u64, user_rsp: u64) -> u64;
    /// Abandon the current kernel stack and return from enter_user with
    /// `value`. Only call when user code (or its handler) is on the CPU.
    pub fn kernel_resume(value: u64) -> !;
}

/// Run user code at `entry` with `user_rsp`. Returns the exit syscall's
/// code, or EXIT_FAULTED if an exception terminated the process.
pub fn enter_user(entry: u64, user_rsp: u64) -> u64 {
    // SAFETY: caller (process::run) has mapped entry and the stack as
    // USER_ACCESSIBLE, installed CURRENT, and holds no locks.
    unsafe { enter_user_asm(entry, user_rsp) }
}
