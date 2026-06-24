//! A fixed-maximum-entry, mutable directory (Design/readwrite_fs.md S3).
//!
//! Same packed-record shape as the read-only archive's directory
//! (`libfs::archive`: 40-byte records, a 32-byte NUL-padded name plus
//! `first_sector`/`byte_len` u32 fields) and the same "an all-zero name field
//! means free" convention -- but here a slot is reusable: `remove` zeroes the
//! name field instead of the table being fixed forever at build time. Pure
//! functions over a caller-owned `&mut [u8]`, host-testable like the archive
//! parser.

pub const ENTRY_SIZE: usize = 40;
pub const NAME_LEN: usize = 32;

const E_NAME: usize = 0;
const E_FIRST_SECTOR: usize = 32;
const E_BYTE_LEN: usize = 36;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirError {
    /// Every slot in the directory's fixed capacity is in use.
    Full,
    /// No entry matches the requested name.
    NotFound,
    /// `create` was given a name already present.
    Duplicate,
    /// The name is empty or longer than `NAME_LEN` -- rejected outright
    /// rather than silently truncated (an empty name is indistinguishable
    /// from a free slot's all-zero field).
    InvalidName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub first_sector: u32,
    pub byte_len: u32,
}

impl Entry {
    /// Sectors this file occupies (its byte length rounded up to whole
    /// sectors) -- what `bitmap::free` needs to release the run.
    pub fn sector_count(&self) -> u32 {
        self.byte_len.div_ceil(512)
    }
}

/// Entry slots a directory of `bytes` bytes holds. Any trailing bytes short
/// of a full `ENTRY_SIZE` record are unused, the same as integer division.
pub fn capacity(bytes: usize) -> usize {
    bytes / ENTRY_SIZE
}

fn name_eq(field: &[u8], name: &[u8]) -> bool {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..end] == name
}

/// A zeroed name field is the free-slot marker (mirrors `libfs::archive`:
/// an all-zero field matches no name, so it can double as "unused").
fn is_free(field: &[u8]) -> bool {
    field[0] == 0
}

fn slot(dir: &[u8], i: usize) -> &[u8] {
    &dir[i * ENTRY_SIZE..(i + 1) * ENTRY_SIZE]
}

fn slot_mut(dir: &mut [u8], i: usize) -> &mut [u8] {
    &mut dir[i * ENTRY_SIZE..(i + 1) * ENTRY_SIZE]
}

fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

fn entry_of(rec: &[u8]) -> Entry {
    Entry {
        first_sector: rd_u32(rec, E_FIRST_SECTOR),
        byte_len: rd_u32(rec, E_BYTE_LEN),
    }
}

/// Look up `name`. `NotFound` if absent.
pub fn find(dir: &[u8], name: &[u8]) -> Result<Entry, DirError> {
    let count = capacity(dir.len());
    for i in 0..count {
        let rec = slot(dir, i);
        if !is_free(rec) && name_eq(&rec[E_NAME..E_NAME + NAME_LEN], name) {
            return Ok(entry_of(rec));
        }
    }
    Err(DirError::NotFound)
}

/// Insert `name` -> `(first_sector, byte_len)` into the first free slot.
/// Rejects a name already present (`Duplicate`), an empty or oversize name
/// (`InvalidName`), or a full directory (`Full`).
pub fn create(dir: &mut [u8], name: &[u8], first_sector: u32, byte_len: u32) -> Result<(), DirError> {
    if name.is_empty() || name.len() > NAME_LEN {
        return Err(DirError::InvalidName);
    }
    if find(dir, name).is_ok() {
        return Err(DirError::Duplicate);
    }
    let count = capacity(dir.len());
    for i in 0..count {
        if is_free(slot(dir, i)) {
            let rec = slot_mut(dir, i);
            rec.fill(0);
            rec[E_NAME..E_NAME + name.len()].copy_from_slice(name);
            rec[E_FIRST_SECTOR..E_FIRST_SECTOR + 4].copy_from_slice(&first_sector.to_le_bytes());
            rec[E_BYTE_LEN..E_BYTE_LEN + 4].copy_from_slice(&byte_len.to_le_bytes());
            return Ok(());
        }
    }
    Err(DirError::Full)
}

