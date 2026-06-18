//! The syscall-backed half of libfs: read a named program out of the boot
//! archive and launch it. This is the FS libOS's reason to exist -- it turns a
//! `BlockRange` capability over the archive disk plus a program name into a
//! running process, with the kernel never learning the archive format.
//!
//! Flow: read the superblock (sector 0), then the directory, parse both with
//! the pure `archive` parser, look the name up, read the program's ELF blob
//! into a contiguous run of mapped frames, and hand that buffer to
//! `spawn_from_buffer`. The kernel re-validates the buffer and runs the ELF
//! through its audited loader -- the on-disk bytes are untrusted input to it.
//!
//! Only compiled on the bare target; the host `cargo test` build of libfs
//! compiles just the pure `archive` parser (see Cargo.toml's target-gated dep).

use libplinth::{
    sys_block_read, sys_frame_alloc, sys_frame_map, sys_spawn_from_buffer, BLK_OK, MAP_BASE,
    PAGE_SIZE, SYS_ERR,
};

use crate::archive::{self, SECTOR};

/// Sectors per 4 KiB frame -- the most `block_read` transfers in one call.
const SECTORS_PER_FRAME: u64 = PAGE_SIZE / SECTOR as u64;

/// Page budget for a program image read from disk. Matches the kernel's
/// `MAX_SPAWN_ELF` (256 KiB) so a buffer this loader builds is never rejected
/// for size by `spawn_from_buffer`.
const MAX_PROG_PAGES: u64 = 64;

/// Fixed addresses in the caller's map window: one scratch page for the
/// superblock and directory, then the program image laid out contiguously from
/// the next page (so `spawn_from_buffer` gets one page-aligned run).
const SCRATCH_VA: u64 = MAP_BASE;
const PROG_VA: u64 = MAP_BASE + PAGE_SIZE;

/// Allocate one frame, map it at `va`, and return its capability slot. The
/// frame carries RIGHT_WRITE (frame_alloc grants it), which `block_read` needs
/// to DMA into it.
fn alloc_mapped(va: u64) -> Result<u64, &'static str> {
    let slot = sys_frame_alloc();
    if slot == SYS_ERR {
        return Err("archive: frame_alloc failed");
    }
    if sys_frame_map(slot, va) != 0 {
        return Err("archive: frame_map failed");
    }
    Ok(slot)
}

/// Read a named program out of the archive reachable through the `BlockRange`
/// capability at `range_slot`, and spawn it. `transfer_slot` optionally moves a
/// capability into the child (or `NO_CAP`). Returns a wait handle to recv the
/// child's result on (as `sys_spawn`), or a diagnostic on any failure.
///
/// The `BlockRange` must start at archive sector 0, so a range-relative sector
/// equals an archive sector (what the directory records).
pub fn spawn_from_archive(
    range_slot: u64,
    name: &[u8],
    transfer_slot: u64,
) -> Result<u64, &'static str> {
    // Superblock = sector 0, into a scratch frame we can read back.
    let scratch = alloc_mapped(SCRATCH_VA)?;
    if sys_block_read(range_slot, scratch, 0, 1) != BLK_OK {
        return Err("archive: superblock read failed");
    }
    // SAFETY: SCRATCH_VA names a frame we just allocated and mapped, into which
    // block_read DMA'd one sector.
    let sec0 = unsafe { core::slice::from_raw_parts(SCRATCH_VA as *const u8, SECTOR) };
    let sb = archive::parse_superblock(sec0).map_err(archive::FsError::as_str)?;

    // Directory = sb.dir_sectors sectors starting at sector 1. This bootstrap
    // loader keeps the directory to a single frame (ample for a boot archive);
    // a larger one would need a multi-frame read like the blob below.
    if sb.dir_sectors as u64 > SECTORS_PER_FRAME {
        return Err("archive: directory too large for the boot loader");
    }
    if sys_block_read(range_slot, scratch, 1, sb.dir_sectors as u64) != BLK_OK {
        return Err("archive: directory read failed");
    }
    let dir_bytes = sb.dir_bytes_len().map_err(archive::FsError::as_str)?;
    // SAFETY: as above; dir_sectors*512 >= dir_bytes were read into the frame.
    let dir = unsafe { core::slice::from_raw_parts(SCRATCH_VA as *const u8, dir_bytes) };
    let entry = archive::find(dir, &sb, name).map_err(archive::FsError::as_str)?;

    // Read the program blob into a contiguous run of frames at PROG_VA, a frame
    // (up to SECTORS_PER_FRAME sectors) at a time.
    let pages = (entry.byte_len as u64).div_ceil(PAGE_SIZE);
    if pages == 0 || pages > MAX_PROG_PAGES {
        return Err("archive: program size out of range");
    }
    let mut remaining = entry.sector_count() as u64;
    for i in 0..pages {
        let va = PROG_VA + i * PAGE_SIZE;
        let frame = alloc_mapped(va)?;
        let n = remaining.min(SECTORS_PER_FRAME);
        let sector = entry.first_sector as u64 + i * SECTORS_PER_FRAME;
        if sys_block_read(range_slot, frame, sector, n) != BLK_OK {
            return Err("archive: program read failed");
        }
        remaining -= n;
    }

    // Hand the contiguous image to the kernel. It re-validates the range and
    // runs the bytes through its audited ELF loader.
    // SAFETY: [PROG_VA, PROG_VA + byte_len) is the contiguous run of frames just
    // mapped above.
    let image =
        unsafe { core::slice::from_raw_parts(PROG_VA as *const u8, entry.byte_len as usize) };
    let handle = sys_spawn_from_buffer(image, transfer_slot);
    if handle == SYS_ERR {
        return Err("archive: spawn_from_buffer failed");
    }
    Ok(handle)
}
