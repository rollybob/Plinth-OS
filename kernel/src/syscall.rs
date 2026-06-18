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
//! | 10 | block_read  | rng, frm, sec, count| BLK_OK, or a BLK_E_* code |
//!
//! block_read is the first syscall to take a fourth argument: it arrives in r8
//! (the System V C ABI's fifth register, after nr+three args), which the entry
//! stub never touches, so the dispatcher reads it directly -- no asm change.
//! Its result is a *status* word (the C1 status/payload split applied to block
//! I/O): the data lands in the caller's frame, so no data value can be mistaken
//! for an error.
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

use crate::capability::{
    CapError, CapObject, Capability, RIGHT_CONSUME, RIGHT_MAP, RIGHT_READ, RIGHT_RECV, RIGHT_SEND,
    RIGHT_WRITE,
};
use crate::fault;
use crate::frame_alloc::{FRAME_ALLOC, FRAME_SIZE};
use crate::gdt::Selectors;
use crate::ipc;
use crate::memory;
use crate::process::{self, FaultReg, USER_MAP_BASE, USER_MAP_END};
use crate::scheduler;
use crate::serial;
use crate::usermode;
use crate::virtio_blk;

pub const ERR: u64 = u64::MAX;

/// block_read status words, returned in rax. The data lands in the caller's
/// frame, so status is its own word (the C1 split): no read-back byte can be
/// confused for an error. Mirrored in libplinth as BLK_*.
const BLK_OK: u64 = 0;
/// count is zero, or count*512 would overflow the I/O frame.
const BLK_E_BADARG: u64 = 1;
/// The request falls outside the holder's BlockRange (multiplexing guarantee).
const BLK_E_RANGE: u64 = 2;
/// Bad slot, wrong object kind, or a missing right on the range or frame cap.
const BLK_E_RIGHTS: u64 = 3;
/// The device reported an error or is not initialised.
const BLK_E_DEV: u64 = 4;

/// 512-byte virtio sector -- the unit a BlockRange counts and block_read reads.
const SECTOR_SIZE: u64 = 512;

const MAX_WRITE: u64 = 4096;

const STACK_SIZE: usize = 16 * 4096;

