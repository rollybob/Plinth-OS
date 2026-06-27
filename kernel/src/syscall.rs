//! The syscall surface.
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
//! | 11 | spawn_buf   | buf_va, len, slot   | wait handle, or ERR      |
//! | 12 | ring_register | sq_slot, cq_slot, entries | ring cap slot, or ERR |
//! | 13 | ring_submit | ring                | count posted, or ERR     |
//! | 14 | fb_map      | slot, va, info_ptr  | 0, or ERR                |
//!
//! Nr 10 (block_read) was retired in ABI v2.3: a blocking read must suspend and
//! resume with a return value, which needs the full resumable trap frame only an
//! interrupt entry saves -- so it moved to the `int 0x80` gate. In v2.4 that gate
//! op was retired too: block I/O is now the async-ring ABI (nr 12/13 here +
//! ring_wait on the `int 0x80` gate, op 6). See rings.rs / virtio_blk.rs.
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

use crate::bkl;
use crate::capability::{
    CapError, CapObject, Capability, RIGHT_CONSUME, RIGHT_MAP, RIGHT_READ, RIGHT_RECV, RIGHT_SEND,
    RIGHT_WRITE,
};
use crate::fault;
use crate::frame_alloc::{FRAME_ALLOC, FRAME_SIZE};
use crate::gdt::Selectors;
use crate::ipc;
use crate::memory;
use crate::percpu;
use crate::process::{self, FaultReg, USER_MAP_BASE, USER_MAP_END};
use crate::scheduler;
use crate::serial;
use crate::usermode;

pub const ERR: u64 = u64::MAX;

const MAX_WRITE: u64 = 4096;

const STACK_SIZE: usize = 16 * 4096;

