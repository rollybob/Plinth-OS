//! The syscall surface -- all five of it.
//!
//! | Nr | Name        | Args (RDI, RSI)     | Returns                  |
//! |----|-------------|---------------------|--------------------------|
//! |  1 | write       | ptr, len            | len, or ERR              |
//! |  2 | exit        | code                | (never returns)          |
//! |  3 | frame_alloc | --                  | capability slot, or ERR  |
//! |  4 | frame_map   | slot, va            | 0, or ERR                |
//! |  5 | frame_free  | slot                | 0, or ERR                |
//! |  6 | cpu_charge  | slot, amount        | remaining, or terminates |
//! |  7 | fault_reg   | entry, stack_top    | 0, or ERR                |
//! |  8 | fault_return| --                  | (resumes), or ERR        |
//! |  9 | spawn       | child_id, slot      | child exit code, or ERR  |
//!
//! This is the whole kernel interface, and that is the point: memory
//! arrives as raw frames through capabilities, and everything resembling
//! an allocator lives in userspace. write is uncapabilitied console
//! output for demo legibility; exit is the synchronous-process model's
//! return statement. cpu_charge is the one capability whose object is
//! spent rather than owned: it debits the process's CpuTime budget and,
//! on overdraw, terminates the process the same way a fault does.
//! fault_reg/fault_return are the self-paging pair: register a ring-3 #PF
//! handler, and return from it to resume the faulting instruction (see
//! the `fault` module).
//!
//! Lock order, everywhere in this file: CURRENT, then FRAME_ALLOC. Page
//! tables are per-process now (memory.rs) and reached through the current
//! process's L4, so there is no global mapper lock; single-CPU execution
//! serialises the transient page-table views. Single CPU also makes a lock
//! order violation deadlock instantly -- a feature, not a hazard.
//!
//! Entry mechanism: syscall/sysret. The entry stub switches to a
//! dedicated kernel stack (syscall does not switch stacks), preserves the
//! user rip/rflags that syscall stashed in rcx/r11, and shuffles the
//! Linux-style argument registers (rax nr; rdi, rsi, rdx args) into the
//! C ABI for the dispatcher.

use core::arch::global_asm;
use core::fmt::Write;
use core::ptr::addr_of;

use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::structures::paging::PageTableFlags;
use x86_64::VirtAddr;

use crate::capability::{CapError, CapObject, RIGHT_CONSUME, RIGHT_MAP, RIGHT_READ, RIGHT_WRITE};
use crate::fault;
use crate::frame_alloc::{FRAME_ALLOC, FRAME_SIZE};
use crate::gdt::Selectors;
use crate::memory;
use crate::process::{self, FaultReg, USER_MAP_BASE, USER_MAP_END};
use crate::serial;
use crate::usermode;

pub const ERR: u64 = u64::MAX;

const MAX_WRITE: u64 = 4096;

const STACK_SIZE: usize = 16 * 4096;

/// Maximum spawn nesting. Each level runs on its own syscall stack so a
/// child's syscalls never clobber the parent's suspended kernel frame --
/// the irreducible cost of synchronous spawn-with-return. Fixed, no heap;
/// depth 0 is the top-level loop.
pub const MAX_SPAWN_DEPTH: usize = 4;

