//! The read-only boot-archive on-disk format and its parser.
//!
//! This module is the CANONICAL definition of the format; xtask's archive
//! assembler mirrors these constants and offsets when it writes the image.
//! There is no shared crate -- the writer is host/std and a root-workspace
//! member, this reader is no_std/bare-target and outside that workspace, and
//! the boundary forbids a path dependency across it. So the two are kept in
//! sync by these comments, by a structural self-check in the assembler, and --
//! authoritatively -- by the kernel selftest that reads a real assembled image
//! off the virtio device and parses it with this code. Keep the layout dead
//! simple so that mirroring stays trivial.
//!
//! All integers are little-endian (x86-64). The unit is the 512-byte virtio
//! sector -- the same unit a `BlockRange` counts and `block_read` transfers --
//! so every structure is sector-addressed and the loader can read it with the
//! block syscall directly.
//!
//! ```text
//! sector 0      Superblock
//!   0   magic         [u8; 8]  = b"PLNTHAR1"
//!   8   entry_count   u32       number of directory entries
//!   12  dir_sectors   u32       sectors the directory occupies (starts at sector 1)
//!   16  total_sectors u32       whole archive size, in sectors
//!   20  ..512                   zero
//!
//! sector 1..    Directory: `entry_count` packed entries, each 40 bytes
//!   0   name          [u8; 32]  NUL-padded ASCII program name
//!   32  first_sector  u32       blob start sector, relative to archive start
//!   36  byte_len      u32       exact blob length in bytes
//!
//! then          Blobs, each starting on a sector boundary (ceil(byte_len/512)
//!               sectors), in directory order.
//! ```
//!
//! The parser is defensive in the same spirit as `elf::parse`: every field is
//! bounds-checked against the slice it was read from, every length is
//! validated with checked arithmetic, and a malformed field is *rejected*,
//! never clamped. The archive is built by trusted xtask today, but treating it
//! as untrusted input costs nothing and is the right discipline for when a
//! write path eventually produces images the kernel did not assemble.

/// The disk sector size, in bytes. The whole format is addressed in these.
pub const SECTOR: usize = 512;

/// Superblock magic: "PLNTHAR1" (Plinth archive, version 1). ASCII only.
pub const MAGIC: [u8; 8] = *b"PLNTHAR1";

/// Fixed directory-entry size, in bytes: a 32-byte name plus two u32 fields.
pub const ENTRY_SIZE: usize = 40;

/// Maximum program-name length. Names are NUL-padded to this in the directory.
pub const NAME_LEN: usize = 32;

// Field offsets, named to match the layout comment above.
const SB_MAGIC: usize = 0;
const SB_ENTRY_COUNT: usize = 8;
const SB_DIR_SECTORS: usize = 12;
const SB_TOTAL_SECTORS: usize = 16;
/// Bytes of the superblock the parser actually reads. The rest of sector 0 is
/// reserved zero; the parser does not require the caller to provide it.
pub const SB_USED: usize = 20;

const E_NAME: usize = 0;
const E_FIRST_SECTOR: usize = 32;
const E_BYTE_LEN: usize = 36;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    /// The superblock slice is shorter than the fields it must hold.
    Truncated,
    /// The superblock magic does not match -- not a Plinth archive.
    BadMagic,
    /// The directory slice is shorter than `entry_count * ENTRY_SIZE`.
    DirTruncated,
    /// `entry_count * ENTRY_SIZE` overflowed -- a corrupt count.
    DirOverflow,
    /// No directory entry matched the requested name.
    NotFound,
}

impl FsError {
    /// A short, stable message, matching the style of `ElfError::as_str`.
    pub fn as_str(self) -> &'static str {
        match self {
            FsError::Truncated => "archive: superblock truncated",
            FsError::BadMagic => "archive: bad magic",
            FsError::DirTruncated => "archive: directory truncated",
            FsError::DirOverflow => "archive: directory size overflow",
            FsError::NotFound => "archive: program not found",
        }
    }
}

/// The validated superblock: enough to find and bound the directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superblock {
    pub entry_count: u32,
    pub dir_sectors: u32,
    pub total_sectors: u32,
}

