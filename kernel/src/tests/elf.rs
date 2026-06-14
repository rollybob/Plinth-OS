//! ELF loader tests.
//!
//! The validator is a pure function over a byte slice, so the whole attack
//! surface is exercised here without ever building a real binary or
//! entering ring 3: each test hand-assembles an ELF image in a fixed
//! buffer, tweaks one field, and asserts the exact rejection (or, for the
//! good cases, the parsed result).

use super::TestCtx;
use crate::elf::{parse, ElfError};
use crate::test_assert;

// Synthetic image window for the tests -- independent of the real layout.
const IMG_BASE: u64 = 0x40_0000;
const IMG_END: u64 = 0x50_0000;
const MAX_PAGES: u64 = 60;

// Field offsets we tweak, named to match the ELF64 layout.
const E_TYPE: usize = 16;
const E_MACHINE: usize = 18;
const E_PHNUM: usize = 56;
const PH0: usize = 64;
const PH0_TYPE: usize = PH0;
const PH0_FLAGS: usize = PH0 + 4;
const PH0_VADDR: usize = PH0 + 16;
const PH0_FILESZ: usize = PH0 + 32;
const PH0_MEMSZ: usize = PH0 + 40;

const PT_LOAD: u32 = 1;
const PF_R: u32 = 4;
const PF_W: u32 = 2;
const PF_X: u32 = 1;

fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// Lay a minimal, valid static ET_EXEC into `buf`: one R-X PT_LOAD of 16
/// bytes at IMG_BASE, entry at IMG_BASE. Returns the file length used.
/// Segment file data sits right after the single program header.
fn minimal(buf: &mut [u8]) -> usize {
    const SEG_OFF: usize = PH0 + 56; // 120
    const SEG_LEN: usize = 16;

    buf[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    buf[4] = 2; // ELFCLASS64
    buf[5] = 1; // ELFDATA2LSB
    buf[6] = 1; // EV_CURRENT
    put_u16(buf, E_TYPE, 2); // ET_EXEC
    put_u16(buf, E_MACHINE, 62); // EM_X86_64
    put_u32(buf, 20, 1); // e_version
    put_u64(buf, 24, IMG_BASE); // e_entry
    put_u64(buf, 32, PH0 as u64); // e_phoff
    put_u16(buf, 54, 56); // e_phentsize
    put_u16(buf, E_PHNUM, 1); // e_phnum

    put_u32(buf, PH0_TYPE, PT_LOAD);
    put_u32(buf, PH0_FLAGS, PF_R | PF_X);
    put_u64(buf, PH0 + 8, SEG_OFF as u64); // p_offset
    put_u64(buf, PH0_VADDR, IMG_BASE); // p_vaddr
    put_u64(buf, PH0 + 24, IMG_BASE); // p_paddr
    put_u64(buf, PH0_FILESZ, SEG_LEN as u64);
    put_u64(buf, PH0_MEMSZ, SEG_LEN as u64);
    put_u64(buf, PH0 + 48, 0x1000); // p_align

    for byte in buf.iter_mut().skip(SEG_OFF).take(SEG_LEN) {
        *byte = 0xcc;
    }
    SEG_OFF + SEG_LEN
}

fn check(buf: &[u8], len: usize) -> Result<crate::elf::Image, ElfError> {
    parse(&buf[..len], IMG_BASE, IMG_END, MAX_PAGES)
}

pub fn valid_minimal(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    let img = check(&buf, len).map_err(|_| "valid minimal exec rejected")?;
    test_assert!(img.entry == IMG_BASE, "wrong entry");
    test_assert!(img.segments().count() == 1, "expected one segment");
    Ok(())
}

/// A three-segment image (R-X / R-- / RW-), the shape lld emits for the
/// real user crates, parses and preserves W^X in the mapped flags.
pub fn valid_three_segments(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 512];
    const SEG_OFF: usize = PH0 + 3 * 56; // after three phdrs
    buf[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    buf[4] = 2;
    buf[5] = 1;
    buf[6] = 1;
    put_u16(&mut buf, E_TYPE, 2);
    put_u16(&mut buf, E_MACHINE, 62);
    put_u32(&mut buf, 20, 1);
    put_u64(&mut buf, 24, IMG_BASE);
    put_u64(&mut buf, 32, PH0 as u64);
    put_u16(&mut buf, 54, 56);
    put_u16(&mut buf, E_PHNUM, 3);

    let segs = [
        (IMG_BASE, PF_R | PF_X),
        (IMG_BASE + 0x1000, PF_R),
        (IMG_BASE + 0x2000, PF_R | PF_W),
    ];
    for (i, (vaddr, flags)) in segs.iter().enumerate() {
        let ph = PH0 + i * 56;
        put_u32(&mut buf, ph, PT_LOAD);
        put_u32(&mut buf, ph + 4, *flags);
        put_u64(&mut buf, ph + 8, SEG_OFF as u64);
        put_u64(&mut buf, ph + 16, *vaddr);
        put_u64(&mut buf, ph + 32, 16); // p_filesz
        put_u64(&mut buf, ph + 40, 16); // p_memsz
        put_u64(&mut buf, ph + 48, 0x1000);
    }
    let len = SEG_OFF + 16;

    let img = check(&buf, len).map_err(|_| "three-segment image rejected")?;
    test_assert!(img.segments().count() == 3, "expected three segments");

    use x86_64::structures::paging::PageTableFlags;
    let mut text = None;
    let mut data = None;
    for s in img.segments() {
        if s.vaddr == IMG_BASE {
            text = Some(s.page_flags());
        }
        if s.vaddr == IMG_BASE + 0x2000 {
            data = Some(s.page_flags());
        }
    }
    let text = text.ok_or("text segment missing")?;
    let data = data.ok_or("data segment missing")?;
    test_assert!(!text.contains(PageTableFlags::WRITABLE), "text segment writable");
    test_assert!(!text.contains(PageTableFlags::NO_EXECUTE), "text segment non-executable");
    test_assert!(data.contains(PageTableFlags::WRITABLE), "data segment not writable");
    test_assert!(data.contains(PageTableFlags::NO_EXECUTE), "data segment executable");
    Ok(())
}

pub fn too_small(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let buf = [0u8; 10];
    test_assert!(check(&buf, 10) == Err(ElfError::TooSmall), "tiny file not rejected");
    Ok(())
}

pub fn bad_magic(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    buf[1] = b'X';
    test_assert!(check(&buf, len) == Err(ElfError::BadMagic), "bad magic not rejected");
    Ok(())
}

pub fn bad_class(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    buf[4] = 1; // ELFCLASS32
    test_assert!(check(&buf, len) == Err(ElfError::BadClass), "32-bit not rejected");
    Ok(())
}

pub fn not_exec(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    put_u16(&mut buf, E_TYPE, 3); // ET_DYN (a PIE)
    test_assert!(check(&buf, len) == Err(ElfError::NotExec), "ET_DYN not rejected");
    Ok(())
}

pub fn bad_machine(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    put_u16(&mut buf, E_MACHINE, 0xB7); // AArch64
    test_assert!(check(&buf, len) == Err(ElfError::BadMachine), "wrong arch not rejected");
    Ok(())
}

pub fn phdrs_out_of_bounds(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    put_u16(&mut buf, E_PHNUM, 2); // claims two phdrs; file holds one
    test_assert!(
        check(&buf, len) == Err(ElfError::PhdrsOutOfBounds),
        "phdr table past EOF not rejected"
    );
    Ok(())
}

pub fn segment_file_range(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    put_u64(&mut buf, PH0_FILESZ, 4096); // file data runs past EOF
    put_u64(&mut buf, PH0_MEMSZ, 4096);
    test_assert!(
        check(&buf, len) == Err(ElfError::SegmentFileRange),
        "segment past EOF not rejected"
    );
    Ok(())
}

pub fn segment_sizes(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    put_u64(&mut buf, PH0_FILESZ, 32); // filesz > memsz (16)
    test_assert!(
        check(&buf, len) == Err(ElfError::SegmentSizes),
        "filesz > memsz not rejected"
    );
    Ok(())
}

pub fn segment_unaligned(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    put_u64(&mut buf, PH0_VADDR, IMG_BASE + 1); // not page-aligned
    test_assert!(
        check(&buf, len) == Err(ElfError::SegmentUnaligned),
        "unaligned vaddr not rejected"
    );
    Ok(())
}

pub fn segment_out_of_window(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    put_u64(&mut buf, PH0_VADDR, IMG_END); // at/after the window end
    test_assert!(
        check(&buf, len) == Err(ElfError::SegmentOutOfWindow),
        "out-of-window segment not rejected"
    );
    Ok(())
}

pub fn wx_violation(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    put_u32(&mut buf, PH0_FLAGS, PF_R | PF_W | PF_X);
    test_assert!(
        check(&buf, len) == Err(ElfError::WxViolation),
        "write+execute segment not rejected"
    );
    Ok(())
}

pub fn bad_flags_unreadable(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    put_u32(&mut buf, PH0_FLAGS, PF_X); // executable but not readable
    test_assert!(
        check(&buf, len) == Err(ElfError::BadFlags),
        "non-readable segment not rejected"
    );
    Ok(())
}

pub fn dynamic_interp(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    put_u32(&mut buf, PH0_TYPE, 3); // PT_INTERP
    test_assert!(
        check(&buf, len) == Err(ElfError::DynamicInterp),
        "PT_INTERP not rejected"
    );
    Ok(())
}

pub fn no_loadable(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    put_u32(&mut buf, PH0_TYPE, 4); // PT_NOTE -- skipped, leaving nothing
    test_assert!(
        check(&buf, len) == Err(ElfError::NoLoadable),
        "image with no PT_LOAD not rejected"
    );
    Ok(())
}

pub fn too_large(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    // 61 pages of memory, one more than MAX_PAGES; filesz stays tiny.
    put_u64(&mut buf, PH0_MEMSZ, (MAX_PAGES + 1) * 4096);
    test_assert!(check(&buf, len) == Err(ElfError::TooLarge), "oversize image not rejected");
    Ok(())
}

pub fn bad_entry(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 256];
    let len = minimal(&mut buf);
    put_u64(&mut buf, 24, IMG_BASE + 0x1000); // entry outside the only segment
    test_assert!(check(&buf, len) == Err(ElfError::BadEntry), "bad entry not rejected");
    Ok(())
}

