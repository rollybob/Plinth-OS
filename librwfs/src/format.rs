//! On-disk format, mount, and the create/read/delete operations
//! (Design/readwrite_fs.md S2/S4/S5, Stage 2-3).
//!
//! Layout within the granted `BlockRange` (sectors, relative to the range's
//! own start):
//!
//! ```text
//! sector 0                  Superblock (magic + data_sectors)
//! sector [1, 1+BITMAP_SECTORS)        Bitmap (one bit per data sector)
//! sector [.., .. + DIR_SECTORS)        Directory (MAX_ENTRIES records)
//! sector [DATA_START, total)          File data, tracked by the bitmap
//! ```
//!
//! The whole metadata region (superblock + bitmap + directory, `META_BYTES`)
//! fits in one frame and is read/written as a single unit: rewritten in full
//! after every `create`/`delete`. There is no separate "mount stale state"
//! path -- per S5, the libOS formats its range fresh every run, so `format`
//! both writes and immediately reads back to verify the round trip (Stage 2),
//! the same proof shape `blkwrite-user` uses for the raw ring primitive this
//! crate is built on. Async-native from the start (`libos::ring`), per S2's
//! recommendation against a polled shim.

use libos::ring;
use libplinth::{PAGE_SIZE, BLK_OK};

use crate::bitmap::{self, BitmapError};
use crate::directory::{self, DirError, ENTRY_SIZE};

const SECTOR: usize = 512;
const MAGIC: [u8; 8] = *b"PLNTHRW1";

/// Directory capacity (Design/readwrite_fs.md S3: "a fixed cap... chosen
/// generously for a demo"). 16 entries comfortably covers the create/read/
/// delete/reuse demo (S6) with room to spare.
pub const MAX_ENTRIES: usize = 16;
const DIR_BYTES: usize = MAX_ENTRIES * ENTRY_SIZE;
const DIR_SECTORS: usize = DIR_BYTES.div_ceil(SECTOR);

/// One bitmap sector (4096 bits) vastly exceeds any data-area size this
/// milestone grants; `total` (passed explicitly to `bitmap::alloc`/`free`,
/// not inferred from this byte length) is what actually bounds allocation to
/// the real data sector count.
const BITMAP_SECTORS: usize = 1;
const BITMAP_BYTES: usize = BITMAP_SECTORS * SECTOR;

/// Sector index, relative to the range's start, where file data begins.
pub const DATA_START: usize = 1 + BITMAP_SECTORS + DIR_SECTORS;

const SB_MAGIC: usize = 0;
const SB_DATA_SECTORS: usize = 8;

const BITMAP_OFF: usize = SECTOR;
const DIR_OFF: usize = BITMAP_OFF + BITMAP_BYTES;

/// Total bytes the metadata region occupies -- must fit in one frame.
pub const META_BYTES: usize = DATA_START * SECTOR;

/// One frame's worth of file bytes is this milestone's file-size ceiling
/// (Design/readwrite_fs.md S4: fixed size at creation, no growth) -- no file
/// in the S6 demo approaches it.
pub const MAX_FILE_BYTES: usize = PAGE_SIZE as usize;

const _: () = assert!(META_BYTES <= PAGE_SIZE as usize, "metadata region must fit one frame");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RwfsError {
    /// A ring read/write completed with a non-OK block status.
    Io,
    /// The superblock magic did not match after format's write+read-back.
    BadMagic,
    /// The granted range is too small to hold even the metadata region.
    Truncated,
    /// `create`'s data is empty (zero-length files are out of scope for this
    /// milestone) or exceeds `MAX_FILE_BYTES`.
    InvalidLen,
    Bitmap(BitmapError),
    Dir(DirError),
}

/// A formatted filesystem over one granted `BlockRange`, plus the DMA frame
/// used to stage every read/write. `range_slot`/`frame_slot` are kernel
/// capability-table slots (Plinth syscall numbers); `frame_va` is where that
/// frame is mapped in this process's address space.
pub struct Mount {
    range_slot: u64,
    frame_slot: u64,
    frame_va: u64,
    data_sectors: u64,
    /// In-memory mirror of sectors `[0, DATA_START)`: superblock + bitmap +
    /// directory. Mutated in place by `create`/`delete`, then rewritten to
    /// disk in full (`persist_meta`) -- no partial metadata writes.
    meta: [u8; META_BYTES],
}

