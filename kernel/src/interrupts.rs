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

use crate::bkl;
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

/// Point this core's IDTR at the already-built IDT (Stage B2.2). The table is
/// shared, read-only-after-boot data -- every core just `lidt`s the same
/// table, no per-core copy needed. Call once per AP, after the BSP's `init`
/// has built it.
pub fn load_on_this_core() {
    // SAFETY: IDT_STORAGE was already built by the BSP's `init` and is never
    // mutated again except by `set_irq_handler` (which only writes existing
    // entries, not the table's identity/address) -- `lidt`-ing it from
    // another core is just pointing that core's own IDTR at the same table.
    unsafe {
        (*addr_of!(IDT_STORAGE)).as_ref().expect("IDT not initialised before AP bring-up").load();
    }
}

/// Install a handler at a runtime-determined IRQ `vector`, after the IDT is
/// already built and loaded. Used for device line IRQs whose vector is only
/// known after PCI discovery (the virtio-blk completion lines, Stage 4). The
/// IDTR still points at the same static table, so writing the entry takes effect
/// on the next interrupt -- no reload needed.
pub fn set_irq_handler(vector: u8, handler: extern "x86-interrupt" fn(InterruptStackFrame)) {
    // SAFETY: single CPU; the IDT lives in IDT_STORAGE (built in `init`) and the
    // CPU only reads an entry while dispatching an interrupt, never concurrently
    // with this write (IF=0 here).
    unsafe {
        let idt = (*addr_of_mut!(IDT_STORAGE))
            .as_mut()
            .expect("IDT not initialised before set_irq_handler");
        idt[vector as usize].set_handler_fn(handler);
    }
}

/// Shared fault path. Diverges (via kernel_resume) for ring-3 faults;
/// panics for kernel faults.
fn handle_fault(name: &str, frame: &InterruptStackFrame, err: Option<u64>, addr: Option<u64>) {
    // BKL (D4): this function always diverges -- either into
    // process::exit_current (which releases the lock at its own chokepoint,
    // scheduler::resume_process / switch_to_next / process::exit_current's
    // kernel_resume arm) or into `panic!` (which halts, needing no release).
    // No explicit release here covers either path correctly.
    bkl::acquire();

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
