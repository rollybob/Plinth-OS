//! Application-level page-fault handling -- "self-paging".
//!
//! The exokernel move: when a user process faults on an unmapped page in
//! its registered *lazy region*, the kernel does not resolve the fault and
//! it does not kill the process. It hands the fault back to ring 3. A
//! user-registered handler maps a frame with the ordinary frame_alloc /
//! frame_map syscalls -- the same interface any process uses -- and the
//! faulting instruction is then retried. Policy (what backs this address)
//! lives in userspace; the kernel keeps only the mechanism (deliver the
//! fault, let the process map, resume).
//!
//! ## Mechanism (signal-style upcall)
//!
//! Resuming the faulting instruction means restoring the *exact* register
//! state it faulted with, because a ring-3 handler clobbers everything in
//! between. So:
//!
//!   1. `page_fault_entry` (a naked stub installed via set_handler_addr,
//!      not the x86-interrupt ABI) captures every GP register into a
//!      `RawTrap` alongside the CPU-pushed rip/cs/rflags/rsp/ss.
//!   2. `page_fault_dispatch` decides: kernel fault -> panic; unhandleable
//!      user fault -> log + terminate (the crash-demo path); a demand fault
//!      in the lazy region with a handler -> snapshot the trap into
//!      SAVED_TRAP and `deliver_fault_handler` -- iretq into the handler in
//!      ring 3, on its own stack, fault address in rdi.
//!   3. The handler maps the page and calls `fault_return` (syscall nr 8).
//!   4. `resume_user_trap` reloads SAVED_TRAP and iretqs back to the
//!      faulting instruction, which now succeeds.
//!
//! Single CPU, one process, run to completion: there is never a second
//! fault in flight, so one SAVED_TRAP suffices. A fault *during* the
//! handler (in_fault already set) is unhandleable and terminates the
//! process -- the kernel never recurses into a broken handler.

use core::arch::global_asm;
use core::fmt::Write;
use core::ptr::{addr_of, addr_of_mut};

use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, PageFaultErrorCode};
use x86_64::VirtAddr;

use crate::process::{self, FaultReg, USER_LAZY_BASE, USER_LAZY_END};
use crate::serial;
use crate::usermode;