fn bitmap_slice(meta: &mut [u8; META_BYTES]) -> &mut [u8] {
    &mut meta[BITMAP_OFF..BITMAP_OFF + BITMAP_BYTES]
}

fn dir_slice(meta: &mut [u8; META_BYTES]) -> &mut [u8] {
    &mut meta[DIR_OFF..DIR_OFF + DIR_BYTES]
}

impl Mount {
    /// Format `range_slot` (a `BlockRange` of `total_sectors` sectors,
    /// `RIGHT_READ | RIGHT_WRITE`) fresh: a zeroed (all-free) bitmap and
    /// directory plus a written superblock, then read the metadata region
    /// back and verify it landed -- the round-trip proof `block_write.md`'s
    /// demo already established for the raw primitive, applied one layer up.
    pub fn format(
        range_slot: u64,
        frame_slot: u64,
        frame_va: u64,
        total_sectors: u64,
    ) -> Result<Mount, RwfsError> {
        if total_sectors <= DATA_START as u64 {
            return Err(RwfsError::Truncated);
        }
        let data_sectors = total_sectors - DATA_START as u64;

        let mut meta = [0u8; META_BYTES];
        meta[SB_MAGIC..SB_MAGIC + MAGIC.len()].copy_from_slice(&MAGIC);
        meta[SB_DATA_SECTORS..SB_DATA_SECTORS + 4].copy_from_slice(&(data_sectors as u32).to_le_bytes());
        // Bitmap and directory regions are already zero: "all free" for
        // both, per S2/S3's all-zero-means-free conventions.

        let mount = Mount { range_slot, frame_slot, frame_va, data_sectors, meta };
        mount.persist_meta()?;

        let status = ring::block_on(ring::read(range_slot, frame_slot, 0, DATA_START as u64));
        if status != BLK_OK {
            return Err(RwfsError::Io);
        }
        let mut back = [0u8; META_BYTES];
        for i in 0..META_BYTES {
            // SAFETY: frame_va is this process's mapped frame; the read above
            // just DMA'd DATA_START sectors (META_BYTES worth) into it.
            back[i] = unsafe { (frame_va as *const u8).add(i).read_volatile() };
        }
        if back[SB_MAGIC..SB_MAGIC + MAGIC.len()] != MAGIC {
            return Err(RwfsError::BadMagic);
        }
        if back != mount.meta {
            return Err(RwfsError::Io);
        }

        Ok(mount)
    }

    /// Rewrite the whole metadata region (superblock + bitmap + directory)
    /// to disk in one ring write. Called after every mutation; `format` also
    /// uses it for the initial write.
    fn persist_meta(&self) -> Result<(), RwfsError> {
        for i in 0..META_BYTES {
            // SAFETY: frame_va is this process's mapped frame; META_BYTES <=
            // one frame (the const assertion above).
            unsafe { (self.frame_va as *mut u8).add(i).write_volatile(self.meta[i]) };
        }
        let status = ring::block_on(ring::write(self.range_slot, self.frame_slot, 0, DATA_START as u64));
        if status != BLK_OK {
            return Err(RwfsError::Io);
        }
        Ok(())
    }

