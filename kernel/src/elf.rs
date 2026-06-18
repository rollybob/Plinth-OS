//! A small, strict, no-allocation ELF64 loader.
//!
//! This is the first kernel code that parses untrusted input, so the
//! discipline is deliberate: every read is bounds-checked against the file
//! length, every malformed field is *rejected* (never clamped or guessed),
//! and nothing is allocated -- parsing produces a fixed-size description of
//! the image, which the caller then maps.
//!
//! Scope (ABI v1): a static, non-PIE `ET_EXEC` x86-64 ELF. PT_LOAD
//! segments are mapped verbatim at their `p_vaddr`; no relocations are
//! applied and no dynamic linking is supported (a `PT_INTERP` is rejected,
//! and any other non-PT_LOAD header -- including a vestigial PT_DYNAMIC --
//! is ignored). Programs must therefore be linked static and non-PIE; the
//! `*-user` crates pass `-no-pie` to do exactly that. See ABI.md.
//!
//! `parse` is a pure function over a byte slice -- it touches no frames and
//! no page tables -- which is what lets the whole validator be unit-tested
//! in the in-kernel test build without ever entering ring 3.
//!
//! ## D8a untrusted-input audit (2026-06-18)
//!
//! When programs load from disk, the bytes originate in a library OS buffer,
//! not the kernel's embedded table -- a libOS-supplied ELF can lie about every
//! field. This validator was audited against that threat model before the
//! spawn-from-buffer path was wired, and found sufficient:
//!
//! - Every header/phdr field is read through the bounds-checked `rd_*` helpers,
//!   which return `TooSmall` rather than read past the slice.
//! - The program-header table extent (`e_phoff + e_phnum * e_phentsize`) is
//!   computed with `checked_mul`/`checked_add` and rejected if it overflows or
//!   exceeds the slice, so every per-header read below it is in bounds. `phnum`
//!   is capped (`MAX_PHDRS`) and `phentsize` pinned to the exact Elf64 stride.
//! - Each segment's file range (`p_offset + p_filesz`) and address span
//!   (`p_vaddr + p_memsz`) use checked arithmetic; the file range must lie
//!   within the slice, the address span within the caller's image window, and
//!   `p_filesz <= p_memsz`. Overlap between accepted segments is rejected.
//! - W^X, readability, alignment, the page budget, and entry-in-an-executable-
//!   segment are all enforced; a bad field is rejected, never clamped.
//!
//! CALLER CONTRACT (the half this function cannot enforce): `parse` validates
//! offsets and sizes against `bytes.len()`, so the caller MUST pass that exact
//! same slice to `load_and_map`, and `bytes` must be a single readable region
//! valid for its whole length. A `LoadSeg`'s `offset`/`filesz` are safe to copy
//! from *only* relative to the slice they were validated against.

use x86_64::structures::paging::PageTableFlags;

/// Page size the loader aligns and counts in. Matches FRAME_SIZE, kept
/// local so the parser depends on nothing outside this module.
const PAGE: u64 = 4096;

// ELF identification and header field values we accept.
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;

// Program-header types and the fixed Elf64 sizes.
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;

// p_flags bits.
const PF_X: u32 = 1 << 0;
const PF_W: u32 = 1 << 1;
const PF_R: u32 = 1 << 2;

/// Hard caps -- no heap, so every collection is a fixed array. A program
/// header table longer than this, or with more PT_LOAD segments than this,
/// is rejected rather than truncated.
pub const MAX_PHDRS: usize = 16;
pub const MAX_LOAD_SEGS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// File too short to hold the structure being read.
    TooSmall,
    BadMagic,
    BadClass,
    BadData,
    BadVersion,
    /// Not ET_EXEC (e.g. a PIE/ET_DYN -- link with -no-pie).
    NotExec,
    BadMachine,
    /// e_phentsize is not sizeof(Elf64_Phdr).
    BadPhEntSize,
    TooManyPhdrs,
    /// The program-header table runs past the end of the file.
    PhdrsOutOfBounds,
    /// No loadable (PT_LOAD) segment with a nonzero size.
    NoLoadable,
    /// A PT_INTERP requests a dynamic loader; unsupported.
    DynamicInterp,
    /// A segment's file range (p_offset + p_filesz) runs past the file.
    SegmentFileRange,
    /// p_filesz exceeds p_memsz.
    SegmentSizes,
    /// p_vaddr is not page-aligned.
    SegmentUnaligned,
    /// p_vaddr + p_memsz overflows.
    SegmentOverflow,
    /// A segment falls outside the permitted image window.
    SegmentOutOfWindow,
    /// Two segments cover overlapping virtual addresses.
    SegmentOverlap,
    /// A segment is not readable, or carries flag bits we do not honor.
    BadFlags,
    /// A segment is both writable and executable (W^X).
    WxViolation,
    /// The image needs more pages than the caller allows.
    TooLarge,
    /// e_entry does not land inside an executable segment.
    BadEntry,
}

