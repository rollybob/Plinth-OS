//! libfs -- the read-only boot-archive library OS.
//!
//! This is the filesystem as *policy*: the kernel multiplexes the disk into
//! `BlockRange` capabilities and refuses to say what an on-disk layout is, so
//! the layout -- and the lookup-by-name-then-load flow built on it -- lives
//! here, in unprivileged code, exactly the way the two memory policies in
//! `libos` live over raw frame capabilities.
//!
//! The format is a minimal read-only archive (the initramfs role): a
//! superblock, a fixed directory of `(name, first_sector, byte_len)`, then the
//! program blobs, sector-aligned. There is no allocation, no free list, and no
//! write path -- none of which "load a program from disk instead of embedding
//! it" needs. A read-write filesystem is a separate, later library OS over the
//! same kernel block-capability surface.
//!
//! `archive` is a pure parser over byte slices -- it issues no syscalls and
//! owns no memory -- which is what lets it be unit-tested on the host with
//! `cargo test`, the same way the kernel's `elf::parse` is exercised. The
//! syscall-backed load helper (block_read into frames, then spawn_from_buffer)
//! is built only on the bare target.

#![cfg_attr(not(test), no_std)]

pub mod archive;

// The load helper issues syscalls, so it exists only on the bare target. The
// host `cargo test` build (which exercises the pure `archive` parser) skips it,
// and with it the libplinth dependency.
#[cfg(target_os = "none")]
pub mod load;
