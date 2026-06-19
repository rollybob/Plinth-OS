//! IDT and exception handling.
//!
//! Every handler distinguishes ring-3 faults from kernel faults by the
//! CPL in the saved CS. A user fault logs and terminates the process via
//! kernel_resume -- the kernel survives anything userspace does. A kernel
//! fault is a bug: log and panic.
//!
//! Known gap, accepted for a toy: a #PF taken at CPL 0 while a syscall
//! handler dereferences a user pointer takes the kernel-fatal path. The
//! syscall layer validates user pointers against the page tables first,
//! so a user process cannot steer the kernel into that path.
//!
//! The #PF gate is the exception: it points at a naked stub in `fault`,
//! which captures the full register context so a user fault in a process's
//! lazy region can be delivered to a ring-3 handler (self-paging) and the
//! faulting instruction resumed. Every other exception goes through the
//! shared `handle_fault` path below.

use core::fmt::Write;
use core::ptr::{addr_of, addr_of_mut};

use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};

use crate::fault;
use crate::gdt::DOUBLE_FAULT_IST_INDEX;
use crate::process;
use crate::serial;
use crate::usermode;

static mut IDT_STORAGE: Option<InterruptDescriptorTable> = None;

pub fn init() {
    // SAFETY: single-threaded early boot; the IDT lives in a static and
    // is never modified after load.
    unsafe {
        let idt = (*addr_of_mut!(IDT_STORAGE)).insert(InterruptDescriptorTable::new());
        idt.divide_error.set_handler_fn(divide_error_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
        idt.stack_segment_fault.set_handler_fn(stack_segment_handler);
        idt.general_protection_fault.set_handler_fn(general_protection_handler);
        // #PF gets a naked stub (full register capture) for self-paging.
        fault::install_page_fault_handler(idt);
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(DOUBLE_FAULT_IST_INDEX);
        // IRQ0 (the PIT timer): the scheduler installs its naked context-switch
        // stub at the remapped vector. Installed now; the timer is armed and
        // fires only later, and only in ring 3 (see `timer` / `scheduler`).
        crate::scheduler::register(idt);
        // The blocking IPC operations enter through a software-interrupt gate
        // (vector 0x80, DPL 3) so they save a full trap frame -- see `ipc`.
        crate::ipc::register(idt);
        // IRQ1 (the i8042 keyboard), at the remapped vector. Installed now; the
        // device is brought up and the line unmasked later, on the boot path.
        crate::keyboard::register(idt);
        (*addr_of!(IDT_STORAGE)).as_ref().unwrap().load();
    }
}

/// Shared fault path. Diverges (via kernel_resume) for ring-3 faults;
/// panics for kernel faults.
fn handle_fault(name: &str, frame: &InterruptStackFrame, err: Option<u64>, addr: Option<u64>) {
    // The low two bits of the saved CS are the CPL at the time of the
    // fault; 3 means the fault came from user code.
    let from_user = frame.code_segment & 3 == 3;

    let mut serial = serial::init();
    let _ = write!(
        serial,
        "plinth: [{}] {} rip={:#x}",
        if from_user { "user fault" } else { "KERNEL FAULT" },
        name,
        frame.instruction_pointer.as_u64(),
    );
    if let Some(e) = err {
        let _ = write!(serial, " err={:#x}", e);
    }
    if let Some(a) = addr {
        let _ = write!(serial, " addr={:#x}", a);
    }
    let _ = writeln!(serial);

    if from_user {
        let _ = writeln!(serial, "plinth: terminating user process");
        // from_user means the CPU was at CPL 3, so no kernel lock is held and
        // the saved context is live. exit_current never returns.
        process::exit_current(usermode::EXIT_FAULTED)
    }
    panic!("unrecoverable kernel fault: {name}");
}

extern "x86-interrupt" fn divide_error_handler(frame: InterruptStackFrame) {
    handle_fault("#DE divide error", &frame, None, None);
}

extern "x86-interrupt" fn invalid_opcode_handler(frame: InterruptStackFrame) {
    handle_fault("#UD invalid opcode", &frame, None, None);
}

extern "x86-interrupt" fn stack_segment_handler(frame: InterruptStackFrame, err: u64) {
    handle_fault("#SS stack segment", &frame, Some(err), None);
}

extern "x86-interrupt" fn general_protection_handler(frame: InterruptStackFrame, err: u64) {
    handle_fault("#GP general protection", &frame, Some(err), None);
}

extern "x86-interrupt" fn double_fault_handler(frame: InterruptStackFrame, _err: u64) -> ! {
    let mut serial = serial::init();
    let _ = writeln!(
        serial,
        "plinth: [KERNEL FAULT] #DF double fault rip={:#x}",
        frame.instruction_pointer.as_u64(),
    );
    panic!("double fault");
}
