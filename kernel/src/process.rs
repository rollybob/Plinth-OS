//! The process abstraction, such as it is.
//!
//! A Plinth process is a capability table plus a record of what it has
//! mapped -- nothing else. No PID, no priority, no state machine: with
//! synchronous one-at-a-time execution (usermode.rs), the kernel-side
//! "process table" is a single Option.
//!
//! run() owns the full lifecycle: allocate and map code + stack, install
//! CURRENT, enter ring 3, and on return (exit syscall or fault) tear
//! everything down -- unmap the user's frame_map mappings, drain the
//! capability table back into the frame allocator, free code and stack.
//! A faulting process leaks nothing.

use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;
use x86_64::structures::paging::PageTableFlags;

use crate::capability::{Capability, CapObject, CapTable, RIGHT_CONSUME};
use crate::frame_alloc::{FRAME_ALLOC, FRAME_SIZE};
use crate::memory;
use crate::usermode;

/// Where user binaries are loaded. Must match the user crates' linker.ld.
pub const USER_CODE_VA: u64 = 0x40_0000;
/// Top of the user stack; pages are mapped below this address.
pub const USER_STACK_TOP: u64 = 0x80_0000;
const USER_STACK_PAGES: u64 = 4;

/// Window in which frame_map accepts user-chosen virtual addresses.
pub const USER_MAP_BASE: u64 = 0x1000_0000;
pub const USER_MAP_END: u64 = 0x2000_0000;

/// Sub-window reserved for demand-paged (lazy) memory. A not-present fault
/// here, when the process has registered a fault handler, is delivered to
/// that handler instead of terminating the process (see `fault`). It sits
/// inside the map window so the handler can satisfy faults with the
/// ordinary frame_map syscall.
pub const USER_LAZY_BASE: u64 = 0x1800_0000;
pub const USER_LAZY_END: u64 = 0x1900_0000;

pub const MAX_USER_MAPS: usize = 16;

/// Every process is minted a CPU-time capability at spawn, in this slot.
/// It is the first mint into a fresh table, so it always lands at index 0;
/// userspace relies on that the way Unix relies on fd 0. (libplinth mirrors
/// this constant as CPU_CAP_SLOT.)
const CPU_CAP_SLOT: usize = 0;

/// A capability transferred into a child by `spawn` lands here -- the first
/// mint after the CPU budget. Like the budget slot, userspace relies on it
/// (libplinth mirrors it as GRANT_SLOT).
const GRANT_SLOT: usize = 1;

/// Ticks granted to each process at spawn. The CPU-budget demo charges
/// against this and is cut off when it overdraws; the other demos never
/// call cpu_charge, so the budget simply goes unused.
const INITIAL_CPU_BUDGET: u64 = 1024;

/// Code + stack pages the kernel sets up at spawn (bounded so the
/// bookkeeping can live in a fixed array).
pub const MAX_BOOT_FRAMES: usize = 64;

/// Physical-memory offset (set once at boot), so `spawn` can load a child
/// without threading it through every call the way the top-level loop does.
static PHYS_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Binaries a process may launch by id via `spawn`. Installed once at boot
/// from the kernel's embedded set; empty until then.
static SPAWNABLE: Mutex<&'static [&'static [u8]]> = Mutex::new(&[]);

pub fn set_phys_offset(offset: u64) {
    PHYS_OFFSET.store(offset, Ordering::Relaxed);
}

pub fn phys_offset() -> u64 {
    PHYS_OFFSET.load(Ordering::Relaxed)
}

pub fn set_spawnable(table: &'static [&'static [u8]]) {
    *SPAWNABLE.lock() = table;
}

/// The spawnable binary with this id, if any.
pub fn spawnable(id: usize) -> Option<&'static [u8]> {
    SPAWNABLE.lock().get(id).copied()
}