/// The faulting context, exactly as `page_fault_entry` lays it out. `gp`
/// is rax, rbx, rcx, rdx, rsi, rdi, rbp, r8..r15 -- the order the stub
/// pushes and `resume_user_trap` pops. The CPU-pushed words follow.
#[repr(C)]
struct RawTrap {
    gp: [u64; 15],
    error_code: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

/// What `resume_user_trap` consumes: the GP registers followed by an iretq
/// frame. Fields are written by `save_trap` and read only by the asm, so
/// Rust sees them as never-read.
#[repr(C)]
#[allow(dead_code)]
struct ResumeFrame {
    gp: [u64; 15],
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

/// The one in-flight faulting context. Written by `save_trap` at delivery,
/// read by `resume_user_trap` at fault_return. The in_fault flag on the
/// process guards against a second fault overwriting it.
static mut SAVED_TRAP: ResumeFrame = ResumeFrame {
    gp: [0; 15],
    rip: 0,
    cs: 0,
    rflags: 0,
    rsp: 0,
    ss: 0,
};

global_asm!(
    r#"
.global page_fault_entry
page_fault_entry:
    // CPU (ring 3 -> ring 0 on RSP0) has pushed: ss, rsp, rflags, cs, rip,
    // error_code -- error_code on top. Push the GP set below it so rsp ends
    // up pointing at a full RawTrap. Push order makes gp[0] = rax (lowest).
    push r15
    push r14
    push r13
    push r12
    push r11
    push r10
    push r9
    push r8
    push rbp
    push rdi
    push rsi
    push rdx
    push rcx
    push rbx
    push rax
    mov rdi, rsp        // &RawTrap
    cld                 // Rust expects DF clear; the fault may have left it set
    sub rsp, 8          // re-establish 16-byte alignment for the call
    call page_fault_dispatch
    ud2                 // page_fault_dispatch never returns

.global deliver_fault_handler
deliver_fault_handler:
    // rdi = handler entry, rsi = handler stack top, rdx = fault address.
    // Build an iretq frame into ring 3, mirroring enter_user_asm; selectors
    // are the same 0x1b/0x23 the GDT asserts. RFLAGS = 0x2 (IF clear).
    mov rax, 0x1b
    push rax            // SS
    push rsi            // RSP (handler's own stack)
    push 0x2            // RFLAGS
    mov rax, 0x23
    push rax            // CS
    push rdi            // RIP (handler entry)
    mov rdi, rdx        // arg0 = fault address
    iretq

.global resume_user_trap
resume_user_trap:
    // rdi = &SAVED_TRAP. Reload the GP set, then iretq back to the faulting
    // instruction with its original rip/rsp/rflags. Pops mirror the stub's
    // push order so each register gets its own saved value back.
    mov rsp, rdi
    pop rax
    pop rbx
    pop rcx
    pop rdx
    pop rsi
    pop rdi
    pop rbp
    pop r8
    pop r9
    pop r10
    pop r11
    pop r12
    pop r13
    pop r14
    pop r15
    iretq
"#
);

extern "C" {
    fn page_fault_entry();
    /// iretq into the ring-3 fault handler. Never returns to the kernel.
    fn deliver_fault_handler(entry: u64, stack_top: u64, fault_addr: u64) -> !;
    /// Restore `frame` and iretq to the faulting instruction. Never returns.
    fn resume_user_trap(frame: *const ResumeFrame) -> !;
}

/// Point the IDT's #PF gate at the naked stub. No IST: a ring-3 #PF uses
/// RSP0 from the TSS, same as the other ring-3-surviving handlers.
pub fn install_page_fault_handler(idt: &mut InterruptDescriptorTable) {
    // SAFETY: page_fault_entry is the naked stub above; it manages the
    // CPU-pushed frame by hand and tail-calls into page_fault_dispatch.
    unsafe {
        idt.page_fault
            .set_handler_addr(VirtAddr::new(page_fault_entry as *const () as u64));
    }
}

/// Copy the faulting context into SAVED_TRAP (dropping the error code,
/// which iretq does not consume).
fn save_trap(raw: &RawTrap) {
    // SAFETY: single CPU, single process; the in_fault guard ensures no
    // other fault is using SAVED_TRAP between here and fault_return.
    unsafe {
        let s = &mut *addr_of_mut!(SAVED_TRAP);
        s.gp = raw.gp;
        s.rip = raw.rip;
        s.cs = raw.cs;
        s.rflags = raw.rflags;
        s.rsp = raw.rsp;
        s.ss = raw.ss;
    }
}

/// Restore the saved faulting context and resume it. Called from
/// sys_fault_return once the handler has mapped the page.
pub fn resume() -> ! {
    // SAFETY: a matching save_trap ran when this fault was delivered (the
    // caller checked in_fault), so SAVED_TRAP holds a valid ring-3 frame.
    unsafe { resume_user_trap(addr_of!(SAVED_TRAP)) }
}

/// The #PF dispatcher. Reached only from `page_fault_entry`.
#[no_mangle]
extern "C" fn page_fault_dispatch(raw: *const RawTrap) -> ! {
    // SAFETY: the stub passes a pointer to the RawTrap it built on the
    // kernel stack; it is valid for the duration of this call.
    let raw = unsafe { &*raw };
    let cr2 = Cr2::read().as_u64();
    let from_user = raw.cs & 3 == 3;

    if from_user {
        let not_present = raw.error_code & PageFaultErrorCode::PROTECTION_VIOLATION.bits() == 0;

        // Decide whether to upcall, under the lock, then release it before
        // leaving CPL 0 (deliver/terminate both abandon this stack frame).
        let deliver = {
            let mut cur = process::CURRENT.lock();
            match cur.as_mut() {
                Some(proc) => match proc.fault {
                    Some(FaultReg { entry, stack_top })
                        if not_present
                            && !proc.in_fault
                            && (USER_LAZY_BASE..USER_LAZY_END).contains(&cr2) =>
                    {
                        proc.in_fault = true;
                        Some((entry, stack_top))
                    }
                    _ => None,
                },
                None => None,
            }
        };

        if let Some((entry, stack_top)) = deliver {
            save_trap(raw);
            // SAFETY: faulted at CPL 3 so no kernel lock is held; this jumps
            // to ring 3 and returns control only via fault_return.
            unsafe { deliver_fault_handler(entry, stack_top, cr2) }
        }

        // Unhandled user fault: log and terminate, identical to any fault.
        let mut serial = serial::init();
        let _ = writeln!(
            serial,
            "plinth: [user fault] #PF page fault rip={:#x} err={:#x} addr={:#x}",
            raw.rip, raw.error_code, cr2
        );
        let _ = writeln!(serial, "plinth: terminating user process");
        // CPL 3 fault, no locks held. exit_current picks the scheduler switch
        // or the synchronous longjmp and never returns.
        process::exit_current(usermode::EXIT_FAULTED)
    }

    // Kernel-mode #PF is a bug -- including the documented CPL-0 user-pointer
    // gap, which the syscall layer is written to keep userspace away from.
    let mut serial = serial::init();
    let _ = writeln!(
        serial,
        "plinth: [KERNEL FAULT] #PF page fault rip={:#x} err={:#x} addr={:#x}",
        raw.rip, raw.error_code, cr2
    );
    panic!("unrecoverable kernel fault: #PF");
}