impl Superblock {
    /// Bytes the directory occupies: `entry_count * ENTRY_SIZE`, checked.
    /// `dir_sectors` is the on-disk rounding of this up to whole sectors; the
    /// loader reads `dir_sectors` sectors, but only this many bytes are entries.
    pub fn dir_bytes_len(&self) -> Result<usize, FsError> {
        (self.entry_count as usize)
            .checked_mul(ENTRY_SIZE)
            .ok_or(FsError::DirOverflow)
    }
}

/// A located program: where its blob starts (sector, relative to the archive)
/// and its exact byte length. The loader reads `ceil(byte_len / SECTOR)`
/// sectors from `first_sector` to recover the ELF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub first_sector: u32,
    pub byte_len: u32,
}

impl Entry {
    /// Sectors this blob occupies (its byte length rounded up to whole sectors).
    pub fn sector_count(&self) -> u32 {
        self.byte_len.div_ceil(SECTOR as u32)
    }
}

// Bounds-checked little-endian reads (mirror elf::parse's helpers): each
// returns None if the field would run past the slice.
fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

/// Validate `sector0` (the archive's first sector) and return its superblock,
/// or the first reason it is unacceptable. Only the first `SB_USED` bytes are
/// read; the caller may pass the whole 512-byte sector or just the prefix.
pub fn parse_superblock(sector0: &[u8]) -> Result<Superblock, FsError> {
    if sector0.len() < SB_USED {
        return Err(FsError::Truncated);
    }
    if sector0[SB_MAGIC..SB_MAGIC + MAGIC.len()] != MAGIC {
        return Err(FsError::BadMagic);
    }
    // Reads are within bounds by the SB_USED length check above.
    let entry_count = rd_u32(sector0, SB_ENTRY_COUNT).ok_or(FsError::Truncated)?;
    let dir_sectors = rd_u32(sector0, SB_DIR_SECTORS).ok_or(FsError::Truncated)?;
    let total_sectors = rd_u32(sector0, SB_TOTAL_SECTORS).ok_or(FsError::Truncated)?;
    Ok(Superblock { entry_count, dir_sectors, total_sectors })
}

/// Compare a directory entry's NUL-padded name field against `name`. Equal iff
/// the bytes before the first NUL match `name` exactly (so trailing padding is
/// ignored and an embedded NUL terminates, as on disk).
fn name_eq(field: &[u8], name: &[u8]) -> bool {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..end] == name
}