pub fn segment_overlap(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut buf = [0u8; 512];
    const SEG_OFF: usize = PH0 + 2 * 56;
    buf[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    buf[4] = 2;
    buf[5] = 1;
    buf[6] = 1;
    put_u16(&mut buf, E_TYPE, 2);
    put_u16(&mut buf, E_MACHINE, 62);
    put_u32(&mut buf, 20, 1);
    put_u64(&mut buf, 24, IMG_BASE);
    put_u64(&mut buf, 32, PH0 as u64);
    put_u16(&mut buf, 54, 56);
    put_u16(&mut buf, E_PHNUM, 2);

    // seg0: [IMG_BASE, IMG_BASE+0x2000), R-X
    put_u32(&mut buf, PH0, PT_LOAD);
    put_u32(&mut buf, PH0 + 4, PF_R | PF_X);
    put_u64(&mut buf, PH0 + 8, SEG_OFF as u64);
    put_u64(&mut buf, PH0 + 16, IMG_BASE);
    put_u64(&mut buf, PH0 + 32, 16);
    put_u64(&mut buf, PH0 + 40, 0x2000);
    // seg1: starts inside seg0, RW-
    put_u32(&mut buf, PH0 + 56, PT_LOAD);
    put_u32(&mut buf, PH0 + 56 + 4, PF_R | PF_W);
    put_u64(&mut buf, PH0 + 56 + 8, SEG_OFF as u64);
    put_u64(&mut buf, PH0 + 56 + 16, IMG_BASE + 0x1000);
    put_u64(&mut buf, PH0 + 56 + 32, 16);
    put_u64(&mut buf, PH0 + 56 + 40, 16);

    let len = SEG_OFF + 16;
    test_assert!(
        check(&buf, len) == Err(ElfError::SegmentOverlap),
        "overlapping segments not rejected"
    );
    Ok(())
}
