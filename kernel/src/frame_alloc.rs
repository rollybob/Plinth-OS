//! Physical frame allocator.
//!
//! Physical memory is the first resource Plinth multiplexes. The kernel
//! tracks frames with a bitmap and hands them out one at a time; it has
//! no opinion about what a frame is *for*. Heaps, stacks, buffers --
//! that's library OS territory.
//!
//! One bit per 4 KiB frame, set = unavailable. The bitmap covers
//! physical memory from 0 to the end of the highest usable region;
//! holes (MMIO, firmware) are permanently marked unavailable. The
//! bitmap cannot distinguish "allocated" from "reserved", so callers
//! must only dealloc addresses that alloc returned -- the capability
//! layer enforces that discipline for userspace.

use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use spin::Mutex;

pub const FRAME_SIZE: u64 = 4096;

/// Installed once by kernel_main; the syscall layer takes frames from
/// here on behalf of userspace.
pub static FRAME_ALLOC: Mutex<Option<FrameAlloc>> = Mutex::new(None);

pub struct FrameAlloc {
    /// One bit per frame, set = unavailable.
    bitmap: &'static mut [u64],
    /// Total frames tracked (index range of the bitmap).
    frames: usize,
    /// Frames currently available.
    free: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    Exhausted,
    /// Address unaligned or beyond tracked memory.
    OutOfRange,
    /// Frame was not currently allocated (double free).
    NotAllocated,
}

// alloc/dealloc are exercised only from test code until the syscall
// layer lands; suppress dead-code noise in the production build.
#[cfg_attr(not(feature = "tests"), allow(dead_code))]
impl FrameAlloc {
    /// Build the allocator from the bootloader memory map, stealing the
    /// bitmap's own storage from the largest usable region.
    pub fn new(regions: &MemoryRegions, phys_offset: u64) -> FrameAlloc {
        let usable = || {
            regions
                .iter()
                .filter(|r| r.kind == MemoryRegionKind::Usable)
        };

        let max_end = usable().map(|r| r.end).max().expect("no usable memory");
        let frames = (max_end / FRAME_SIZE) as usize;
        let words = frames.div_ceil(64);
        let bitmap_bytes = (words * 8) as u64;
        let bitmap_frames = bitmap_bytes.div_ceil(FRAME_SIZE);

        // Host the bitmap at the (aligned) start of the largest usable region.
        let host = usable().max_by_key(|r| r.end - r.start).expect("no usable memory");
        let bitmap_phys = (host.start + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
        assert!(
            host.end >= bitmap_phys + bitmap_frames * FRAME_SIZE,
            "largest usable region cannot hold the frame bitmap"
        );

        // SAFETY: [bitmap_phys, bitmap_phys + bitmap_bytes) lies inside a
        // bootloader-reported Usable region, which the bootloader mapped at
        // phys_offset for the kernel's lifetime. The frames are marked
        // unavailable below, so nothing else will ever be handed this range.
        let bitmap: &'static mut [u64] = unsafe {
            core::slice::from_raw_parts_mut((phys_offset + bitmap_phys) as usize as *mut u64, words)
        };

        // Start fully unavailable (covers holes and the partial tail word),
        // then open up each usable region, then re-reserve the bitmap itself.
        bitmap.fill(u64::MAX);
        let mut alloc = FrameAlloc { bitmap, frames, free: 0 };

        for r in usable() {
            let first = (r.start + FRAME_SIZE - 1) / FRAME_SIZE;
            let last = r.end / FRAME_SIZE;
            for idx in first..last {
                alloc.mark_free(idx as usize);
            }
        }
        let first = (bitmap_phys / FRAME_SIZE) as usize;
        for idx in first..first + bitmap_frames as usize {
            alloc.mark_used(idx);
        }

        alloc
    }

    /// Allocate one frame; returns its physical address.
    pub fn alloc(&mut self) -> Result<u64, FrameError> {
        for (wi, word) in self.bitmap.iter_mut().enumerate() {
            if *word != u64::MAX {
                let bit = (!*word).trailing_zeros() as usize;
                let idx = wi * 64 + bit;
                if idx >= self.frames {
                    break;
                }
                *word |= 1 << bit;
                self.free -= 1;
                return Ok(idx as u64 * FRAME_SIZE);
            }
        }
        Err(FrameError::Exhausted)
    }

    /// Return a frame previously handed out by alloc.
    pub fn dealloc(&mut self, addr: u64) -> Result<(), FrameError> {
        if addr % FRAME_SIZE != 0 {
            return Err(FrameError::OutOfRange);
        }
        let idx = (addr / FRAME_SIZE) as usize;
        if idx >= self.frames {
            return Err(FrameError::OutOfRange);
        }
        let (wi, bit) = (idx / 64, idx % 64);
        if self.bitmap[wi] & (1 << bit) == 0 {
            return Err(FrameError::NotAllocated);
        }
        self.bitmap[wi] &= !(1 << bit);
        self.free += 1;
        Ok(())
    }

    pub fn free_frames(&self) -> usize {
        self.free
    }

    fn mark_free(&mut self, idx: usize) {
        let (wi, bit) = (idx / 64, idx % 64);
        if self.bitmap[wi] & (1 << bit) != 0 {
            self.bitmap[wi] &= !(1 << bit);
            self.free += 1;
        }
    }

    fn mark_used(&mut self, idx: usize) {
        let (wi, bit) = (idx / 64, idx % 64);
        if self.bitmap[wi] & (1 << bit) == 0 {
            self.bitmap[wi] |= 1 << bit;
            self.free -= 1;
        }
    }
}