/// A registered ring-3 page-fault handler: where to jump (entry) and the
/// stack it runs on. Pure data -- teardown ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultReg {
    pub entry: u64,
    pub stack_top: u64,
}

pub struct Process {
    pub caps: CapTable,
    /// Live frame_map results as (virtual address, capability slot), so
    /// frame_free and teardown can unmap them.
    pub maps: [Option<(u64, usize)>; MAX_USER_MAPS],
    /// The process's self-paging handler, if it registered one.
    pub fault: Option<FaultReg>,
    /// True while a fault is being serviced in the handler. A second fault
    /// in that window is unhandleable and terminates the process -- the
    /// kernel never recurses into a handler.
    pub in_fault: bool,
    /// Physical address of this process's private L4 (its address space).
    /// Zero on the placeholder Process; set once the address space exists.
    pub l4: u64,
}

impl Process {
    pub const fn new() -> Process {
        Process {
            caps: CapTable::new(),
            maps: [None; MAX_USER_MAPS],
            fault: None,
            in_fault: false,
            l4: 0,
        }
    }
}

pub static CURRENT: Mutex<Option<Process>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Exited(u64),
    Faulted,
    /// The process overdrew its CPU-time budget and the kernel terminated
    /// it (cpu_charge with nothing left). Reclaimed like any other exit.
    OutOfBudget,
}

/// Allocate, copy, and map a flat user image's code + stack pages into the
/// current address space, recording every (va, phys) pair in `boot_frames`
/// (which the caller must pass zeroed, sized to bound the image). Rolls its
/// own mappings back on failure. Shared by the top-level loop and `spawn`.
pub fn load_and_map(
    binary: &[u8],
    phys_offset: u64,
    l4: u64,
    boot_frames: &mut [Option<(u64, u64)>],
) -> Result<(), &'static str> {
    let code_pages = (binary.len() as u64).div_ceil(FRAME_SIZE);
    if code_pages == 0 {
        return Err("empty user binary");
    }
    if code_pages + USER_STACK_PAGES > boot_frames.len() as u64 {
        return Err("user binary too large");
    }

    let mut fa_guard = FRAME_ALLOC.lock();
    let fa = fa_guard.as_mut().ok_or("frame allocator not initialised")?;

    let mut next = 0usize;
    let mut setup = || -> Result<(), &'static str> {
        // Code pages: copied from the embedded binary, zero-padded.
        // Mapped writable because the flat image carries .data/.bss,
        // and executable because it carries .text -- page-level W^X
        // is not worth a segment-aware loader in a toy.
        for i in 0..code_pages {
            let phys = fa.alloc().map_err(|_| "out of frames for user code")?;
            // SAFETY: phys is a freshly allocated frame, reachable
            // through the bootloader's full physical mapping.
            unsafe {
                let dst = (phys_offset + phys) as *mut u8;
                core::ptr::write_bytes(dst, 0, FRAME_SIZE as usize);
                let off = (i * FRAME_SIZE) as usize;
                let end = usize::min(off + FRAME_SIZE as usize, binary.len());
                core::ptr::copy_nonoverlapping(binary.as_ptr().add(off), dst, end - off);
            }
            let va = USER_CODE_VA + i * FRAME_SIZE;
            let flags = PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::USER_ACCESSIBLE;
            memory::map_user_page(l4, fa, va, phys, flags)?;
            boot_frames[next] = Some((va, phys));
            next += 1;
        }

        // Stack pages: zeroed, non-executable, below USER_STACK_TOP.
        for i in 0..USER_STACK_PAGES {
            let phys = fa.alloc().map_err(|_| "out of frames for user stack")?;
            // SAFETY: as above.
            unsafe {
                core::ptr::write_bytes((phys_offset + phys) as *mut u8, 0, FRAME_SIZE as usize);
            }
            let va = USER_STACK_TOP - (i + 1) * FRAME_SIZE;
            let flags = PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::USER_ACCESSIBLE
                | PageTableFlags::NO_EXECUTE;
            memory::map_user_page(l4, fa, va, phys, flags)?;
            boot_frames[next] = Some((va, phys));
            next += 1;
        }
        Ok(())
    };
    let result = setup();
    if result.is_err() {
        // Partial spawn: roll back whatever was mapped before failing.
        for entry in boot_frames.iter().flatten() {
            memory::unmap_user_page(l4, entry.0);
            let _ = fa.dealloc(entry.1);
        }
    }
    result
}