/// Remove `name`, returning its freed `Entry` so the caller can release its
/// bitmap run. `NotFound` if absent.
pub fn remove(dir: &mut [u8], name: &[u8]) -> Result<Entry, DirError> {
    let count = capacity(dir.len());
    for i in 0..count {
        let rec = slot(dir, i);
        if !is_free(rec) && name_eq(&rec[E_NAME..E_NAME + NAME_LEN], name) {
            let entry = entry_of(rec);
            slot_mut(dir, i).fill(0);
            return Ok(entry);
        }
    }
    Err(DirError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_find_two_entries() {
        let mut dir = [0u8; ENTRY_SIZE * 4];
        create(&mut dir, b"a.txt", 32, 5).unwrap();
        create(&mut dir, b"b.txt", 33, 7).unwrap();

        let a = find(&dir, b"a.txt").unwrap();
        assert_eq!((a.first_sector, a.byte_len), (32, 5));
        let b = find(&dir, b"b.txt").unwrap();
        assert_eq!((b.first_sector, b.byte_len), (33, 7));
        assert_eq!(find(&dir, b"c.txt"), Err(DirError::NotFound));
    }

    #[test]
    fn create_rejects_duplicate_name() {
        let mut dir = [0u8; ENTRY_SIZE * 2];
        create(&mut dir, b"a.txt", 32, 5).unwrap();
        assert_eq!(create(&mut dir, b"a.txt", 99, 1), Err(DirError::Duplicate));
    }

    #[test]
    fn create_rejects_invalid_names() {
        let mut dir = [0u8; ENTRY_SIZE * 2];
        assert_eq!(create(&mut dir, b"", 32, 5), Err(DirError::InvalidName));
        let long = [b'x'; NAME_LEN + 1];
        assert_eq!(create(&mut dir, &long, 32, 5), Err(DirError::InvalidName));
    }

    #[test]
    fn directory_full_when_all_slots_used() {
        let mut dir = [0u8; ENTRY_SIZE * 2];
        create(&mut dir, b"a.txt", 1, 1).unwrap();
        create(&mut dir, b"b.txt", 2, 1).unwrap();
        assert_eq!(create(&mut dir, b"c.txt", 3, 1), Err(DirError::Full));
    }

    #[test]
    fn remove_then_reuse_slot_others_unaffected() {
        let mut dir = [0u8; ENTRY_SIZE * 2];
        create(&mut dir, b"a.txt", 32, 5).unwrap();
        create(&mut dir, b"b.txt", 33, 7).unwrap();

        let freed = remove(&mut dir, b"a.txt").unwrap();
        assert_eq!((freed.first_sector, freed.byte_len), (32, 5));
        assert_eq!(find(&dir, b"a.txt"), Err(DirError::NotFound));

        // The freed slot is reusable -- a directory sized for exactly two
        // entries still accepts a third create after one delete.
        create(&mut dir, b"c.txt", 40, 9).unwrap();
        let c = find(&dir, b"c.txt").unwrap();
        assert_eq!((c.first_sector, c.byte_len), (40, 9));

        // b.txt must be untouched by a.txt's delete/reuse cycle.
        let b = find(&dir, b"b.txt").unwrap();
        assert_eq!((b.first_sector, b.byte_len), (33, 7));
    }

    #[test]
    fn remove_not_found() {
        let mut dir = [0u8; ENTRY_SIZE * 2];
        assert_eq!(remove(&mut dir, b"nope"), Err(DirError::NotFound));
    }

    #[test]
    fn sector_count_rounds_up() {
        assert_eq!(Entry { first_sector: 0, byte_len: 512 }.sector_count(), 1);
        assert_eq!(Entry { first_sector: 0, byte_len: 513 }.sector_count(), 2);
        assert_eq!(Entry { first_sector: 0, byte_len: 0 }.sector_count(), 0);
    }
}