/// An iterator over the directory's `(name_field, Entry)` pairs. `dir` must be
/// the directory bytes (read from `Superblock::dir_bytes_len`); a too-short
/// slice yields nothing rather than reading past it.
pub fn entries<'a>(
    dir: &'a [u8],
    sb: &Superblock,
) -> impl Iterator<Item = (&'a [u8], Entry)> + 'a {
    let count = sb.entry_count as usize;
    (0..count).filter_map(move |i| {
        let base = i.checked_mul(ENTRY_SIZE)?;
        let rec = dir.get(base..base + ENTRY_SIZE)?;
        let name = &rec[E_NAME..E_NAME + NAME_LEN];
        let first_sector = rd_u32(rec, E_FIRST_SECTOR)?;
        let byte_len = rd_u32(rec, E_BYTE_LEN)?;
        Some((name, Entry { first_sector, byte_len }))
    })
}

/// Look up `name` in the directory and return its located blob, or `NotFound`.
/// `dir` must cover `entry_count * ENTRY_SIZE` bytes; a short slice is rejected
/// up front (a truncated directory must not silently hide entries).
pub fn find(dir: &[u8], sb: &Superblock, name: &[u8]) -> Result<Entry, FsError> {
    let need = sb.dir_bytes_len()?;
    if dir.len() < need {
        return Err(FsError::DirTruncated);
    }
    for (field, entry) in entries(dir, sb) {
        if name_eq(field, name) {
            return Ok(entry);
        }
    }
    Err(FsError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a minimal valid archive image (superblock sector + packed
    // directory) for two programs, in a Vec. Blob bytes are not included --
    // the parser never reads them; it only locates them.
    fn build(progs: &[(&str, u32, u32)]) -> Vec<u8> {
        let mut img = vec![0u8; SECTOR]; // superblock sector
        img[SB_MAGIC..SB_MAGIC + MAGIC.len()].copy_from_slice(&MAGIC);
        img[SB_ENTRY_COUNT..SB_ENTRY_COUNT + 4]
            .copy_from_slice(&(progs.len() as u32).to_le_bytes());
        let dir_bytes = progs.len() * ENTRY_SIZE;
        let dir_sectors = dir_bytes.div_ceil(SECTOR) as u32;
        img[SB_DIR_SECTORS..SB_DIR_SECTORS + 4].copy_from_slice(&dir_sectors.to_le_bytes());
        img[SB_TOTAL_SECTORS..SB_TOTAL_SECTORS + 4]
            .copy_from_slice(&(1 + dir_sectors).to_le_bytes());

        for (name, first_sector, byte_len) in progs {
            let mut rec = [0u8; ENTRY_SIZE];
            let nb = name.as_bytes();
            rec[E_NAME..E_NAME + nb.len()].copy_from_slice(nb);
            rec[E_FIRST_SECTOR..E_FIRST_SECTOR + 4].copy_from_slice(&first_sector.to_le_bytes());
            rec[E_BYTE_LEN..E_BYTE_LEN + 4].copy_from_slice(&byte_len.to_le_bytes());
            img.extend_from_slice(&rec);
        }
        img
    }

    #[test]
    fn parses_superblock_and_finds_programs() {
        let img = build(&[("hello-user", 2, 7000), ("list-user", 16, 9000)]);
        let sb = parse_superblock(&img[..SECTOR]).expect("superblock");
        assert_eq!(sb.entry_count, 2);
        assert_eq!(sb.total_sectors, 2); // superblock + 1 directory sector

        let dir = &img[SECTOR..];
        let hello = find(dir, &sb, b"hello-user").expect("hello-user present");
        assert_eq!(hello.first_sector, 2);
        assert_eq!(hello.byte_len, 7000);
        assert_eq!(hello.sector_count(), 14); // ceil(7000/512)

        let list = find(dir, &sb, b"list-user").expect("list-user present");
        assert_eq!(list.first_sector, 16);

        assert_eq!(find(dir, &sb, b"nope"), Err(FsError::NotFound));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut img = build(&[("a", 2, 1)]);
        img[1] = b'X';
        assert_eq!(parse_superblock(&img[..SECTOR]), Err(FsError::BadMagic));
    }

    #[test]
    fn rejects_truncated_superblock() {
        let img = build(&[("a", 2, 1)]);
        assert_eq!(parse_superblock(&img[..SB_USED - 1]), Err(FsError::Truncated));
    }

    #[test]
    fn rejects_truncated_directory() {
        // Superblock claims two entries; hand find() a directory holding one.
        let img = build(&[("a", 2, 1), ("b", 3, 1)]);
        let sb = parse_superblock(&img[..SECTOR]).unwrap();
        let short_dir = &img[SECTOR..SECTOR + ENTRY_SIZE]; // only the first entry
        assert_eq!(find(short_dir, &sb, b"b"), Err(FsError::DirTruncated));
    }

    #[test]
    fn directory_byte_length_overflow_is_rejected() {
        // A corrupt entry_count whose ENTRY_SIZE product overflows usize must
        // be reported, not wrapped into a small (and falsely satisfiable) len.
        let sb = Superblock {
            entry_count: u32::MAX,
            dir_sectors: 1,
            total_sectors: 1,
        };
        // On a 64-bit host u32::MAX * 40 does not overflow usize, so this
        // surfaces as a truncated directory rather than overflow; either way
        // it is rejected and never reads past the slice.
        assert!(matches!(
            find(&[0u8; ENTRY_SIZE], &sb, b"x"),
            Err(FsError::DirTruncated) | Err(FsError::DirOverflow)
        ));
    }

    #[test]
    fn name_match_ignores_padding_and_respects_nul() {
        let field = b"hello-user\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
        assert!(name_eq(field, b"hello-user"));
        assert!(!name_eq(field, b"hello"));
        assert!(!name_eq(field, b"hello-user-x"));
    }
}
