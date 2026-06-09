//! Page-table access for user mappings.
//!
//! The kernel's own mappings come from the bootloader and are never
//! touched. This module only adds and removes USER_ACCESSIBLE 4 KiB
//! mappings in the low half on behalf of user processes.

use spin::Mutex;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::mapper::TranslateResult;
use x86_64::structures::paging::{
    Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB, Translate,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::frame_alloc::FrameAlloc;

pub static MAPPER: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);

pub fn init(phys_offset: u64) {
    let (l4_frame, _) = Cr3::read();
    let l4_virt = phys_offset + l4_frame.start_address().as_u64();
    // SAFETY: the bootloader maps all physical memory at phys_offset and
    // hands us the active level-4 table; it stays active for the kernel
    // lifetime. MAPPER's Mutex serialises all access.
    let l4: &'static mut PageTable = unsafe { &mut *(l4_virt as *mut PageTable) };
    let mapper = unsafe { OffsetPageTable::new(l4, VirtAddr::new(phys_offset)) };
    *MAPPER.lock() = Some(mapper);
}

/// Map one user-accessible page at `va` -> `phys`. Intermediate page
/// tables are allocated from `frames` and never reclaimed -- a handful of
/// frames per distinct user region, allocated once per boot.
pub fn map_user_page(
    mapper: &mut OffsetPageTable<'static>,
    frames: &mut FrameAlloc,
    va: u64,
    phys: u64,
    flags: PageTableFlags,
) -> Result<(), &'static str> {
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

pub fn unmap_user_page(mapper: &mut OffsetPageTable<'static>, va: u64) {
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
    if let Ok((_frame, flush)) = mapper.unmap(page) {
        flush.flush();
    }
}

/// Is `va` mapped with USER_ACCESSIBLE? Used to validate user pointers
/// before the kernel dereferences them.
pub fn user_accessible(mapper: &OffsetPageTable<'static>, va: u64) -> bool {
    match mapper.translate(VirtAddr::new(va)) {
        TranslateResult::Mapped { flags, .. } => flags.contains(PageTableFlags::USER_ACCESSIBLE),
        _ => false,
    }
}

/// Is `va` mapped at all?
pub fn is_mapped(mapper: &OffsetPageTable<'static>, va: u64) -> bool {
    !matches!(mapper.translate(VirtAddr::new(va)), TranslateResult::NotMapped)
}