/// Build a fresh process: mint its CPU-time budget (always CPU_CAP_SLOT),
/// and, if `transferred` is given, install that capability (always
/// GRANT_SLOT, the next mint). Used by both the top-level loop and `spawn`.
pub fn spawn_process(transferred: Option<Capability>) -> Process {
    let mut proc = Process::new();
    let slot = proc
        .caps
        .mint(CapObject::CpuTime { budget: INITIAL_CPU_BUDGET }, RIGHT_CONSUME)
        .expect("fresh capability table cannot be full");
    debug_assert_eq!(slot, CPU_CAP_SLOT, "CPU-time capability landed in an unexpected slot");
    if let Some(cap) = transferred {
        let granted = proc
            .caps
            .mint(cap.object, cap.rights)
            .expect("fresh table has room for a grant");
        debug_assert_eq!(granted, GRANT_SLOT, "granted capability landed in an unexpected slot");
    }
    proc
}

/// Load `binary` (a flat image linked at USER_CODE_VA), run it in ring 3
/// to completion, and tear it down. Returns how it ended.
pub fn run(binary: &[u8], phys_offset: u64) -> Result<Outcome, &'static str> {
    // A private address space for this process.
    let l4 = memory::create_address_space()?;

    // (va, phys) for every page the kernel maps on the process's behalf.
    let mut boot_frames: [Option<(u64, u64)>; MAX_BOOT_FRAMES] = [None; MAX_BOOT_FRAMES];
    if let Err(e) = load_and_map(binary, phys_offset, l4, &mut boot_frames) {
        memory::destroy_address_space(l4);
        return Err(e);
    }

    let mut proc = spawn_process(None);
    proc.l4 = l4;
    *CURRENT.lock() = Some(proc);

    // Run under the process's own address space; locks are all released here.
    memory::switch_to(l4);
    let raw = usermode::enter_user(USER_CODE_VA, USER_STACK_TOP);
    memory::switch_to_kernel();

    let proc = CURRENT.lock().take().expect("CURRENT vanished during user execution");
    teardown(proc, &boot_frames);
    memory::destroy_address_space(l4);

    let outcome = match raw {
        usermode::EXIT_FAULTED => Outcome::Faulted,
        usermode::EXIT_OUT_OF_BUDGET => Outcome::OutOfBudget,
        code => Outcome::Exited(code),
    };
    Ok(outcome)
}

/// Return everything the process held: frame_map mappings, capability-owned
/// frames, then the kernel-made code and stack pages. The address space's
/// own page-table frames are reclaimed by destroy_address_space afterward.
pub fn teardown(mut proc: Process, boot_frames: &[Option<(u64, u64)>]) {
    let l4 = proc.l4;
    let mut fa_guard = FRAME_ALLOC.lock();
    let fa = fa_guard.as_mut().expect("frame allocator not initialised");

    for (va, _slot) in proc.maps.iter().flatten() {
        memory::unmap_user_page(l4, *va);
    }
    proc.caps.drain(|cap| {
        // Only Frame capabilities own a poolable resource. A CpuTime
        // budget (spent or not) has nothing to return -- dropping the slot
        // is the whole of its teardown.
        if let CapObject::Frame { addr } = cap.object {
            let _ = fa.dealloc(addr);
        }
    });
    for (va, phys) in boot_frames.iter().flatten() {
        memory::unmap_user_page(l4, *va);
        let _ = fa.dealloc(*phys);
    }
}