// The field is storage only -- referenced by address, never read as data.
#[repr(align(16))]
struct Stack(#[allow(dead_code)] [u8; STACK_SIZE]);

// One syscall stack per nesting level, laid out contiguously so a level's
// top is base + (level + 1) * STACK_SIZE.
static mut SYSCALL_STACKS: [Stack; MAX_SPAWN_DEPTH] =
    [const { Stack([0; STACK_SIZE]) }; MAX_SPAWN_DEPTH];

/// Current spawn nesting depth; selects the active syscall stack.
static mut SPAWN_DEPTH: usize = 0;

/// Top of the active syscall stack; loaded into rsp by the entry stub.
#[no_mangle]
static mut SYSCALL_STACK_PTR: u64 = 0;

/// User rsp across the syscall. Single CPU, no nesting.
#[no_mangle]
static mut USER_RSP_SAVE: u64 = 0;

global_asm!(
    r#"
.global syscall_entry
syscall_entry:
    // syscall left: rcx = user rip, r11 = user rflags. rsp is still the
    // user's -- switch to the kernel syscall stack before touching memory.
    mov [rip + USER_RSP_SAVE], rsp
    mov rsp, [rip + SYSCALL_STACK_PTR]
    push rcx
    push r11

    // (rax, rdi, rsi, rdx) -> C ABI (rdi, rsi, rdx, rcx). Each move reads
    // a register whose old value has already been consumed.
    mov rcx, rdx
    mov rdx, rsi
    mov rsi, rdi
    mov rdi, rax
    call syscall_dispatch

    // rax carries the return value through sysretq untouched.
    pop r11
    pop rcx
    mov rsp, [rip + USER_RSP_SAVE]
    sysretq
"#
);

extern "C" {
    fn syscall_entry();
}

pub fn init(sel: &Selectors) {
    // SAFETY: single-threaded boot; MSR writes configure syscall entry
    // exactly once, with selectors asserted by gdt::init.
    unsafe {
        SYSCALL_STACK_PTR = syscall_stack_top(0);
        Efer::update(|f| f.insert(EferFlags::SYSTEM_CALL_EXTENSIONS));
        Star::write(sel.ucode, sel.udata, sel.kcode, sel.kdata)
            .expect("GDT selector layout incompatible with STAR");
        LStar::write(VirtAddr::new(syscall_entry as *const () as u64));
        // Mask IF/TF/DF/AC on entry so handlers run with a clean, known
        // flag state; sysretq restores the user's flags from r11.
        SFMask::write(
            RFlags::INTERRUPT_FLAG
                | RFlags::TRAP_FLAG
                | RFlags::DIRECTION_FLAG
                | RFlags::ALIGNMENT_CHECK,
        );
    }
}

/// Top of the syscall stack for nesting level `depth`.
fn syscall_stack_top(depth: usize) -> u64 {
    // SAFETY: address arithmetic over the static array; no reference taken.
    let base = addr_of!(SYSCALL_STACKS) as u64;
    base + (depth as u64 + 1) * STACK_SIZE as u64
}

/// Nesting depth of the currently running process.
fn current_spawn_depth() -> usize {
    // SAFETY: scalar read of a single-CPU static.
    unsafe { SPAWN_DEPTH }
}

/// Run a child at `entry`/`stack` one nesting level down, in its own address
/// space (`child_l4`), on its own syscall stack. Saves every transient
/// global the parent's syscall-return path depends on -- the kernel-resume
/// anchor, the saved user rsp, the syscall stack pointer, the depth, and the
/// active address space -- and restores them once the child exits, because
/// the child's own execution overwrites all of them. `return_l4` is the
/// parent's address space, made active again before control returns.
///
/// # Safety
/// `child_depth` must be < MAX_SPAWN_DEPTH, the child must be installed as
/// CURRENT, and the parent must be suspended with its frame live on the
/// parent's syscall stack.
unsafe fn spawn_enter(
    entry: u64,
    stack: u64,
    child_depth: usize,
    child_l4: u64,
    return_l4: u64,
) -> u64 {
    let saved_anchor = usermode::kernel_anchor();
    let saved_user_rsp = USER_RSP_SAVE;
    let saved_stack_ptr = SYSCALL_STACK_PTR;
    let saved_depth = SPAWN_DEPTH;

    SPAWN_DEPTH = child_depth;
    SYSCALL_STACK_PTR = syscall_stack_top(child_depth);
    memory::switch_to(child_l4);

    let raw = usermode::enter_user(entry, stack);

    memory::switch_to(return_l4);
    SPAWN_DEPTH = saved_depth;
    SYSCALL_STACK_PTR = saved_stack_ptr;
    USER_RSP_SAVE = saved_user_rsp;
    usermode::set_kernel_anchor(saved_anchor);
    raw
}

#[no_mangle]
extern "C" fn syscall_dispatch(nr: u64, a1: u64, a2: u64, _a3: u64) -> u64 {
    match nr {
        1 => sys_write(a1, a2),
        2 => sys_exit(a1),
        3 => sys_frame_alloc(),
        4 => sys_frame_map(a1, a2),
        5 => sys_frame_free(a1),
        6 => sys_cpu_charge(a1, a2),
        7 => sys_fault_reg(a1, a2),
        8 => sys_fault_return(),
        9 => sys_spawn(a1, a2),
        _ => ERR,
    }
}

/// write(ptr, len): copy bytes from validated user memory to the serial
/// console. Every touched page must be mapped USER_ACCESSIBLE -- the
/// kernel never dereferences a user pointer it has not checked against
/// the page tables.
fn sys_write(ptr: u64, len: u64) -> u64 {
    if len == 0 {
        return 0;
    }
    if len > MAX_WRITE {
        return ERR;
    }
    let Some(last) = ptr.checked_add(len - 1) else {
        return ERR;
    };

    {
        let l4 = {
            let cur = process::CURRENT.lock();
            match cur.as_ref() {
                Some(proc) => proc.l4,
                None => return ERR,
            }
        };
        let mut page = ptr & !(FRAME_SIZE - 1);
        loop {
            if !memory::user_accessible(l4, page) {
                return ERR;
            }
            if page >= last & !(FRAME_SIZE - 1) {
                break;
            }
            page += FRAME_SIZE;
        }
    }

    let mut serial = serial::init();
    for i in 0..len {
        // SAFETY: every page in [ptr, ptr+len) was just verified mapped
        // and user-accessible; nothing can unmap it mid-loop (single CPU,
        // no preemption in kernel mode).
        let byte = unsafe { ((ptr + i) as *const u8).read_volatile() };
        serial.send(byte);
    }
    len
}

/// exit(code): never returns to the caller -- control resumes in
/// process::run on the kernel side.
fn sys_exit(code: u64) -> u64 {
    // SAFETY: reached only from the syscall path, so user code was on the
    // CPU and the saved kernel context is live. No locks are held here.
    unsafe { usermode::kernel_resume(code & 0xFFFF_FFFF) }
}

/// frame_alloc(): allocate one physical frame and mint a capability for
/// it in the calling process's table. Returns the slot index.
fn sys_frame_alloc() -> u64 {
    let mut cur = process::CURRENT.lock();
    let Some(proc) = cur.as_mut() else {
        return ERR;
    };
    let mut fa_guard = FRAME_ALLOC.lock();
    let Some(fa) = fa_guard.as_mut() else {
        return ERR;
    };
    let Ok(addr) = fa.alloc() else {
        return ERR;
    };
    match proc.caps.mint(CapObject::Frame { addr }, RIGHT_READ | RIGHT_WRITE | RIGHT_MAP) {
        Ok(slot) => slot as u64,
        Err(_) => {
            let _ = fa.dealloc(addr);
            ERR
        }
    }
}

/// frame_map(slot, va): map the frame named by the capability at the
/// user-chosen virtual address. The user picks the address -- that is the
/// exokernel contract -- and the kernel checks only that it is aligned,
/// inside the user mapping window, and not already in use.
fn sys_frame_map(slot: u64, va: u64) -> u64 {
    if va % FRAME_SIZE != 0 || !(USER_MAP_BASE..USER_MAP_END).contains(&va) {
        return ERR;
    }

    let mut cur = process::CURRENT.lock();
    let Some(proc) = cur.as_mut() else {
        return ERR;
    };
    let l4 = proc.l4;
    let Ok(cap) = proc.caps.lookup(slot as usize, RIGHT_MAP) else {
        return ERR;
    };
    // A CpuTime capability never carries RIGHT_MAP, so lookup above already
    // rejects it; this binding only ever sees a Frame in practice.
    let CapObject::Frame { addr } = cap.object else {
        return ERR;
    };

    let Some(entry) = proc.maps.iter_mut().find(|e| e.is_none()) else {
        return ERR;
    };

    let mut fa_guard = FRAME_ALLOC.lock();
    let Some(fa) = fa_guard.as_mut() else {
        return ERR;
    };

    if memory::is_mapped(l4, va) {
        return ERR;
    }
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;
    if memory::map_user_page(l4, fa, va, addr, flags).is_err() {
        return ERR;
    }

    *entry = Some((va, slot as usize));
    0
}

/// frame_free(slot): revoke the capability, unmap any mapping made
/// through it, and return the frame to the allocator.
fn sys_frame_free(slot: u64) -> u64 {
    let mut cur = process::CURRENT.lock();
    let Some(proc) = cur.as_mut() else {
        return ERR;
    };
    // Only frames are freeable this way. Check the type with a no-rights
    // lookup *before* revoking, so a frame_free aimed at a non-frame slot
    // (e.g. the CpuTime budget) fails without destroying the capability.
    match proc.caps.lookup(slot as usize, 0) {
        Ok(cap) if matches!(cap.object, CapObject::Frame { .. }) => {}
        _ => return ERR,
    }
    let Ok(cap) = proc.caps.revoke(slot as usize) else {
        return ERR;
    };
    let CapObject::Frame { addr } = cap.object else {
        return ERR;
    };
    let l4 = proc.l4;

    let mut fa_guard = FRAME_ALLOC.lock();
    let Some(fa) = fa_guard.as_mut() else {
        return ERR;
    };

    for entry in proc.maps.iter_mut() {
        if let Some((va, s)) = *entry {
            if s == slot as usize {
                memory::unmap_user_page(l4, va);
                *entry = None;
            }
        }
    }
    let _ = fa.dealloc(addr);
    0
}

/// cpu_charge(slot, amount): debit `amount` CPU ticks from the CpuTime
/// capability at `slot`, returning the remaining budget. The libOS reads
/// that return to pace itself -- that is the policy half of the contract,
/// and it lives in userspace. The kernel keeps only the mechanism: a
/// process that charges more than it holds has tried to consume CPU it has
/// no capability for, so the kernel terminates it and reclaims it exactly
/// as it does a faulting process. There is deliberately no recoverable
/// error return for overdraw.
///
/// Caveat (documented in the README too): with no timer, enforcement is
/// cooperative. A process that spins without ever calling cpu_charge is
/// never debited -- preemptive enforcement is what the timer interrupt is
/// for, and that is out of scope by design.
fn sys_cpu_charge(slot: u64, amount: u64) -> u64 {
    // Take, use, and release the CURRENT lock entirely inside this block:
    // the overdraw path longjmps out via kernel_resume, which never runs
    // Drop, so no lock may be held when we reach it.
    let result = {
        let mut cur = process::CURRENT.lock();
        let Some(proc) = cur.as_mut() else {
            return ERR;
        };
        proc.caps.charge(slot as usize, amount, RIGHT_CONSUME)
    };

    match result {
        Ok(remaining) => remaining,
        Err(CapError::Insufficient) => {
            // serial::init() takes a fresh, lock-free handle (same as the
            // panic handler), so this holds nothing across kernel_resume.
            let mut serial = serial::init();
            let _ = writeln!(serial, "plinth: [out of budget] terminating user process");
            // SAFETY: reached from the syscall path, so user code was on
            // the CPU and the saved kernel context is live; no locks held.
            unsafe { usermode::kernel_resume(usermode::EXIT_OUT_OF_BUDGET) }
        }
        Err(_) => ERR,
    }
}

/// fault_reg(entry, stack_top): register a ring-3 page-fault handler for
/// this process's lazy region. A later not-present fault there is delivered
/// to `entry`, running on `stack_top`, instead of terminating the process.
/// Both must be non-zero; the kernel does not otherwise vet them -- a bad
/// handler simply faults, which (being a nested fault) terminates the
/// process. That is the process harming only itself.
fn sys_fault_reg(entry: u64, stack_top: u64) -> u64 {
    if entry == 0 || stack_top == 0 {
        return ERR;
    }
    let mut cur = process::CURRENT.lock();
    let Some(proc) = cur.as_mut() else {
        return ERR;
    };
    proc.fault = Some(FaultReg { entry, stack_top });
    0
}

/// fault_return(): resume the instruction that faulted into the handler.
/// Only valid while a fault is being serviced; otherwise ERR. On success it
/// does not return to the handler -- control resumes in the faulting code.
fn sys_fault_return() -> u64 {
    {
        let mut cur = process::CURRENT.lock();
        match cur.as_mut() {
            Some(proc) if proc.in_fault => proc.in_fault = false,
            _ => return ERR,
        }
    }
    // SAFETY: in_fault was set, so SAVED_TRAP holds the faulting context;
    // the guard is dropped above, so no lock is held across the resume.
    fault::resume()
}

/// spawn(child_id, slot): run an embedded child binary to completion in a
/// fresh, isolated address space, transferring the capability at `slot` from
/// the caller into the child (where it lands at GRANT_SLOT). Returns the
/// child's exit code, or ERR if the child faulted, overran its budget, or
/// the spawn could not be set up. The child runs synchronously, one nesting
/// level down, on its own syscall stack and its own page tables.
fn sys_spawn(child_id: u64, slot: u64) -> u64 {
    let Some(binary) = process::spawnable(child_id as usize) else {
        return ERR;
    };
    let child_depth = current_spawn_depth() + 1;
    if child_depth >= MAX_SPAWN_DEPTH {
        return ERR;
    }
    // The parent's address space, to restore when the child returns.
    let parent_l4 = {
        let cur = process::CURRENT.lock();
        match cur.as_ref() {
            Some(parent) => parent.l4,
            None => return ERR,
        }
    };

    // Build the child's address space and load its image into it.
    let Ok(child_l4) = memory::create_address_space() else {
        return ERR;
    };
    let phys = process::phys_offset();
    let mut boot_frames: [Option<(u64, u64)>; process::MAX_BOOT_FRAMES] =
        [None; process::MAX_BOOT_FRAMES];
    if process::load_and_map(binary, phys, child_l4, &mut boot_frames).is_err() {
        free_child(child_l4, &boot_frames);
        return ERR;
    }

    // Commit: move the granted capability out of the parent's table.
    let revoked = {
        let mut cur = process::CURRENT.lock();
        cur.as_mut().and_then(|p| p.caps.revoke(slot as usize).ok())
    };
    let Some(transferred) = revoked else {
        free_child(child_l4, &boot_frames);
        return ERR;
    };

    // Suspend the parent (it stays in this stack frame, on the parent's
    // syscall stack, which the child never touches) and install the child.
    let parent = process::CURRENT.lock().take();
    let mut child = process::spawn_process(Some(transferred));
    child.l4 = child_l4;
    *process::CURRENT.lock() = Some(child);

    // Run the child to completion; spawn_enter switches address space and
    // syscall stack down and back.
    // SAFETY: child_depth < MAX_SPAWN_DEPTH; the child is CURRENT and the
    // parent is parked in this frame.
    let raw = unsafe {
        spawn_enter(
            process::USER_CODE_VA,
            process::USER_STACK_TOP,
            child_depth,
            child_l4,
            parent_l4,
        )
    };

    // Reclaim the child and its address space, then restore the parent.
    let child = process::CURRENT.lock().take().expect("child vanished during spawn");
    process::teardown(child, &boot_frames);
    memory::destroy_address_space(child_l4);
    *process::CURRENT.lock() = parent;

    match raw {
        usermode::EXIT_FAULTED | usermode::EXIT_OUT_OF_BUDGET => ERR,
        code => code,
    }
}

/// Tear down a half-built child address space (mapped image + page tables)
/// when a spawn aborts before the child runs.
fn free_child(child_l4: u64, boot_frames: &[Option<(u64, u64)>]) {
    let mut throwaway = process::Process::new();
    throwaway.l4 = child_l4;
    process::teardown(throwaway, boot_frames);
    memory::destroy_address_space(child_l4);
}