// The field is storage only -- referenced by address, never read as data.
#[repr(align(16))]
struct Stack(#[allow(dead_code)] [u8; STACK_SIZE]);

/// One kernel stack per core (Stage B2.2, D6): a syscall always runs to
/// completion before any context switch -- the kernel is non-preemptible,
/// and the blocking IPC operations enter through their own interrupt gate
/// (per-process kernel stacks), not `syscall` -- so each core's own stack
/// here is empty whenever that core is running scheduled (or another
/// core's) work. (Synchronous nested spawn, which needed a stack per depth,
/// is gone: spawn now launches a scheduled process instead.)
static mut SYSCALL_STACKS: [Stack; percpu::MAX_CORES] =
    [const { Stack([0; STACK_SIZE]) }; percpu::MAX_CORES];

global_asm!(
    r#"
.global syscall_entry
syscall_entry:
    // syscall left: rcx = user rip, r11 = user rflags. rsp is still the
    // user's -- switch to the kernel syscall stack before touching memory.
    // gs:[USER_RSP_SAVE]/gs:[STACK_TOP] are PerCpu::user_rsp_save/
    // syscall_stack_top (percpu.rs); GS_BASE points at THIS core's slot
    // (percpu::init), set up before syscall is ever armed on that core, so
    // this is correct even with multiple cores running syscall_entry
    // concurrently (Stage B2.2 -- no swapgs needed, see percpu.rs's module
    // doc).
    mov gs:[{user_rsp_save}], rsp
    mov rsp, gs:[{stack_top}]
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
    mov rsp, gs:[{user_rsp_save}]
    sysretq
"#,
    user_rsp_save = const percpu::USER_RSP_SAVE_OFFSET,
    stack_top = const percpu::SYSCALL_STACK_PTR_OFFSET,
);

extern "C" {
    fn syscall_entry();
}

/// Configure this core's syscall/sysret MSRs: EFER.SCE, STAR (selectors,
/// shared across cores by construction -- gdt::init builds an identical
/// layout on every core), the entry point, and the flag mask. Call once per
/// core (BSP at boot, each AP at bring-up) -- these are per-core MSRs, not
/// shared state. Must run AFTER `percpu::init` has pointed this core's
/// GS_BASE at its own slot, since `syscall_entry` is now `gs:`-relative.
pub fn init(sel: &Selectors) {
    // SAFETY: called once per core, after that core's percpu::init.
    unsafe {
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

/// Top of core `core_id`'s syscall stack, for `percpu::init` to record before
/// `init` (above) arms `syscall_entry` on that core.
pub fn stack_top(core_id: usize) -> u64 {
    // SAFETY: address arithmetic over the static; no reference taken.
    unsafe { addr_of!(SYSCALL_STACKS[core_id]) as u64 + STACK_SIZE as u64 }
}

// Three args suffice for every syscall (the four-arg block_read moved to the
// `int 0x80` gate in v2.3). a3 arrives in rcx, the 4th C-ABI register; the entry
// stub shuffles the user's first three args into rsi/rdx/rcx. spawn_from_buffer
// is the only remaining three-arg syscall.
#[no_mangle]
extern "C" fn syscall_dispatch(nr: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    // BKL (D4): some arms below (sys_exit, sys_cpu_charge's overdraw path,
    // sys_fault_return's success path) diverge several frames deep
    // (process::exit_current / fault::resume) and never reach the release
    // below -- they release the lock themselves at their actual longjmp
    // point (see those functions). Every other arm returns normally here,
    // where the release below covers it.
    bkl::acquire();
    let result = match nr {
        1 => sys_write(a1, a2),
        2 => sys_exit(a1),
        3 => sys_frame_alloc(),
        4 => sys_frame_map(a1, a2),
        5 => sys_frame_free(a1),
        6 => sys_cpu_charge(a1, a2),
        7 => sys_fault_reg(a1, a2),
        8 => sys_fault_return(),
        9 => sys_spawn(a1, a2),
        // nr 10 (block_read) was retired in ABI v2.3: a blocking read needs a
        // resumable trap frame, so block_read moved to the `int 0x80` gate. That
        // gate op was itself retired in v2.4 -- block I/O is now the ring ABI
        // below. The number is left unused.
        11 => sys_spawn_from_buffer(a1, a2, a3),
        // Async completion rings (ABI v2.4, Design/async_rings.md). register and
        // submit are non-blocking, so they ride the fast `syscall` path; the
        // blocking `ring_wait` is on the `int 0x80` gate (op 6, see ipc.rs).
        12 => crate::rings::ring_register(a1, a2, a3),
        13 => crate::rings::ring_submit(a1),
        14 => sys_fb_map(a1, a2, a3),
        _ => ERR,
    };
    unsafe { bkl::release() };
    result
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
            let cur = process::current().lock();
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
    let mut cur = process::current().lock();
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

    let mut cur = process::current().lock();
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

/// fb_map(slot, va, info_ptr): map the framebuffer named by the capability at
/// `slot` into the caller's address space, contiguously from the user-chosen
/// `va` (the exokernel contract, like frame_map), and write its geometry to the
/// FbInfo struct at `info_ptr` (five u32s: width, height, stride,
/// bytes_per_pixel, format). Returns 0, or ERR.
///
/// The mapped pages name firmware MMIO, not pooled frames, so they are NOT
/// recorded in `proc.maps` (which exists to dealloc pooled frames) and are never
/// returned to the allocator -- teardown's `destroy_address_space` frees only
/// the page tables. There can be far more pages than `proc.maps` holds anyway
/// (a 1280x800x4 framebuffer is ~1000 pages).
fn sys_fb_map(slot: u64, va: u64, info_ptr: u64) -> u64 {
    // Resolve the capability (and the process L4) up front, then drop CURRENT
    // before taking FRAME_ALLOC -- the file-wide lock order is CURRENT then
    // FRAME_ALLOC, never nested the other way.
    let (l4, phys_base, width, height, stride, bpp, format) = {
        let cur = process::current().lock();
        let Some(proc) = cur.as_ref() else {
            return ERR;
        };
        let Ok(cap) = proc.caps.lookup(slot as usize, RIGHT_MAP) else {
            return ERR;
        };
        // A non-framebuffer capability (even one that carries RIGHT_MAP, like a
        // Frame) is rejected here: the kind check is the multiplexing/type
        // guard, the display analogue of BlockRange's device+range check.
        let CapObject::Framebuffer { phys_base, width, height, stride, bytes_per_pixel, format } =
            cap.object
        else {
            return ERR;
        };
        (proc.l4, phys_base, width, height, stride, bytes_per_pixel, format)
    };

    // Bytes the whole framebuffer spans, rounded up to a page.
    let map_size = (height as u64) * (stride as u64) * (bpp as u64);
    let span = (map_size + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
    if span == 0 || va % FRAME_SIZE != 0 || va < USER_MAP_BASE {
        return ERR;
    }
    let Some(end) = va.checked_add(span) else {
        return ERR;
    };
    if end > USER_MAP_END {
        return ERR;
    }

    // Validate the FbInfo destination (5 * u32 = 20 bytes) is mapped and
    // user-accessible before the kernel writes it -- the same discipline `write`
    // applies before touching any user pointer.
    const FB_INFO_BYTES: u64 = 20;
    let Some(info_last) = info_ptr.checked_add(FB_INFO_BYTES - 1) else {
        return ERR;
    };
    {
        let mut page = info_ptr & !(FRAME_SIZE - 1);
        loop {
            if !memory::user_accessible(l4, page) {
                return ERR;
            }
            if page >= info_last & !(FRAME_SIZE - 1) {
                break;
            }
            page += FRAME_SIZE;
        }
    }

    // Map the framebuffer pages: writable, user-accessible, non-executable.
    // Cacheable (no NO_CACHE), matching frame_map and the bootloader's own
    // framebuffer mapping -- the GOP framebuffer is WB RAM under QEMU. A real-
    // hardware port would want write-combining here (a later refinement).
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;
    {
        let mut fa_guard = FRAME_ALLOC.lock();
        let Some(fa) = fa_guard.as_mut() else {
            return ERR;
        };
        let mut off = 0u64;
        while off < span {
            if memory::map_user_page(l4, fa, va + off, phys_base + off, flags).is_err() {
                // Roll back the pages mapped before the failure; the page-table
                // frames allocated stay until teardown frees the address space.
                let mut back = 0u64;
                while back < off {
                    memory::unmap_user_page(l4, va + back);
                    back += FRAME_SIZE;
                }
                return ERR;
            }
            off += FRAME_SIZE;
        }
    }

    // Hand the geometry back. SAFETY: [info_ptr, info_ptr+20) was just verified
    // mapped and user-accessible in the active address space; IF is masked and
    // the CPU is single, so nothing can unmap it under us.
    unsafe {
        let p = info_ptr as *mut u32;
        p.add(0).write_volatile(width);
        p.add(1).write_volatile(height);
        p.add(2).write_volatile(stride);
        p.add(3).write_volatile(bpp as u32);
        p.add(4).write_volatile(format as u32);
    }
    0
}

/// frame_free(slot): revoke the capability, unmap any mapping made
/// through it, and return the frame to the allocator.
fn sys_frame_free(slot: u64) -> u64 {
    let mut cur = process::current().lock();
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
        let mut cur = process::current().lock();
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
    let mut cur = process::current().lock();
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
        let mut cur = process::current().lock();
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
    spawn_scheduled(binary, transfer_slot)
}

/// Largest ELF the spawn-from-buffer path accepts, in bytes. The real user
/// binaries are ~7-13 KiB; this is generous headroom while still bounding the
/// page-validation loop and the image the child must fit. A larger buffer is
/// rejected up front.
const MAX_SPAWN_ELF: u64 = 256 * 1024;

/// spawn_from_buffer(buf_va, len, transfer_slot): like `spawn`, but the child's
/// ELF image comes from `len` bytes at `buf_va` in the CALLER's address space
/// (a library OS's buffer -- e.g. an FS libOS that read the bytes off disk),
/// not the kernel's embedded `SPAWNABLE` table. This is the load-from-disk path
/// (ABI v2.x): application binaries live on disk, while embedded `SPAWNABLE`
/// stays as the built-in bootstrap loader (D8b).
///
/// The buffer is untrusted input -- a libOS-supplied ELF can lie about every
/// field -- so it flows through the same audited `elf::parse` validator as
/// every other binary (elf.rs, D8a audit). Before reading it, the kernel checks
/// (exactly as `write` does) that the whole range lies in the user map window
/// and every page is mapped and user-accessible, so a bogus pointer faults the
/// syscall cleanly instead of reading kernel memory. Syscalls run with
/// interrupts masked on a single CPU, so the caller cannot run (or remap) while
/// the kernel copies the bytes into the child's frames.
fn sys_spawn_from_buffer(buf_va: u64, len: u64, transfer_slot: u64) -> u64 {
    if len == 0 || len > MAX_SPAWN_ELF || buf_va % FRAME_SIZE != 0 {
        return ERR;
    }
    let Some(last) = buf_va.checked_add(len - 1) else {
        return ERR;
    };
    if !(USER_MAP_BASE..USER_MAP_END).contains(&buf_va)
        || !(USER_MAP_BASE..USER_MAP_END).contains(&last)
    {
        return ERR;
    }

    // Every page of the buffer must be mapped and user-accessible in the
    // caller's address space (the active one) before the kernel reads it.
    let l4 = {
        let cur = process::current().lock();
        match cur.as_ref() {
            Some(proc) => proc.l4,
            None => return ERR,
        }
    };
    let mut page = buf_va & !(FRAME_SIZE - 1);
    loop {
        if !memory::user_accessible(l4, page) {
            return ERR;
        }
        if page >= last & !(FRAME_SIZE - 1) {
            break;
        }
        page += FRAME_SIZE;
    }

    // SAFETY: every page in [buf_va, buf_va+len) was just verified mapped and
    // user-accessible in the active address space; IF is masked and the CPU is
    // single, so no other process can run to unmap it and the caller is
    // suspended in this syscall. scheduler::spawn consumes the bytes
    // synchronously (it copies the segments into the child's frames) before
    // this returns, so the borrow never outlives the mapping.
    let binary = unsafe { core::slice::from_raw_parts(buf_va as *const u8, len as usize) };

    spawn_scheduled(binary, transfer_slot)
}

/// Shared body of the two spawn syscalls: launch `binary` as an independent,
/// concurrently scheduled process with a fresh result channel, optionally
/// moving one capability from the caller into the child (at GRANT_SLOT), and
/// return the caller's RECV handle on that channel (recv on it IS the wait).
/// Returns the handle slot, or ERR. Non-blocking.
fn spawn_scheduled(binary: &[u8], transfer_slot: u64) -> u64 {
    let phys = process::phys_offset();

    // A fresh result channel for this spawn.
    let Some(ep) = ipc::create_endpoint() else {
        return ERR;
    };

    // Optionally move one capability out of the caller into the child.
    let transferred = if transfer_slot != ERR {
        let mut cur = process::current().lock();
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
            let mut cur = process::current().lock();
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
        let mut cur = process::current().lock();
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

