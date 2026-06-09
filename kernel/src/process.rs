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

use spin::Mutex;
use x86_64::structures::paging::PageTableFlags;

use crate::capability::{CapObject, CapTable};
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

pub const MAX_USER_MAPS: usize = 16;

/// Code + stack pages the kernel sets up at spawn (bounded so the
/// bookkeeping can live in a fixed array).
const MAX_BOOT_FRAMES: usize = 64;

pub struct Process {
    pub caps: CapTable,
    /// Live frame_map results as (virtual address, capability slot), so
    /// frame_free and teardown can unmap them.
    pub maps: [Option<(u64, usize)>; MAX_USER_MAPS],
}

impl Process {
    pub const fn new() -> Process {
        Process { caps: CapTable::new(), maps: [None; MAX_USER_MAPS] }
    }
}

pub static CURRENT: Mutex<Option<Process>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Exited(u64),
    Faulted,
}

/// Load `binary` (a flat image linked at USER_CODE_VA), run it in ring 3
/// to completion, and tear it down. Returns how it ended.
pub fn run(binary: &[u8], phys_offset: u64) -> Result<Outcome, &'static str> {
    let code_pages = (binary.len() as u64).div_ceil(FRAME_SIZE);
    if code_pages == 0 {
        return Err("empty user binary");
    }
    if code_pages + USER_STACK_PAGES > MAX_BOOT_FRAMES as u64 {
        return Err("user binary too large");
    }

    // (va, phys) for every page the kernel maps on the process's behalf.
    let mut boot_frames: [Option<(u64, u64)>; MAX_BOOT_FRAMES] = [None; MAX_BOOT_FRAMES];

    let setup = {
        let mut fa_guard = FRAME_ALLOC.lock();
        let fa = fa_guard.as_mut().ok_or("frame allocator not initialised")?;
        let mut mapper_guard = memory::MAPPER.lock();
        let mapper = mapper_guard.as_mut().ok_or("mapper not initialised")?;

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
                memory::map_user_page(mapper, fa, va, phys, flags)?;
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
                memory::map_user_page(mapper, fa, va, phys, flags)?;
                boot_frames[next] = Some((va, phys));
                next += 1;
            }
            Ok(())
        };
        let result = setup();
        if result.is_err() {
            // Partial spawn: roll back whatever was mapped before failing.
            for entry in boot_frames.iter().flatten() {
                memory::unmap_user_page(mapper, entry.0);
                let _ = fa.dealloc(entry.1);
            }
        }
        result
    };
    setup?;

    *CURRENT.lock() = Some(Process::new());

    // All locks are released here: syscall handlers re-acquire them.
    let raw = usermode::enter_user(USER_CODE_VA, USER_STACK_TOP);

    let proc = CURRENT.lock().take().expect("CURRENT vanished during user execution");
    teardown(proc, &boot_frames);

    if raw == usermode::EXIT_FAULTED {
        Ok(Outcome::Faulted)
    } else {
        Ok(Outcome::Exited(raw))
    }
}

/// Return everything the process held: frame_map mappings, capability-
/// owned frames, then the kernel-made code and stack pages. Intermediate
/// page-table frames are intentionally not reclaimed (memory.rs).
fn teardown(mut proc: Process, boot_frames: &[Option<(u64, u64)>]) {
    let mut fa_guard = FRAME_ALLOC.lock();
    let fa = fa_guard.as_mut().expect("frame allocator not initialised");
    let mut mapper_guard = memory::MAPPER.lock();
    let mapper = mapper_guard.as_mut().expect("mapper not initialised");

    for (va, _slot) in proc.maps.iter().flatten() {
        memory::unmap_user_page(mapper, *va);
    }
    proc.caps.drain(|cap| {
        let CapObject::Frame { addr } = cap.object;
        let _ = fa.dealloc(addr);
    });
    for (va, phys) in boot_frames.iter().flatten() {
        memory::unmap_user_page(mapper, *va);
        let _ = fa.dealloc(*phys);
    }
}
