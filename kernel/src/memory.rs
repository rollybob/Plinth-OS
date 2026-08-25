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
/// Writable bit of a page-table entry.
const WRITABLE: u64 = 1 << 1;
/// Physical-address field of a page-table entry.
const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

/// Bytes of virtual address space one PML4 slot covers (512 GiB).
const PML4_SLOT_SPAN: u64 = 1 << 39;

/// Base virtual address of the device-MMIO window: one whole PML4 slot claimed
/// at boot, used for nothing else. Zero until `init` has run.
static MMIO_BASE: AtomicU64 = AtomicU64::new(0);
/// Bump pointer within the MMIO window. Device mappings are never freed, so a
/// bump allocator is the whole allocator.
static MMIO_NEXT: AtomicU64 = AtomicU64::new(0);
/// Pages of device MMIO mapped so far -- reported at boot as the assertion that
/// every one of them went through this path (see `map_kernel_mmio`).
static MMIO_PAGES: AtomicU64 = AtomicU64::new(0);

pub fn init(phys_offset: u64) {
    let (l4_frame, flags) = Cr3::read();
    PHYS_OFFSET.store(phys_offset, Ordering::Relaxed);
    KERNEL_L4.store(l4_frame.start_address().as_u64(), Ordering::Relaxed);
    KERNEL_CR3_FLAGS.store(flags.bits(), Ordering::Relaxed);

    // Claim the MMIO window NOW, before any device is mapped and before any
    // process address space exists. Both halves of that ordering matter:
    //
    //  - `create_address_space` copies PML4 entries 1..512 at CREATION time, and
    //    the virtio completion IRQ touches a BAR while a *process* is scheduled,
    //    under that process's CR3. A PML4 entry minted after the first address
    //    space was cloned would be missing from it, and the device access would
    //    fault in an interrupt handler. Today's boot order happens to avoid that;
    //    reserving here makes it structural instead of incidental.
    //  - The frame allocator is already live at this point (main.rs installs it
    //    immediately before calling us), so the L3 table can be allocated.
    let base = reserve_mmio_window(phys_offset, l4_frame.start_address().as_u64());
    MMIO_BASE.store(base, Ordering::Relaxed);
    MMIO_NEXT.store(base, Ordering::Relaxed);
}

/// Find a free PML4 slot, give it an empty L3 table, and return its base VA.
///
/// Scanned rather than hardcoded because the bootloader places the kernel, the
/// stacks and the physical-memory window with `Mapping::Dynamic`: how many slots
/// the phys window occupies depends on the machine's top physical address, so a
/// constant that is free on one box can collide on another with more RAM or
/// higher MMIO. Slot 0 is skipped -- that is the user half.
///
/// Returns 0 if no slot is free or no frame can be had; `map_kernel_mmio` then
/// refuses rather than silently falling back to a cacheable mapping.
fn reserve_mmio_window(phys_offset: u64, l4_phys: u64) -> u64 {
    // SAFETY: the live kernel L4 at boot, single CPU, nothing else mapping.
    let l4 = unsafe { &mut *((phys_offset + l4_phys) as *mut [u64; 512]) };
    let mut idx = 1;
    while idx < 512 {
        if l4[idx] & PRESENT == 0 {
            break;
        }
        idx += 1;
    }
    if idx >= 512 {
        return 0;
    }
    let l3 = {
        let mut fa_guard = FRAME_ALLOC.lock();
        let Some(fa) = fa_guard.as_mut() else {
            return 0;
        };
        match fa.alloc() {
            Ok(f) => f,
            Err(_) => return 0,
        }
    };
    // SAFETY: freshly allocated frame, reachable through the phys window.
    unsafe {
        let t = &mut *((phys_offset + l3) as *mut [u64; 512]);
        for e in t.iter_mut() {
            *e = 0;
        }
    }
    // Kernel-only on purpose: no USER_ACCESSIBLE anywhere on this path, so a
    // device register is not merely unmapped from ring 3, it is unreachable.
    l4[idx] = l3 | PRESENT | WRITABLE;
    let i = idx as u64;
    if i < 256 {
        i << 39
    } else {
        0xFFFF_0000_0000_0000 | (i << 39)
    }
}

