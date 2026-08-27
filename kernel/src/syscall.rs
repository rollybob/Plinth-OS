//! The syscall surface.
//!
//! | Nr | Name        | Args (RDI, RSI)     | Returns                  |
//! |----|-------------|---------------------|--------------------------|
//! |  1 | write       | ptr, len            | len, or ERR              |
//! |  2 | exit        | code                | (never returns)          |
//! |  3 | frame_alloc | --                  | capability slot, or ERR  |
//! |  4 | frame_map   | slot, va            | 0, or ERR                |
//! |  6 | cpu_charge  | slot, amount        | remaining, or terminates |
//! |  7 | fault_reg   | entry, stack_top    | 0, or ERR                |
//! |  8 | fault_return| --                  | (resumes), or ERR        |
//! |  9 | spawn       | child_id, slot      | child exit code, or ERR  |
//! | 11 | spawn_buf   | buf_va, len, slot   | wait handle, or ERR      |
//! | 12 | ring_register | sq_slot, cq_slot, entries | ring cap slot, or ERR |
//! | 13 | ring_submit | ring                | count posted, or ERR     |
//! | 14 | fb_map      | slot, va, info_ptr  | 0, or ERR                |
//! | 15 | cap_release | slot                | 0, or ERR                |
//! | 16 | ring_dropped| ring, user_data     | drop count, or ERR       |
//! | 17 | bind_device | slot, va, info_ptr  | 0, or ERR                |
//!
//! Nr 5 (frame_free) was retired in ABI v2.8: `cap_release` generalises it to
//! every capability kind, and a frame release is exactly what it used to do.
//! `libplinth::sys_frame_free` survives as a wrapper.
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
use crate::capability;
use crate::capability::{
    CapError, CapObject, Capability, Origin, RIGHT_CONSUME, RIGHT_MAP, RIGHT_READ, RIGHT_RECV,
    RIGHT_SEND, RIGHT_WRITE,
};
use crate::fault;
use crate::frame_alloc::{FRAME_ALLOC, FRAME_SIZE};
use crate::gdt::Selectors;
use crate::ipc;
use crate::memory;
use crate::percpu;
use crate::process::{self, FaultReg, USER_MAP_BASE, USER_MAP_END};
use crate::scheduler;
use crate::console;
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
        // nr 5 (frame_free) was retired in ABI v2.8: cap_release (nr 15)
        // generalises it to every capability kind. The number is left unused.
        6 => sys_cpu_charge(a1, a2),
        7 => sys_fault_reg(a1, a2),
        8 => sys_fault_return(),
        9 => sys_spawn(a1, a2),
        // nr 10 (block_read) was retired in ABI v2.3: a blocking read needs a
        // resumable trap frame, so block_read moved to the `int 0x80` gate. That
        // gate op was itself retired in v2.4 -- block I/O is now the ring ABI
        // below. The number is left unused.
        11 => sys_spawn_from_buffer(a1, a2, a3),
        // Async completion rings (ABI v2.4). register and
        // submit are non-blocking, so they ride the fast `syscall` path; the
        // blocking `ring_wait` is on the `int 0x80` gate (op 6, see ipc.rs).
        12 => crate::rings::ring_register(a1, a2, a3),
        13 => crate::rings::ring_submit(a1),
        14 => sys_fb_map(a1, a2, a3),
        15 => sys_cap_release(a1),
        // Read the sticky per-subscription dropped-event count (ABI v2.11). A
        // non-blocking, read-only query, so it rides the fast syscall path like
        // register/submit; owner-scoping and the subscription lookup live in
        // rings::dropped_for.
        16 => crate::rings::dropped_for(a1, a2),
        // Direct-binding slice 3: map a bound device's doorbell + used ring + data
        // buffer into a library OS. Non-blocking (it maps and returns), so it rides
        // the fast syscall path.
        17 => sys_bind_device(a1, a2, a3),
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
        // Reserve capacity before mapping anything. `fb_maps` is a fixed array,
        // and discovering it full after ~1000 pages are already in place would
        // leave either an untracked mapping -- the exact defect D1 closes -- or
        // a rollback path to get wrong. The BKL is held across the whole
        // syscall, so a record free here is still free at the insert below.
        if proc.fb_maps.iter().all(|e| e.is_some()) {
            return ERR;
        }
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

    // Track the mapping under its capability, so that losing the capability --
    // by transfer or by cap_release -- tears the mapping down with it
    // (D1/D2). One record covers the whole contiguous run.
    {
        let mut cur = process::current().lock();
        let Some(proc) = cur.as_mut() else {
            return ERR;
        };
        let record = process::FbMap {
            va_base: va,
            pages: (span / FRAME_SIZE) as u32,
            slot: slot as usize,
        };
        if !process::fb_record(&mut proc.fb_maps, record) {
            // Unreachable: capacity was checked above under the same BKL hold.
            // Unmap rather than leave a mapping the kernel has lost track of --
            // an untracked mapping is precisely the bug being closed.
            let mut off = 0u64;
            while off < span {
                memory::unmap_user_page(l4, va + off);
                off += FRAME_SIZE;
            }
            return ERR;
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

/// bind_device(slot, va, info_ptr): map the directly-bound device named by the
/// capability at `slot` into the caller -- its notify register page (uncached, the
/// doorbell) at `va`, its used ring at `va + 4096`, and a read buffer at
/// `va + 8192` -- and write the queue size (one u32) to `info_ptr`. The kernel has
/// pre-written a read of sector 0 into the buffer; the library OS rings the
/// doorbell (a store to `va`), polls the used ring for the completion, and reads
/// the buffer. Returns 0, or ERR.
///
/// Direct-binding slice 3 (D4/D5): the doorbell is exposed as a mapped MMIO page
/// only because it was verified isolated from the control registers (slice 3a);
/// the used ring drains in the library OS. The descriptor write is still the
/// kernel's here -- the library OS writes its own in slice 4. The mapped pages name
/// the device's own MMIO/frames, not pooled frames, so (like `fb_map`) they are
/// not recorded in `proc.maps` and teardown frees only the page tables.
fn sys_bind_device(slot: u64, va: u64, info_ptr: u64) -> u64 {
    let (l4, dev) = {
        let cur = process::current().lock();
        let Some(proc) = cur.as_ref() else {
            return ERR;
        };
        let Ok(cap) = proc.caps.lookup(slot as usize, RIGHT_MAP) else {
            return ERR;
        };
        let CapObject::BoundDevice { dev } = cap.object else {
            return ERR;
        };
        (proc.l4, dev as usize)
    };

    // Three contiguous user pages: notify (uncached MMIO), used ring, data buffer.
    const PAGES: u64 = 3;
    let span = PAGES * FRAME_SIZE;
    if va % FRAME_SIZE != 0 || va < USER_MAP_BASE {
        return ERR;
    }
    let Some(end) = va.checked_add(span) else {
        return ERR;
    };
    if end > USER_MAP_END {
        return ERR;
    }
    // The info destination (one u32) must be mapped user-accessible, both ends.
    let Some(info_last) = info_ptr.checked_add(3) else {
        return ERR;
    };
    if !memory::user_accessible(l4, info_ptr & !(FRAME_SIZE - 1))
        || !memory::user_accessible(l4, info_last & !(FRAME_SIZE - 1))
    {
        return ERR;
    }

    // Pre-write the read descriptor and learn which pages to map (no doorbell rung).
    let Some(bm) = crate::virtio_blk::bind_prime(dev) else {
        return ERR;
    };

    // notify page: uncached (device MMIO); used ring + data: cacheable RAM. All
    // writable, user-accessible, non-executable.
    let uncached = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_CACHE
        | PageTableFlags::NO_EXECUTE;
    let cached = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;
    let maps = [
        (va, bm.notify_page_phys, uncached),
        (va + FRAME_SIZE, bm.used_phys, cached),
        (va + 2 * FRAME_SIZE, bm.data_phys, cached),
    ];
    {
        let mut fa_guard = FRAME_ALLOC.lock();
        let Some(fa) = fa_guard.as_mut() else {
            return ERR;
        };
        let mut done = 0usize;
        for &(dst, phys, flags) in &maps {
            if memory::map_user_page(l4, fa, dst, phys, flags).is_err() {
                // Roll back the pages mapped before the failure.
                for &(back, _, _) in &maps[..done] {
                    memory::unmap_user_page(l4, back);
                }
                return ERR;
            }
            done += 1;
        }
    }

    // Hand back the queue size. SAFETY: info_ptr's pages were verified above; IF is
    // masked and the CPU is single, so nothing unmaps them under us.
    unsafe {
        core::ptr::write_volatile(info_ptr as *mut u32, bm.qsize as u32);
    }
    0
}

/// cap_release(slot): give a capability back. Revoke it, release whatever
/// pooled kernel resource it owned, and leave the slot free for reuse.
///
/// A capability table is a fixed 16-slot array with no heap behind it, so a
/// slot is a real resource and a process that cannot return one can run out.
/// Until v2.8 the only way to empty a slot was `frame_free`, which type-checked
/// for `Frame` -- so an endpoint capability a process was *done* with (the
/// spawn wait handle, after the join) was stuck there for the process's whole
/// life. That is the leak `shell-user` hit: one slot per app launch, table full
/// after ~9.
///
/// The per-kind decision is `capability::release_action`, shared with
/// `process::teardown` so the two cannot drift. No rights are required:
/// rights gate use, not removal (the same rule `CapTable::revoke` states, and
/// the reason the type lookup below passes a mask of 0).
///
/// Returns 0, or ERR for a bad slot, an empty slot, or a `Reply` capability
/// (releasing one would strand a caller blocked awaiting that reply -- D3).
fn sys_cap_release(slot: u64) -> u64 {
    let mut cur = process::current().lock();
    let Some(proc) = cur.as_mut() else {
        return ERR;
    };

    // Decide BEFORE revoking, so a release the kernel refuses (a Reply cap)
    // leaves the capability intact rather than destroying it on the way to
    // reporting the error. Same ordering discipline the retired frame_free
    // used for its type check, generalised.
    let Ok(cap) = proc.caps.lookup(slot as usize, 0) else {
        return ERR;
    };
    let action = capability::release_action(&cap);
    if action == capability::ReleaseAction::Refuse {
        return ERR;
    }
    if proc.caps.revoke(slot as usize).is_err() {
        return ERR;
    }

    match action {
        capability::ReleaseAction::FreeFrame { addr } => {
            let l4 = proc.l4;
            let mut fa_guard = FRAME_ALLOC.lock();
            let Some(fa) = fa_guard.as_mut() else {
                return ERR;
            };
            // Unmap before returning the frame: it is about to be handed to
            // someone else, and a surviving mapping would outlive the grant.
            for entry in proc.maps.iter_mut() {
                if let Some((va, s)) = *entry {
                    if s == slot as usize {
                        memory::unmap_user_page(l4, va);
                        *entry = None;
                    }
                }
            }
            let _ = fa.dealloc(addr);
        }
        capability::ReleaseAction::ReleaseRing { id } => {
            crate::rings::release(id);
        }
        capability::ReleaseAction::DropEndpoint => {
            // A permanent removal, so the free-at-zero check runs (unlike a
            // transfer, which passes false because a matching mint follows).
            // This is the second permanent-removal site after teardown.
            ipc::note_cap_removed(&cap, true);
        }
        // The authority is leaving, so the access goes with it. Nothing is
        // freed: the pages are firmware MMIO, never allocated from anywhere
        // (D1 -- this is what D5 deferred).
        capability::ReleaseAction::UnmapFramebuffer => {
            process::unmap_fb_for_slot(proc, slot as usize);
        }
        // A *voluntary* release of a BORROWED framebuffer sends it home to its
        // lender's reserved slot -- the same reclamation a death runs
        // (`scheduler::reclaim_cap_home`) -- rather than unmapping and dropping
        // it. Ruled 2026-08-15 (cap_release-on-reserved). The old behaviour treated
        // this arm as plain `UnmapFramebuffer`, which stranded the lender's
        // reserved slot and bricked the borrowed screen precisely when a borrower
        // was well-behaved: a crash returned the screen (via death reclamation), a
        // polite release did not. That asymmetry was backwards, and the stranded
        // reservation is the same "green but leaking" class the slice-2 fix closed
        // on the IPC path -- here reached through the `release` verb.
        //
        // The borrower's own mapping still comes down first: the authority is
        // leaving it. Then the capability goes home with its origin cleared.
        capability::ReleaseAction::ReclaimTo { origin } => {
            process::unmap_fb_for_slot(proc, slot as usize);
            // `home` is `cap` with its origin cleared -- the lender owns it
            // outright again. `reclaim_target` builds it through the single
            // homecoming rule (`lent_to`); `current_slot()` is the releaser's own
            // table slot, which that rule takes but does not read (the origin
            // already names the destination). It always returns `Some` here --
            // `release_action` just said `ReclaimTo` -- so the fallback is inert.
            let home = match capability::reclaim_target(&cap, scheduler::current_slot()) {
                Some((_, home)) => home,
                None => cap,
            };
            // Safe while holding the current-process guard: the lender is a
            // different process (a cap's origin names who lent it, never its
            // holder), so `with_lender_caps` reaches TABLE or another core's
            // CURRENT, never this core's. LenderFull / NoLender leave the
            // now-revoked, now-unmapped capability to cease to exist -- the same
            // D4 fallback a death takes, identical to the old drop for those cases.
            let _ = scheduler::reclaim_cap_home(&cap, origin, home);
        }
        // Nothing pooled.
        capability::ReleaseAction::DropSlot => {}
        capability::ReleaseAction::Refuse => unreachable!("refused above"),
    }
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
            // console::writer() takes a fresh, lock-free handle (same as the
            // panic handler), so this holds nothing across kernel_resume.
            let mut serial = console::writer();
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
            // Reserve the slot the capability is leaving, so it has somewhere
            // guaranteed to come home to (D2(D)).
            Some(p) => process::revoke_and_unmap_for_lend(p, transfer_slot as usize),
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
        origin: None,
    };
    // The caller is lending this capability to the child, so record the caller
    // as its origin (D3). Kept separate from
    // `transferred`, which stays the pristine original for the rollback below:
    // if the spawn fails the move did not happen, and the caller must get back
    // exactly the capability it had.
    //
    // Set here rather than inside `scheduler::spawn` because only this call site
    // knows *which* grant is a lend -- `send_cap` beside it is a kernel mint and
    // must keep `origin: None`. The homecoming rule cannot apply to a spawn: the
    // child's slot is one that is free right now, and `clear_origins_naming`
    // clears every origin naming a slot as its process exits, so no live
    // capability names a free slot. That sweep has to reach running processes on
    // other cores for this to hold -- see `scheduler::clear_origins_naming`,
    // where walking `TABLE` alone was a real bug.
    // K-025: preserve an existing origin instead of overwriting it. D8 ruled
    // that `origin` names the ROOT lender, and `lent_to`'s middle branch exists
    // so a claim survives a hop -- but this site never went through `lent_to`
    // and unconditionally recorded the caller, laundering the real owner's claim
    // whenever a borrower passed a capability on to a child. The homecoming
    // branch genuinely cannot apply here (the child's slot is fresh), which is
    // what the comment above reasoned about and why the gap was not obvious.
    let lent = transferred.map(|cap| Capability {
        origin: cap
            .origin
            .or_else(|| Some(Origin::new(scheduler::current_slot(), transfer_slot as usize))),
        ..cap
    });
    if scheduler::spawn(binary, phys, &[Some(send_cap), lent]).is_none() {
        // Could not create the child: it never minted send_cap, so the result
        // endpoint is unreferenced -- reclaim the slot. Then undo the capability
        // move by restoring it to the caller verbatim (re-accounting it).
        ipc::release_endpoint(ep);
        if let Some(cap) = transferred {
            let mut cur = process::current().lock();
            if let Some(p) = cur.as_mut() {
                // The loan never happened, so its reservation must not outlive
                // it. Restore into the capability's OWN slot: `install` would
                // skip that slot precisely because it is reserved, and the
                // caller would silently find its capability moved by a failure
                // that is supposed to be invisible.
                let slot = transfer_slot as usize;
                if p.caps.reclaim_to(slot, cap).is_none() {
                    p.caps.clear_reservation(slot);
                    let _ = p.caps.install(cap);
                }
            }
            ipc::note_cap_added(&cap);
        }
        return ERR;
    }

    // The caller's RECV handle on the result channel; recv on it = wait.
    let recv_cap =
        Capability { object: CapObject::Endpoint { id: ep }, rights: RIGHT_RECV, origin: None };
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

