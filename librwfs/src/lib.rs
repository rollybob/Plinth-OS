//! librwfs -- the read-write filesystem library OS (Design/readwrite_fs.md).
//!
//! Builds entirely on the block ring ABI's read/write primitives
//! (block_write.md, ABI v2.6): the kernel ships sectors, and this crate
//! decides what they mean -- a bitmap free-space allocator (`bitmap`) and a
//! fixed-max-entry mutable directory (`directory`), both pure logic over
//! byte slices, no allocation and no device I/O, so they are host-testable
//! exactly like `libfs::archive`'s parser. The format/mount layer wiring
//! these to real sectors via `libos::ring` builds only on the bare target,
//! the same split `libfs` uses between `archive` and `load`.
//!
//! Kept as a separate crate from the read-only `libfs` archive rather than a
//! module inside it (Design/readwrite_fs.md S1): the two formats share no
//! code -- one is immutable and build-time-assembled, the other mutable and
//! runtime-allocated -- and the archive remains the permanently-useful
//! initramfs/boot-image format, unchanged.

#![cfg_attr(not(test), no_std)]

pub mod bitmap;
pub mod directory;

// The format/mount/create/read/delete layer issues ring syscalls, so it
// exists only on the bare target -- the same split libfs uses between its
// pure archive parser and its syscall-backed load helper.
#[cfg(target_os = "none")]
pub mod format;
