//! A bitmap free-space allocator over a byte slice (Design/readwrite_fs.md
//! S2).
//!
//! One bit per sector in the filesystem's granted range, packed LSB-first
//! into bytes immediately after the superblock. Mirrors the kernel frame
//! allocator's "set = unavailable" convention (kernel/src/frame_alloc.rs), at
//! byte instead of word granularity, since this bitmap is read and written
//! as raw disk bytes rather than held as an aligned in-memory array.
//!
//! `alloc`/`free` take an explicit `total` bit count rather than inferring it
//! from the slice length: the on-disk bitmap is sized in whole sectors, but
//! the meaningful bit count (the granted range's actual data-sector count)
//! is usually smaller, leaving padding bits in the last byte. Without `total`
//! those padding bits would look free and `alloc` could hand out a "sector"
//! past the real data area. The caller passes the true bit count; this module
//! never reads or writes past it.
//!
//! Pure functions over a caller-owned `&mut [u8]` -- no allocation, no device
//! I/O -- so the allocator is host-testable exactly like `libfs::archive`'s
//! parser. Stage 2 wires this to a real on-disk bitmap sector via
//! `libos::ring`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitmapError {
    /// No run of the requested length of consecutive free bits exists.
    OutOfSpace,
    /// The requested run (zero-length, or `start + n`), or `total` itself,
    /// falls outside the bitmap's actual byte-backed bit count.
    OutOfRange,
}

/// Total bits a bitmap of `bytes` bytes can physically hold (not necessarily
/// all meaningful -- see the module note on `total`).
pub fn capacity(bytes: usize) -> usize {
    bytes * 8
}

fn bit(bitmap: &[u8], i: usize) -> bool {
    bitmap[i / 8] & (1 << (i % 8)) != 0
}

fn set_bit(bitmap: &mut [u8], i: usize, used: bool) {
    if used {
        bitmap[i / 8] |= 1 << (i % 8);
    } else {
        bitmap[i / 8] &= !(1 << (i % 8));
    }
}

/// First-fit: scan bits `[0, total)` for the first run of `n` consecutive
/// clear bits, mark them used, and return the run's starting index. `n` of
/// zero, `n > total`, or `total` exceeding the slice's physical capacity are
/// all rejected (`OutOfRange`).
pub fn alloc(bitmap: &mut [u8], total: usize, n: usize) -> Result<usize, BitmapError> {
    if n == 0 || n > total || total > capacity(bitmap.len()) {
        return Err(BitmapError::OutOfRange);
    }
    let mut run_start = 0usize;
    let mut run_len = 0usize;
    for i in 0..total {
        if bit(bitmap, i) {
            run_len = 0;
            run_start = i + 1;
        } else {
            run_len += 1;
            if run_len == n {
                for j in run_start..run_start + n {
                    set_bit(bitmap, j, true);
                }
                return Ok(run_start);
            }
        }
    }
    Err(BitmapError::OutOfSpace)
}

/// Clear `n` bits starting at `start`, within `[0, total)`. Out-of-range is
/// rejected rather than silently clamped -- a caller passing a bad
/// `(start, n)` is a bug worth surfacing, not papering over.
pub fn free(bitmap: &mut [u8], total: usize, start: usize, n: usize) -> Result<(), BitmapError> {
    if total > capacity(bitmap.len()) {
        return Err(BitmapError::OutOfRange);
    }
    let end = start.checked_add(n).ok_or(BitmapError::OutOfRange)?;
    if end > total {
        return Err(BitmapError::OutOfRange);
    }
    for i in start..end {
        set_bit(bitmap, i, false);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_finds_first_fit() {
        let mut bm = [0u8; 2]; // 16 bits, all free
        assert_eq!(alloc(&mut bm, 16, 3), Ok(0));
        assert_eq!(bm[0], 0b0000_0111);
        assert_eq!(alloc(&mut bm, 16, 2), Ok(3));
        assert_eq!(bm[0], 0b0001_1111);
    }

    #[test]
    fn alloc_skips_used_run_finds_next() {
        let mut bm = [0u8; 1]; // 8 bits
        set_bit(&mut bm, 0, true);
        set_bit(&mut bm, 1, true);
        // bits [0,2) used; a run of 3 must start at bit 2.
        assert_eq!(alloc(&mut bm, 8, 3), Ok(2));
        assert_eq!(bm[0], 0b0001_1111);
    }

    #[test]
    fn alloc_fails_when_fragmented_below_request() {
        // Free bits in two runs of 2 (0,1) and (4,5), separated by used bits
        // (2,3) and trailing used bits (6,7): no run of 3 exists anywhere.
        let mut bm = [0b1100_1100u8]; // bits 2,3,6,7 used; 0,1,4,5 free
        assert_eq!(alloc(&mut bm, 8, 3), Err(BitmapError::OutOfSpace));
        // A run of 2 still succeeds, at the first fit (bit 0).
        assert_eq!(alloc(&mut bm, 8, 2), Ok(0));
    }

    #[test]
    fn free_then_realloc_reclaims_space() {
        let mut bm = [0u8; 1];
        let start = alloc(&mut bm, 8, 4).unwrap();
        assert_eq!(bm[0], 0b0000_1111);
        free(&mut bm, 8, start, 4).unwrap();
        assert_eq!(bm[0], 0);
        // The freed run is available again, not permanently lost.
        assert_eq!(alloc(&mut bm, 8, 4), Ok(0));
    }

    #[test]
    fn alloc_rejects_zero_or_over_total() {
        let mut bm = [0u8; 1]; // 8 physical bits
        assert_eq!(alloc(&mut bm, 8, 0), Err(BitmapError::OutOfRange));
        assert_eq!(alloc(&mut bm, 8, 9), Err(BitmapError::OutOfRange));
    }

    #[test]
    fn free_out_of_range_rejected() {
        let mut bm = [0u8; 1]; // 8 physical bits
        assert_eq!(free(&mut bm, 8, 6, 4), Err(BitmapError::OutOfRange));
        assert_eq!(free(&mut bm, 8, 8, 1), Err(BitmapError::OutOfRange));
    }

    #[test]
    fn full_bitmap_rejects_alloc() {
        let mut bm = [0xFFu8; 1];
        assert_eq!(alloc(&mut bm, 8, 1), Err(BitmapError::OutOfSpace));
    }

    #[test]
    fn total_smaller_than_capacity_protects_padding_bits() {
        // One byte physically holds 8 bits, but only the first 6 are real
        // sectors (e.g. a data area of 6 sectors backed by a whole bitmap
        // byte). alloc must never hand out bits 6 or 7.
        let mut bm = [0u8; 1];
        assert_eq!(alloc(&mut bm, 6, 6), Ok(0));
        assert_eq!(bm[0], 0b0011_1111); // bits 0..6 used, 6 and 7 untouched
        assert_eq!(alloc(&mut bm, 6, 1), Err(BitmapError::OutOfSpace));
        // A request that would only fit using the padding bits is rejected
        // outright (total=6 makes 7 sectors un-allocatable in this bitmap).
        assert_eq!(alloc(&mut bm, 6, 7), Err(BitmapError::OutOfRange));
    }

    #[test]
    fn total_exceeding_physical_capacity_rejected() {
        let mut bm = [0u8; 1]; // only 8 physical bits
        assert_eq!(alloc(&mut bm, 9, 1), Err(BitmapError::OutOfRange));
        assert_eq!(free(&mut bm, 9, 0, 1), Err(BitmapError::OutOfRange));
    }
}