    /// Create `name` with exactly `data`'s bytes: allocate a contiguous run
    /// in the bitmap, record it in the directory, write the bytes to disk,
    /// then persist the updated metadata. Any failure after the bitmap
    /// allocation rolls the allocation (and, if reached, the directory
    /// insert) back -- a failed `create` must not leak space.
    pub fn create(&mut self, name: &[u8], data: &[u8]) -> Result<(), RwfsError> {
        if data.is_empty() || data.len() > MAX_FILE_BYTES {
            return Err(RwfsError::InvalidLen);
        }
        let n = data.len().div_ceil(SECTOR);
        let rel = bitmap::alloc(bitmap_slice(&mut self.meta), self.data_sectors as usize, n)
            .map_err(RwfsError::Bitmap)?;
        let abs_sector = DATA_START as u32 + rel as u32;

        if let Err(e) = directory::create(dir_slice(&mut self.meta), name, abs_sector, data.len() as u32) {
            let _ = bitmap::free(bitmap_slice(&mut self.meta), self.data_sectors as usize, rel, n);
            return Err(RwfsError::Dir(e));
        }

        // Zero the frame first (a manual loop, not core::ptr::write_bytes --
        // that lowers to a memset call needing a GOT relocation this
        // minimal linker script doesn't provide) so a partial last sector
        // never DMAs out stray bytes left over from a previous use.
        // SAFETY: frame_va is this process's mapped frame; n sectors fit
        // within it (data.len() <= MAX_FILE_BYTES <= one frame).
        unsafe {
            for i in 0..(n * SECTOR) {
                (self.frame_va as *mut u8).add(i).write_volatile(0u8);
            }
            for (i, &b) in data.iter().enumerate() {
                (self.frame_va as *mut u8).add(i).write_volatile(b);
            }
        }
        let status = ring::block_on(ring::write(self.range_slot, self.frame_slot, abs_sector as u64, n as u64));
        if status != BLK_OK {
            // The data never reached the device -- undo the directory entry
            // and the bitmap reservation rather than leaving a dangling one.
            let _ = directory::remove(dir_slice(&mut self.meta), name);
            let _ = bitmap::free(bitmap_slice(&mut self.meta), self.data_sectors as usize, rel, n);
            return Err(RwfsError::Io);
        }

        self.persist_meta()
    }

    /// Look up `name`'s on-disk location and length without reading its
    /// bytes -- (first_sector, byte_len). Used to prove the bitmap actually
    /// reclaims freed space (S6): a file created after a delete that lands
    /// at the deleted file's exact former sector is the deterministic proof,
    /// not just "creation succeeded."
    pub fn stat(&self, name: &[u8]) -> Result<(u32, u32), RwfsError> {
        let entry = directory::find(&self.meta[DIR_OFF..DIR_OFF + DIR_BYTES], name).map_err(RwfsError::Dir)?;
        Ok((entry.first_sector, entry.byte_len))
    }

    /// Read `name`'s exact bytes into `out` (which must be at least its
    /// length). Returns the byte count read.
    pub fn read(&self, name: &[u8], out: &mut [u8]) -> Result<usize, RwfsError> {
        // dir_slice needs &mut for create/remove's shared helper signature;
        // find only reads, so borrow the same bytes immutably here instead.
        let entry = directory::find(&self.meta[DIR_OFF..DIR_OFF + DIR_BYTES], name).map_err(RwfsError::Dir)?;
        let len = entry.byte_len as usize;
        if out.len() < len {
            return Err(RwfsError::InvalidLen);
        }
        let n = entry.sector_count() as u64;
        let status = ring::block_on(ring::read(self.range_slot, self.frame_slot, entry.first_sector as u64, n));
        if status != BLK_OK {
            return Err(RwfsError::Io);
        }
        for i in 0..len {
            // SAFETY: frame_va just received n sectors via DMA; len <= n*SECTOR.
            out[i] = unsafe { (self.frame_va as *const u8).add(i).read_volatile() };
        }
        Ok(len)
    }

    /// Delete `name`: free its bitmap run and remove its directory entry,
    /// then persist the updated metadata.
    pub fn delete(&mut self, name: &[u8]) -> Result<(), RwfsError> {
        let entry = directory::remove(dir_slice(&mut self.meta), name).map_err(RwfsError::Dir)?;
        let rel = entry.first_sector - DATA_START as u32;
        bitmap::free(bitmap_slice(&mut self.meta), self.data_sectors as usize, rel as usize, entry.sector_count() as usize)
            .map_err(RwfsError::Bitmap)?;
        self.persist_meta()
    }
}