impl ElfError {
    /// A short, stable message for the boot path's `&'static str` errors.
    pub fn as_str(self) -> &'static str {
        match self {
            ElfError::TooSmall => "elf: file too small",
            ElfError::BadMagic => "elf: bad magic",
            ElfError::BadClass => "elf: not 64-bit",
            ElfError::BadData => "elf: not little-endian",
            ElfError::BadVersion => "elf: bad version",
            ElfError::NotExec => "elf: not ET_EXEC (link with -no-pie)",
            ElfError::BadMachine => "elf: not x86-64",
            ElfError::BadPhEntSize => "elf: bad phentsize",
            ElfError::TooManyPhdrs => "elf: too many program headers",
            ElfError::PhdrsOutOfBounds => "elf: phdr table out of bounds",
            ElfError::NoLoadable => "elf: no loadable segment",
            ElfError::DynamicInterp => "elf: dynamic linking unsupported",
            ElfError::SegmentFileRange => "elf: segment file range out of bounds",
            ElfError::SegmentSizes => "elf: p_filesz exceeds p_memsz",
            ElfError::SegmentUnaligned => "elf: segment vaddr not page-aligned",
            ElfError::SegmentOverflow => "elf: segment address overflow",
            ElfError::SegmentOutOfWindow => "elf: segment outside image window",
            ElfError::SegmentOverlap => "elf: segments overlap",
            ElfError::BadFlags => "elf: bad segment flags",
            ElfError::WxViolation => "elf: write+execute segment",
            ElfError::TooLarge => "elf: image too large",
            ElfError::BadEntry => "elf: entry not in an executable segment",
        }
    }
}

/// One validated PT_LOAD segment: where it lives, what backs it in the
/// file, and the access it asks for. `flags` is the raw `p_flags`, already
/// checked for readability and W^X; `page_flags` turns it into the mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadSeg {
    pub vaddr: u64,
    pub offset: usize,
    pub filesz: usize,
    pub memsz: u64,
    pub flags: u32,
}

impl LoadSeg {
    /// Page-table flags for this segment: always present and user, writable
    /// only if PF_W, and non-executable unless PF_X. The W^X invariant was
    /// enforced at parse time, so at most one of WRITABLE / executable holds.
    pub fn page_flags(&self) -> PageTableFlags {
        let mut f = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        if self.flags & PF_W != 0 {
            f |= PageTableFlags::WRITABLE;
        }
        if self.flags & PF_X == 0 {
            f |= PageTableFlags::NO_EXECUTE;
        }
        f
    }

    /// Number of pages this segment occupies in memory (covers the .bss
    /// tail, where p_memsz > p_filesz).
    pub fn pages(&self) -> u64 {
        self.memsz.div_ceil(PAGE)
    }
}

/// A validated image: the entry point and its loadable segments. Bounded,
/// copyable, owns no memory -- just a plan the caller carries out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Image {
    pub entry: u64,
    segs: [Option<LoadSeg>; MAX_LOAD_SEGS],
}

impl Image {
    /// The loadable segments, in the order they appeared in the file.
    pub fn segments(&self) -> impl Iterator<Item = &LoadSeg> {
        self.segs.iter().flatten()
    }

    /// Total in-memory footprint of the image: the sum of every PT_LOAD
    /// segment's memory size (including .bss tails). This is what the
    /// program actually occupies once mapped, distinct from the ELF file
    /// size, which also carries headers and the section table.
    pub fn image_bytes(&self) -> u64 {
        self.segments().map(|s| s.memsz).sum()
    }
}

