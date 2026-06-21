//! Two library OSes.
//!
//! This crate is the demonstration the rest of the repository exists to
//! set up. The kernel hands out raw physical frames through capabilities
//! and refuses to say what memory management *is* -- so here are two
//! complete, mutually incompatible answers, both implemented entirely in
//! unprivileged code over the same two syscalls (frame_alloc +
//! frame_map):
//!
//!   BumpAlloc      never reuses anything; free is a no-op. Fast,
//!                  trivially correct, wasteful. The kernel's capability
//!                  teardown reclaims its frames at exit anyway.
//!   FreeListAlloc  recycles freed blocks first-fit before asking the
//!                  kernel for more.
//!
//! The same application linked against each produces visibly different
//! address traces and kernel frame counts. That difference is policy,
//! and it lives here -- not in the kernel.

#![no_std]

/// A reference async block-I/O executor over the kernel's completion rings -- a
/// second piece of library-OS policy (alongside the allocators), built entirely
/// in unprivileged code over the ring ABI. See `ring`.
pub mod ring;

use libplinth::{sys_frame_alloc, sys_frame_map, MAP_BASE, PAGE_SIZE, SYS_ERR};

/// Minimum alignment and rounding granule for allocations.
const ALIGN: usize = 8;

pub trait MemPolicy {
    fn name(&self) -> &'static str;
    /// Allocate `size` bytes (8-aligned). Returns null on failure.
    fn alloc(&mut self, size: usize) -> *mut u8;
    /// Return a block. `size` must be the original request -- blocks are
    /// header-free, so the caller carries the size. An arena-style
    /// contract, chosen over headers to keep the policies legible.
    fn free(&mut self, ptr: *mut u8, size: usize);
    /// Frames this policy has requested from the kernel so far.
    fn kernel_frames(&self) -> usize;
}

/// Grow-only backing region shared by both policies: contiguous virtual
/// space from MAP_BASE, extended one kernel frame at a time. Each frame
/// is mapped at an address THIS CODE chooses -- the kernel only checks
/// the capability and the window.
struct Region {
    next: u64,
    mapped_end: u64,
    frames: usize,
}

impl Region {
    const fn new() -> Region {
        Region { next: MAP_BASE, mapped_end: MAP_BASE, frames: 0 }
    }

    /// Carve `size` fresh bytes, growing the mapping as needed.
    fn carve(&mut self, size: usize) -> *mut u8 {
        let start = self.next;
        let end = start + size as u64;
        while self.mapped_end < end {
            let slot = sys_frame_alloc();
            if slot == SYS_ERR {
                return core::ptr::null_mut();
            }
            if sys_frame_map(slot, self.mapped_end) != 0 {
                return core::ptr::null_mut();
            }
            self.mapped_end += PAGE_SIZE;
            self.frames += 1;
        }
        self.next = end;
        start as *mut u8
    }
}

fn round_up(size: usize) -> usize {
    size.div_ceil(ALIGN) * ALIGN
}

// ---------------------------------------------------------------------------
// Policy 1: bump
// ---------------------------------------------------------------------------

pub struct BumpAlloc {
    region: Region,
}

impl BumpAlloc {
    pub const fn new() -> BumpAlloc {
        BumpAlloc { region: Region::new() }
    }
}

impl Default for BumpAlloc {
    fn default() -> Self {
        Self::new()
    }
}

impl MemPolicy for BumpAlloc {
    fn name(&self) -> &'static str {
        "bump"
    }

    fn alloc(&mut self, size: usize) -> *mut u8 {
        self.region.carve(round_up(size))
    }

    /// Deliberately nothing. Freed memory is gone until the process
    /// exits, at which point the kernel's capability accounting returns
    /// every frame regardless of how lazy this policy was.
    fn free(&mut self, _ptr: *mut u8, _size: usize) {}

    fn kernel_frames(&self) -> usize {
        self.region.frames
    }
}

// ---------------------------------------------------------------------------
// Policy 2: free list
// ---------------------------------------------------------------------------

const MAX_FREE_SPANS: usize = 32;

pub struct FreeListAlloc {
    region: Region,
    /// Freed spans as (address, size). First-fit, split on larger-than-
    /// requested hits, no coalescing -- a real allocator would coalesce;
    /// this one stays readable instead.
    spans: [Option<(u64, usize)>; MAX_FREE_SPANS],
}

impl FreeListAlloc {
    pub const fn new() -> FreeListAlloc {
        FreeListAlloc { region: Region::new(), spans: [None; MAX_FREE_SPANS] }
    }
}

impl Default for FreeListAlloc {
    fn default() -> Self {
        Self::new()
    }
}

impl MemPolicy for FreeListAlloc {
    fn name(&self) -> &'static str {
        "freelist"
    }

    fn alloc(&mut self, size: usize) -> *mut u8 {
        let size = round_up(size);
        // First fit from the free list; only then ask the kernel.
        for entry in self.spans.iter_mut() {
            if let Some((addr, span_size)) = *entry {
                if span_size >= size {
                    let leftover = span_size - size;
                    *entry =
                        if leftover >= ALIGN { Some((addr + size as u64, leftover)) } else { None };
                    return addr as *mut u8;
                }
            }
        }
        self.region.carve(size)
    }

    fn free(&mut self, ptr: *mut u8, size: usize) {
        for entry in self.spans.iter_mut() {
            if entry.is_none() {
                *entry = Some((ptr as u64, round_up(size)));
                return;
            }
        }
        // Table full: the span is dropped (kernel reclaims at exit).
    }

    fn kernel_frames(&self) -> usize {
        self.region.frames
    }
}