// The field is storage only -- referenced by address, never read as data.
#[repr(align(16))]
struct Stack(#[allow(dead_code)] [u8; STACK_SIZE]);

/// The single kernel stack syscalls run on. One suffices: a syscall always
/// runs to completion before any context switch -- the kernel is
/// non-preemptible, and the blocking IPC operations enter through their own
/// interrupt gate (per-process kernel stacks), not `syscall` -- so this stack
/// is empty whenever another process is scheduled. (Synchronous nested spawn,
/// which needed a stack per depth, is gone: spawn now launches a scheduled
/// process instead.)
static mut SYSCALL_STACK: Stack = Stack([0; STACK_SIZE]);

/// Top of the syscall stack; loaded into rsp by the entry stub.
#[no_mangle]
static mut SYSCALL_STACK_PTR: u64 = 0;

/// User rsp across the syscall. Single CPU; a syscall never yields.
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
        SYSCALL_STACK_PTR = syscall_stack_top();
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

/// Top of the syscall stack.
fn syscall_stack_top() -> u64 {
    // SAFETY: address arithmetic over the static; no reference taken.
    addr_of!(SYSCALL_STACK) as u64 + STACK_SIZE as u64
}

// a3/a4 arrive in rcx/r8 (the 4th/5th C-ABI registers); the entry stub places
// the user's first three args in rsi/rdx/rcx and leaves r8 untouched, so a4 is
// the user's r8. Only block_read uses a4 today.
#[no_mangle]
extern "C" fn syscall_dispatch(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
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
        10 => sys_block_read(a1, a2, a3, a4),
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
    // Reached only from the syscall path, so user code was on the CPU and no
    // locks are held. exit_current picks the right unwind (scheduler switch or
    // the synchronous kernel_resume) and never returns.
    process::exit_current(code & 0xFFFF_FFFF)
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
            // Reached from the syscall path: user code was on the CPU, no
            // locks held. exit_current never returns.
            process::exit_current(usermode::EXIT_OUT_OF_BUDGET)
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

/// spawn(child_id, transfer_slot): launch the embedded child `child_id` as an
/// independent *scheduled* process, and return a handle to wait on its result.
/// This is the reconciliation of spawn with the preemptive scheduler: instead
/// of running the child synchronously nested under the caller, the kernel
/// creates a fresh result endpoint, mints the child a SEND capability to it
/// (at ENDPOINT_SLOT) and the caller a RECV capability (the returned handle),
/// and adds the child to the ready set. The child sends its result and exits;
/// the caller `recv`s the handle to collect it -- that recv IS the wait.
///
/// `transfer_slot` optionally moves one capability from the caller into the
/// child (landing after its endpoint cap); pass `ERR`/`u64::MAX` for none.
/// Returns the handle slot, or ERR. Non-blocking: the child runs concurrently.
fn sys_spawn(child_id: u64, transfer_slot: u64) -> u64 {
    let Some(binary) = process::spawnable(child_id as usize) else {
        return ERR;
    };
    let phys = process::phys_offset();

    // A fresh result channel for this spawn.
    let Some(ep) = ipc::create_endpoint() else {
        return ERR;
    };

    // Optionally move one capability out of the caller into the child.
    let transferred = if transfer_slot != ERR {
        let mut cur = process::CURRENT.lock();
        match cur.as_mut() {
            Some(p) => process::revoke_and_unmap(p, transfer_slot as usize),
            None => None,
        }
    } else {
        None
    };
    // Account the give half of a spawn capability transfer (no-op for a
    // non-endpoint cap; no free -- the child's mint in setup_process re-refs).
    if let Some(ref cap) = transferred {
        ipc::note_cap_removed(cap, false);
    }

    // Child capabilities: a SEND cap to the result endpoint (ENDPOINT_SLOT),
    // then the optional transferred capability (GRANT_SLOT).
    let send_cap = Capability {
        object: CapObject::Endpoint { id: ep },
        rights: RIGHT_SEND,
    };
    if scheduler::spawn(binary, phys, &[Some(send_cap), transferred]).is_none() {
        // Could not create the child: it never minted send_cap, so the result
        // endpoint is unreferenced -- reclaim the slot. Then undo the capability
        // move by re-minting it back to the caller (re-accounting it).
        ipc::release_endpoint(ep);
        if let Some(cap) = transferred {
            let mut cur = process::CURRENT.lock();
            if let Some(p) = cur.as_mut() {
                let _ = p.caps.mint(cap.object, cap.rights);
            }
            ipc::note_cap_added(&cap);
        }
        return ERR;
    }

    // The caller's RECV handle on the result channel; recv on it = wait.
    let recv_cap = Capability { object: CapObject::Endpoint { id: ep }, rights: RIGHT_RECV };
    let handle = {
        let mut cur = process::CURRENT.lock();
        cur.as_mut().and_then(|p| p.caps.mint(recv_cap.object, recv_cap.rights).ok())
    };
    match handle {
        Some(h) => {
            ipc::note_cap_added(&recv_cap);
            h as u64
        }
        None => ERR,
    }
}

/// block_read(range_slot, frame_slot, sector_off, count): read `count` 512-byte
/// sectors -- starting `sector_off` sectors into the BlockRange capability at
/// `range_slot` -- into the frame named by `frame_slot`. The device DMAs the
/// data into the frame; the holder maps that frame to read it. Returns a status
/// word (BLK_OK or a BLK_E_* code), never a data value.
///
/// Two checks make this the exokernel multiplexing surface: the request must
/// fall inside the holder's range (so a BlockRange cannot read another libOS's
/// blocks), and the frame must be the holder's with RIGHT_WRITE (so the device
/// DMAs only into a frame the caller owns). The range start is added by the
/// kernel -- the holder names sectors relative to its range, never absolute.
fn sys_block_read(range_slot: u64, frame_slot: u64, sector_off: u64, count: u64) -> u64 {
    // Bound the transfer: at least one sector, and it must fit the I/O frame.
    if count == 0 || count.saturating_mul(SECTOR_SIZE) > FRAME_SIZE {
        return BLK_E_BADARG;
    }

    // Resolve both capabilities under the CURRENT lock, then drop it before the
    // (polled) device I/O -- nothing the read touches needs CURRENT.
    let (abs_sector, frame_phys) = {
        let cur = process::CURRENT.lock();
        let Some(proc) = cur.as_ref() else {
            return BLK_E_RIGHTS;
        };

        // The BlockRange: RIGHT_READ to read from the disk.
        let Ok(range) = proc.caps.lookup(range_slot as usize, RIGHT_READ) else {
            return BLK_E_RIGHTS;
        };
        let CapObject::BlockRange { start, count: range_count } = range.object else {
            return BLK_E_RIGHTS;
        };
        // Multiplexing guarantee: [sector_off, sector_off+count) must lie inside
        // [0, range_count). Checked-add so a huge sector_off cannot wrap past it.
        let Some(end) = sector_off.checked_add(count) else {
            return BLK_E_RANGE;
        };
        if end > range_count {
            return BLK_E_RANGE;
        }

        // The I/O frame: RIGHT_WRITE, since the device DMAs into it.
        let Ok(frame) = proc.caps.lookup(frame_slot as usize, RIGHT_WRITE) else {
            return BLK_E_RIGHTS;
        };
        let CapObject::Frame { addr } = frame.object else {
            return BLK_E_RIGHTS;
        };

        (start + sector_off, addr)
    };

    match virtio_blk::read(abs_sector, count, frame_phys) {
        Ok(()) => BLK_OK,
        Err(_) => BLK_E_DEV,
    }
}
