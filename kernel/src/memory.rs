//! Per-process address spaces.
//!
//! Each process runs in its own L4 page table. The probe at bring-up showed
//! the bootloader keeps everything the kernel needs -- code, stack, the
//! physical-memory map, boot structures -- in PML4 entries 1..512, and the
//! entire user region in PML4[0]. So a process address space is just a
//! private L4 whose entries 1..512 are copied from the bootloader's L4
//! (kernel mappings, shared) and whose PML4[0] (all user memory) is its own.
//! The kernel runs correctly under any process's CR3 because the shared half
//! is identical everywhere; only the user half differs.
//!
//! Creating a process clones the kernel half; destroying it frees the user
//! half's page-table frames and the L4 itself, so an address space leaks
//! nothing. User *data* frames are reclaimed separately, via capabilities.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::mapper::{MapToError, TranslateResult};
use x86_64::structures::paging::{
    Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB, Translate,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::frame_alloc::{FrameAlloc, FRAME_ALLOC, FRAME_SIZE};

/// All physical memory is reachable at `phys + PHYS_OFFSET`.
static PHYS_OFFSET: AtomicU64 = AtomicU64::new(0);
/// The bootloader's L4: the template each process L4 copies its kernel half
/// from, and the address space the kernel uses between processes.
static KERNEL_L4: AtomicU64 = AtomicU64::new(0);
/// CR3 flags captured at boot, reused on every switch.
static KERNEL_CR3_FLAGS: AtomicU64 = AtomicU64::new(0);

/// Present bit of a page-table entry.
const PRESENT: u64 = 1 << 0;
/// Physical-address field of a page-table entry.
const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

pub fn init(phys_offset: u64) {
    let (l4_frame, flags) = Cr3::read();
    PHYS_OFFSET.store(phys_offset, Ordering::Relaxed);
    KERNEL_L4.store(l4_frame.start_address().as_u64(), Ordering::Relaxed);
    KERNEL_CR3_FLAGS.store(flags.bits(), Ordering::Relaxed);
}

fn phys_offset() -> u64 {
    PHYS_OFFSET.load(Ordering::Relaxed)
}

/// The kernel/bootloader address space (active between processes).
pub fn kernel_l4() -> u64 {
    KERNEL_L4.load(Ordering::Relaxed)
}

/// An OffsetPageTable over the L4 at physical address `l4`.
///
/// # Safety
/// `l4` must name a live L4 frame, and the caller must not let two mappers
/// over the same table be used concurrently (single CPU makes this trivial).
unsafe fn mapper_for(l4: u64) -> OffsetPageTable<'static> {
    let table = &mut *((phys_offset() + l4) as *mut PageTable);
    OffsetPageTable::new(table, VirtAddr::new(phys_offset()))
}

/// Build a fresh address space: a private L4 that shares the kernel's half
/// (PML4 1..512) and starts with an empty user half (PML4[0]).
pub fn create_address_space() -> Result<u64, &'static str> {
    let l4 = {
        let mut fa_guard = FRAME_ALLOC.lock();
        let fa = fa_guard.as_mut().ok_or("frame allocator not initialised")?;
        fa.alloc().map_err(|_| "out of frames for address space")?
    };
    // SAFETY: l4 is freshly allocated; kernel_l4() is the live template.
    // The two raw views name different frames, so they never alias.
    unsafe {
        let new = &mut *((phys_offset() + l4) as *mut [u64; 512]);
        let kernel = &*((phys_offset() + kernel_l4()) as *const [u64; 512]);
        new[0] = 0; // private user half
        for i in 1..512 {
            new[i] = kernel[i]; // shared kernel half
        }
    }
    Ok(l4)
}

/// Free a process's user-half page tables (the PML4[0] subtree) and its L4.
/// User data frames are reclaimed elsewhere, through capabilities, so this
/// walks tables only -- never the leaf frames they point at.
pub fn destroy_address_space(l4: u64) {
    let mut fa_guard = FRAME_ALLOC.lock();
    let fa = fa_guard.as_mut().expect("frame allocator not initialised");
    // SAFETY: this is the process's own L4 during teardown; nothing else
    // references its user-half tables. Plinth maps only 4 KiB pages, so a
    // present L3/L2 entry always points at a child table, never a huge page.
    unsafe {
        let l4t = &*((phys_offset() + l4) as *const [u64; 512]);
        let e0 = l4t[0];
        if e0 & PRESENT != 0 {
            let l3 = e0 & ADDR_MASK;
            let l3t = &*((phys_offset() + l3) as *const [u64; 512]);
            for &e3 in l3t.iter() {
                if e3 & PRESENT == 0 {
                    continue;
                }
                let l2 = e3 & ADDR_MASK;
                let l2t = &*((phys_offset() + l2) as *const [u64; 512]);
                for &e2 in l2t.iter() {
                    if e2 & PRESENT != 0 {
                        let _ = fa.dealloc(e2 & ADDR_MASK); // L1 table
                    }
                }
                let _ = fa.dealloc(l2); // L2 table
            }
            let _ = fa.dealloc(l3); // L3 table
        }
        let _ = fa.dealloc(l4); // the L4 itself
    }
}

