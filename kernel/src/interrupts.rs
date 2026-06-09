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

use core::fmt::Write;
use core::ptr::{addr_of, addr_of_mut};

use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::gdt::DOUBLE_FAULT_IST_INDEX;
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
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(DOUBLE_FAULT_IST_INDEX);
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
        // SAFETY: from_user is only true when the CPU was at CPL 3, so no
        // kernel lock is held and the saved kernel context is live.
        unsafe { usermode::kernel_resume(usermode::EXIT_FAULTED) }
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

extern "x86-interrupt" fn page_fault_handler(frame: InterruptStackFrame, err: PageFaultErrorCode) {
    let cr2 = Cr2::read().as_u64();
    handle_fault("#PF page fault", &frame, Some(err.bits()), Some(cr2));
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