/// Pages of device MMIO mapped, and the window they were mapped into. Reported
/// at boot so that "every device mapping is uncached" is an asserted line rather
/// than a property nobody checks.
pub fn mmio_stats() -> (u64, u64) {
    (
        MMIO_PAGES.load(Ordering::Relaxed),
        MMIO_BASE.load(Ordering::Relaxed),
    )
}

pub(crate) fn phys_offset() -> u64 {
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
/// kernel, returning the virtual address to use. Every page is mapped
/// **uncached** (`NO_CACHE`) and kernel-only (never `USER_ACCESSIBLE`).
///
/// **Device MMIO does NOT follow the phys-offset convention.** It is mapped into
/// a private window -- one PML4 slot claimed at boot -- and the returned address
/// bears no fixed relation to `phys`, so callers must keep what they are given.
/// That is deliberate, and it is the fix for a real defect (`real_hardware.md`
/// D2, ruled 2026-08-18).
///
/// The old version mapped at `phys_offset + phys` and skipped any page already
/// translating there. Since the bootloader's `Mapping::Dynamic` window spans the
/// BARs with huge pages, that branch was taken **every time**: measured on
/// 2026-08-18, all six device mappings (LAPIC, I/O APIC, both MSI-X tables, both
/// virtio BARs) silently inherited the bootloader's **cacheable** attributes and
/// `NO_CACHE` was dead code. QEMU treats BAR accesses as device MMIO regardless,
/// which is exactly why it never showed. On real hardware cached MMIO means
/// writes that coalesce or land late, reads that return stale data, and MSI-X
/// table writes that may never reach the device.
///
/// Mapping into virgin address space rather than repairing an inherited mapping
/// is what makes the guarantee structural: there is no "already mapped?" case to
/// get wrong, because nothing else ever maps into this window. A private range
/// was chosen over splitting the bootloader's huge page for that reason -- see
/// the D2 ruling for why PAT/MTRR was rejected outright (an attribute keyed to a
/// physical range is a shared global that outlives every mapping of it, and
/// cannot travel with a capability the way a PTE does).
///
/// Panicking is not this function's job, but a caller that ignores the `Err` and
/// pokes the returned address anyway is the one bug this cannot prevent.
pub fn map_kernel_mmio(phys: u64, size: u64) -> Result<u64, &'static str> {
    let base = MMIO_BASE.load(Ordering::Relaxed);
    if base == 0 {
        return Err("mmio map: no window reserved");
    }
    let mut fa_guard = FRAME_ALLOC.lock();
    let fa = fa_guard.as_mut().ok_or("frame allocator not initialised")?;
    // SAFETY: kernel_l4() is the live kernel L4; we are the only mapper over it
    // here (single CPU, boot-time), and every page we add is in the MMIO window,
    // which nothing else maps into.
    let mut mapper = unsafe { mapper_for(kernel_l4()) };

    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE;
    // Intermediate tables stay kernel-only (no USER_ACCESSIBLE).
    let parent = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;

    let first = phys & !(FRAME_SIZE - 1);
    let last = (phys + size + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
    let span = last - first;
    let va_base = MMIO_NEXT.load(Ordering::Relaxed);
    if va_base + span > base + PML4_SLOT_SPAN {
        return Err("mmio map: window exhausted");
    }

    let mut off = 0;
    while off < span {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va_base + off));
        let frame = PhysFrame::containing_address(PhysAddr::new(first + off));
        // SAFETY: [first+off, +FRAME_SIZE) is device MMIO (a BAR or an interrupt
        // controller the kernel owns), and va_base+off is virgin address space
        // inside the reserved window, so this can alias nothing.
        unsafe { mapper.map_to_with_table_flags(page, frame, flags, parent, fa) }
            .map_err(|e| match e {
                MapToError::FrameAllocationFailed => "mmio map: frame alloc failed",
                MapToError::ParentEntryHugePage => "mmio map: parent huge page",
                // Unreachable by construction: the window is bump-allocated and
                // never reused. If this ever fires, the window is being shared.
                MapToError::PageAlreadyMapped(_) => "mmio map: page already mapped",
            })?
            .flush();
        // Counted here, one page at a time, and deliberately NOT as `span /
        // FRAME_SIZE` up front: a per-call total would report what the function
        // intended to map rather than what it mapped, so the boot assertion
        // would read the same whether the mapping happened or was skipped. That
        // is a control that cannot fail, which is worse than no control.
        MMIO_PAGES.fetch_add(1, Ordering::Relaxed);
        off += FRAME_SIZE;
    }
    MMIO_NEXT.store(va_base + span, Ordering::Relaxed);
    // Preserve the offset within the page: an MSI-X table need not start on a
    // page boundary.
    Ok(va_base + (phys - first))
}