/// Make `l4` the active address space. The kernel half is shared, so kernel
/// code and data stay mapped across the switch.
pub fn switch_to(l4: u64) {
    let frame = PhysFrame::containing_address(PhysAddr::new(l4));
    let flags = Cr3Flags::from_bits_truncate(KERNEL_CR3_FLAGS.load(Ordering::Relaxed));
    // SAFETY: every L4 we hand out shares the kernel half captured at init.
    unsafe { Cr3::write(frame, flags) };
}

/// Return to the kernel/bootloader address space.
pub fn switch_to_kernel() {
    switch_to(kernel_l4());
}

/// Map one user-accessible page at `va` -> `phys` in address space `l4`.
/// Intermediate page tables are allocated from `frames`; they are reclaimed
/// by destroy_address_space, not here.
pub fn map_user_page(
    l4: u64,
    frames: &mut FrameAlloc,
    va: u64,
    phys: u64,
    flags: PageTableFlags,
) -> Result<(), &'static str> {
    let mut mapper = unsafe { mapper_for(l4) };
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
    let frame = PhysFrame::containing_address(PhysAddr::new(phys));
    // Parent entries need USER_ACCESSIBLE too; permissions AND together
    // down the walk.
    let parent =
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    // SAFETY: va is in the user window (caller-validated), phys is a frame
    // the caller owns, and the parent flags never weaken kernel mappings.
    unsafe { mapper.map_to_with_table_flags(page, frame, flags, parent, frames) }
        .map_err(|_| "page already mapped or table allocation failed")?
        .flush();
    Ok(())
}

/// Make `size` bytes of device MMIO at physical `phys` reachable from the
/// kernel, returning the virtual address. The address follows the phys-offset
/// convention (va = phys_offset + phys), so translating back is trivial.
///
/// On this platform the bootloader's `Mapping::Dynamic` physical-memory window
/// already spans the device BARs (the virtio-blk BAR sits at 0xc000000000) with
/// huge pages, so usually there is nothing to do: each page is checked, and one
/// already translating to the target frame is reused as-is. Any page NOT
/// already covered is mapped fresh -- non-cacheable and kernel-only (never
/// USER_ACCESSIBLE), so nothing in ring 3 can reach device registers.
///
/// Caveat: a page the bootloader already mapped keeps the bootloader's
/// (cacheable) attributes; this is harmless under QEMU, where BAR accesses are
/// treated as device MMIO regardless. A real-hardware port would force UC here.
///
/// Must be called at boot, BEFORE any process address space is created: a
/// freshly mapped page lands in the kernel half of `kernel_l4`, which each
/// process L4 copies at creation, so it becomes visible under every process CR3
/// without a later shootdown.
pub fn map_kernel_mmio(phys: u64, size: u64) -> Result<u64, &'static str> {
    let mut fa_guard = FRAME_ALLOC.lock();
    let fa = fa_guard.as_mut().ok_or("frame allocator not initialised")?;
    // SAFETY: kernel_l4() is the live kernel L4; we are the only mapper over it
    // here (single CPU, boot-time), and any page we add is a fresh MMIO page
    // that aliases nothing else.
    let mut mapper = unsafe { mapper_for(kernel_l4()) };

    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE;
    // Intermediate tables stay kernel-only (no USER_ACCESSIBLE).
    let parent = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;

    let first = phys & !(FRAME_SIZE - 1);
    let last = (phys + size + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
    let mut p = first;
    while p < last {
        let va = phys_offset() + p;
        // Already covered (e.g. by the bootloader's huge-page phys window)? Then
        // reuse the existing mapping rather than collide with the huge parent.
        if mapper.translate_addr(VirtAddr::new(va)) != Some(PhysAddr::new(p)) {
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
            let frame = PhysFrame::containing_address(PhysAddr::new(p));
            // SAFETY: [p, p+FRAME_SIZE) is device MMIO (a BAR the kernel owns),
            // va is the matching phys-offset address, and the parent flags never
            // weaken existing kernel mappings.
            unsafe { mapper.map_to_with_table_flags(page, frame, flags, parent, fa) }
                .map_err(|e| match e {
                    MapToError::FrameAllocationFailed => "mmio map: frame alloc failed",
                    MapToError::ParentEntryHugePage => "mmio map: parent huge page",
                    MapToError::PageAlreadyMapped(_) => "mmio map: page already mapped",
                })?
                .flush();
        }
        p += FRAME_SIZE;
    }
    Ok(phys_offset() + phys)
}

pub fn unmap_user_page(l4: u64, va: u64) {
    let mut mapper = unsafe { mapper_for(l4) };
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
    if let Ok((_frame, flush)) = mapper.unmap(page) {
        flush.flush();
    }
}

/// Is `va` mapped USER_ACCESSIBLE in `l4`? Used to validate user pointers
/// before the kernel dereferences them.
pub fn user_accessible(l4: u64, va: u64) -> bool {
    let mapper = unsafe { mapper_for(l4) };
    match mapper.translate(VirtAddr::new(va)) {
        TranslateResult::Mapped { flags, .. } => flags.contains(PageTableFlags::USER_ACCESSIBLE),
        _ => false,
    }
}

/// Is `va` mapped at all in `l4`?
pub fn is_mapped(l4: u64, va: u64) -> bool {
    let mapper = unsafe { mapper_for(l4) };
    !matches!(mapper.translate(VirtAddr::new(va)), TranslateResult::NotMapped)
}
