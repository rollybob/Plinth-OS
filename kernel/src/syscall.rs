//! The syscall surface -- all five of it.
//!
//! | Nr | Name        | Args (RDI, RSI)     | Returns                  |
//! |----|-------------|---------------------|--------------------------|
//! |  1 | write       | ptr, len            | len, or ERR              |
//! |  2 | exit        | code                | (never returns)          |
//! |  3 | frame_alloc | --                  | capability slot, or ERR  |
//! |  4 | frame_map   | slot, va            | 0, or ERR                |
//! |  5 | frame_free  | slot                | 0, or ERR                |
//!
//! This is the whole kernel interface, and that is the point: memory
//! arrives as raw frames through capabilities, and everything resembling
//! an allocator lives in userspace. write is uncapabilitied console
//! output for demo legibility; exit is the synchronous-process model's
//! return statement.
//!
//! Lock order, everywhere in this file: CURRENT, then FRAME_ALLOC, then
//! MAPPER. Single CPU makes violations deadlock instantly, which is a
//! feature -- they cannot survive a smoke run unnoticed.
//!
//! Entry mechanism: syscall/sysret. The entry stub switches to a
//! dedicated kernel stack (syscall does not switch stacks), preserves the
//! user rip/rflags that syscall stashed in rcx/r11, and shuffles the
//! Linux-style argument registers (rax nr; rdi, rsi, rdx args) into the
//! C ABI for the dispatcher.

use core::arch::global_asm;
use core::ptr::addr_of;

use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::structures::paging::PageTableFlags;
use x86_64::VirtAddr;

use crate::capability::{CapObject, RIGHT_MAP, RIGHT_READ, RIGHT_WRITE};
use crate::frame_alloc::{FRAME_ALLOC, FRAME_SIZE};
use crate::gdt::Selectors;
use crate::memory;
use crate::process::{self, USER_MAP_BASE, USER_MAP_END};
use crate::serial;
use crate::usermode;

pub const ERR: u64 = u64::MAX;

const MAX_WRITE: u64 = 4096;

const STACK_SIZE: usize = 16 * 4096;

// The field is storage only -- referenced by address, never read as data.
#[repr(align(16))]
struct Stack(#[allow(dead_code)] [u8; STACK_SIZE]);

static mut SYSCALL_STACK: Stack = Stack([0; STACK_SIZE]);

/// Top of SYSCALL_STACK; loaded into rsp by the entry stub. Set in init().
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
        SYSCALL_STACK_PTR = addr_of!(SYSCALL_STACK) as u64 + STACK_SIZE as u64;
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

#[no_mangle]
extern "C" fn syscall_dispatch(nr: u64, a1: u64, a2: u64, _a3: u64) -> u64 {
    match nr {
        1 => sys_write(a1, a2),
        2 => sys_exit(a1),
        3 => sys_frame_alloc(),
        4 => sys_frame_map(a1, a2),
        5 => sys_frame_free(a1),
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
        let mapper_guard = memory::MAPPER.lock();
        let Some(mapper) = mapper_guard.as_ref() else {
            return ERR;
        };
        let mut page = ptr & !(FRAME_SIZE - 1);
        loop {
            if !memory::user_accessible(mapper, page) {
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
    let Ok(cap) = proc.caps.lookup(slot as usize, RIGHT_MAP) else {
        return ERR;
    };
    let CapObject::Frame { addr } = cap.object;

    let Some(entry) = proc.maps.iter_mut().find(|e| e.is_none()) else {
        return ERR;
    };

    let mut fa_guard = FRAME_ALLOC.lock();
    let Some(fa) = fa_guard.as_mut() else {
        return ERR;
    };
    let mut mapper_guard = memory::MAPPER.lock();
    let Some(mapper) = mapper_guard.as_mut() else {
        return ERR;
    };

    if memory::is_mapped(mapper, va) {
        return ERR;
    }
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;
    if memory::map_user_page(mapper, fa, va, addr, flags).is_err() {
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
    let Ok(cap) = proc.caps.revoke(slot as usize) else {
        return ERR;
    };
    let CapObject::Frame { addr } = cap.object;

    let mut fa_guard = FRAME_ALLOC.lock();
    let Some(fa) = fa_guard.as_mut() else {
        return ERR;
    };
    let mut mapper_guard = memory::MAPPER.lock();
    let Some(mapper) = mapper_guard.as_mut() else {
        return ERR;
    };

    for entry in proc.maps.iter_mut() {
        if let Some((va, s)) = *entry {
            if s == slot as usize {
                memory::unmap_user_page(mapper, va);
                *entry = None;
            }
        }
    }
    let _ = fa.dealloc(addr);
    0
}