/// Map one page identity (virtual address == physical address) into the
/// kernel's own address space: present, writable, executable, kernel-only.
///
/// Needed only by the AP trampoline (broader hardware, Stage B1): the moment
/// the trampoline enables paging, the instruction fetch for whatever runs
/// next is already translated through the new page tables -- and that
/// "next" is still the trampoline's own code, sitting at its low physical
/// address (the bootloader's `phys_offset + phys` scheme everywhere else
/// does NOT cover raw low physical addresses at their own value). This is a
/// transient mapping: `unmap_identity` removes it once every AP has moved
/// past it into normal phys-offset-mapped kernel code, per
/// section 10.
pub fn map_identity(phys: u64, size: u64) -> Result<(), &'static str> {
    let mut fa_guard = FRAME_ALLOC.lock();
    let fa = fa_guard.as_mut().ok_or("frame allocator not initialised")?;
    // SAFETY: kernel_l4() is the live kernel L4; boot-time, single CPU, and
    // the identity range is the trampoline's own reserved page (never
    // otherwise mapped).
    let mut mapper = unsafe { mapper_for(kernel_l4()) };

    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    let parent = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;

    let first = phys & !(FRAME_SIZE - 1);
    let last = (phys + size + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
    let mut p = first;
    while p < last {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(p));
        let frame = PhysFrame::containing_address(PhysAddr::new(p));
        // SAFETY: identity-mapping the trampoline's own reserved, otherwise
        // untouched low page; the parent flags never weaken existing kernel
        // mappings (this range was never mapped before).
        unsafe { mapper.map_to_with_table_flags(page, frame, flags, parent, fa) }
            .map_err(|e| match e {
                MapToError::FrameAllocationFailed => "identity map: frame alloc failed",
                MapToError::ParentEntryHugePage => "identity map: parent huge page",
                MapToError::PageAlreadyMapped(_) => "identity map: page already mapped",
            })?
            .flush();
        p += FRAME_SIZE;
    }
    Ok(())
}

/// Remove a mapping `map_identity` installed. Call once every AP that needs
/// it has moved past the trampoline into ordinary kernel code.
pub fn unmap_identity(phys: u64, size: u64) {
    let mut mapper = unsafe { mapper_for(kernel_l4()) };
    let first = phys & !(FRAME_SIZE - 1);
    let last = (phys + size + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
    let mut p = first;
    while p < last {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(p));
        if let Ok((_frame, flush)) = mapper.unmap(page) {
            flush.flush();
        }
        p += FRAME_SIZE;
    }
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

/// Translate a kernel virtual address to its physical address, if mapped.
/// Used at boot to learn the framebuffer's physical base from the virtual
/// address the bootloader mapped it at (framebuffer.rs), so
/// the region can be re-mapped into a user address space.
pub fn kernel_phys_of(va: u64) -> Option<u64> {
    let mapper = unsafe { mapper_for(kernel_l4()) };
    mapper.translate_addr(VirtAddr::new(va)).map(|pa| pa.as_u64())
}

/// Is `va` mapped at all in `l4`?
pub fn is_mapped(l4: u64, va: u64) -> bool {
    let mapper = unsafe { mapper_for(l4) };
    !matches!(mapper.translate(VirtAddr::new(va)), TranslateResult::NotMapped)
}