// Bounds-checked little-endian reads. Each returns None if the field would
// run past the end of the slice -- the single place out-of-range input is
// turned into a clean failure instead of a panic or a wild read.
fn rd_u16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn rd_u64(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Validate `bytes` as a static ET_EXEC x86-64 ELF and return its loadable
/// image, or the first reason it is unacceptable.
///
/// All PT_LOAD segments must lie within `[image_base, image_end)` and the
/// image must fit in `max_image_pages` pages. The window is the caller's
/// guarantee that a segment can never land on the stack, the map window, or
/// the kernel half -- so this function only has to check containment, not
/// every forbidden region individually.
pub fn parse(
    bytes: &[u8],
    image_base: u64,
    image_end: u64,
    max_image_pages: u64,
) -> Result<Image, ElfError> {
    // --- ELF header ---
    if bytes.len() < EHDR_SIZE {
        return Err(ElfError::TooSmall);
    }
    if bytes[0..4] != ELF_MAGIC {
        return Err(ElfError::BadMagic);
    }
    if bytes[4] != ELFCLASS64 {
        return Err(ElfError::BadClass);
    }
    if bytes[5] != ELFDATA2LSB {
        return Err(ElfError::BadData);
    }
    if bytes[6] != EV_CURRENT {
        return Err(ElfError::BadVersion);
    }
    // e_type @16, e_machine @18, e_version @20, e_entry @24, e_phoff @32,
    // e_phentsize @54, e_phnum @56. Reads use the bounds-checked helpers.
    if rd_u16(bytes, 16).ok_or(ElfError::TooSmall)? != ET_EXEC {
        return Err(ElfError::NotExec);
    }
    if rd_u16(bytes, 18).ok_or(ElfError::TooSmall)? != EM_X86_64 {
        return Err(ElfError::BadMachine);
    }
    if rd_u32(bytes, 20).ok_or(ElfError::TooSmall)? != EV_CURRENT as u32 {
        return Err(ElfError::BadVersion);
    }
    let entry = rd_u64(bytes, 24).ok_or(ElfError::TooSmall)?;
    let phoff = rd_u64(bytes, 32).ok_or(ElfError::TooSmall)?;
    let phentsize = rd_u16(bytes, 54).ok_or(ElfError::TooSmall)?;
    let phnum = rd_u16(bytes, 56).ok_or(ElfError::TooSmall)?;

    if phentsize as usize != PHDR_SIZE {
        return Err(ElfError::BadPhEntSize);
    }
    if phnum as usize > MAX_PHDRS {
        return Err(ElfError::TooManyPhdrs);
    }
    // The whole program-header table must lie within the file.
    let ph_table = (phnum as usize)
        .checked_mul(PHDR_SIZE)
        .and_then(|sz| (phoff as usize).checked_add(sz))
        .ok_or(ElfError::PhdrsOutOfBounds)?;
    if ph_table > bytes.len() {
        return Err(ElfError::PhdrsOutOfBounds);
    }

    // --- program headers ---
    let mut segs: [Option<LoadSeg>; MAX_LOAD_SEGS] = [None; MAX_LOAD_SEGS];
    let mut nsegs = 0usize;
    let mut total_pages = 0u64;

    for i in 0..phnum as usize {
        let base = phoff as usize + i * PHDR_SIZE;
        // Within bounds by the ph_table check above.
        let p_type = rd_u32(bytes, base).ok_or(ElfError::PhdrsOutOfBounds)?;

        if p_type == PT_INTERP {
            return Err(ElfError::DynamicInterp);
        }
        if p_type != PT_LOAD {
            // PT_DYNAMIC, PT_GNU_STACK, PT_NOTE, ... -- nothing to map.
            continue;
        }

        let p_flags = rd_u32(bytes, base + 4).ok_or(ElfError::PhdrsOutOfBounds)?;
        let p_offset = rd_u64(bytes, base + 8).ok_or(ElfError::PhdrsOutOfBounds)?;
        let p_vaddr = rd_u64(bytes, base + 16).ok_or(ElfError::PhdrsOutOfBounds)?;
        let p_filesz = rd_u64(bytes, base + 32).ok_or(ElfError::PhdrsOutOfBounds)?;
        let p_memsz = rd_u64(bytes, base + 40).ok_or(ElfError::PhdrsOutOfBounds)?;

        // A zero-memory segment maps nothing; skip it.
        if p_memsz == 0 {
            continue;
        }

        // Sizes and file range.
        if p_filesz > p_memsz {
            return Err(ElfError::SegmentSizes);
        }
        let file_end = (p_offset)
            .checked_add(p_filesz)
            .ok_or(ElfError::SegmentFileRange)?;
        if file_end > bytes.len() as u64 {
            return Err(ElfError::SegmentFileRange);
        }

        // Placement: page-aligned and entirely inside the image window.
        if p_vaddr % PAGE != 0 {
            return Err(ElfError::SegmentUnaligned);
        }
        let seg_end = p_vaddr.checked_add(p_memsz).ok_or(ElfError::SegmentOverflow)?;
        if p_vaddr < image_base || seg_end > image_end {
            return Err(ElfError::SegmentOutOfWindow);
        }

        // Flags: must be readable; honor only R/W/X; never both W and X.
        if p_flags & PF_R == 0 || p_flags & !(PF_R | PF_W | PF_X) != 0 {
            return Err(ElfError::BadFlags);
        }
        if p_flags & PF_W != 0 && p_flags & PF_X != 0 {
            return Err(ElfError::WxViolation);
        }

        // No overlap with an already-accepted segment.
        for prior in segs.iter().flatten() {
            let prior_end = prior.vaddr + prior.memsz; // checked when accepted
            if p_vaddr < prior_end && prior.vaddr < seg_end {
                return Err(ElfError::SegmentOverlap);
            }
        }

        // Page budget.
        let pages = p_memsz.div_ceil(PAGE);
        total_pages = total_pages.checked_add(pages).ok_or(ElfError::TooLarge)?;
        if total_pages > max_image_pages {
            return Err(ElfError::TooLarge);
        }

        // nsegs cannot exceed MAX_LOAD_SEGS: phnum <= MAX_PHDRS == it.
        segs[nsegs] = Some(LoadSeg {
            vaddr: p_vaddr,
            offset: p_offset as usize,
            filesz: p_filesz as usize,
            memsz: p_memsz,
            flags: p_flags,
        });
        nsegs += 1;
    }

    if nsegs == 0 {
        return Err(ElfError::NoLoadable);
    }

    // The entry must sit inside a segment we will actually map executable.
    let entry_ok = segs
        .iter()
        .flatten()
        .any(|s| s.flags & PF_X != 0 && (s.vaddr..s.vaddr + s.memsz).contains(&entry));
    if !entry_ok {
        return Err(ElfError::BadEntry);
    }

    Ok(Image { entry, segs })
}
