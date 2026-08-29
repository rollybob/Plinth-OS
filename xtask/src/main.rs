//! Build-and-run tasks for Plinth (cargo-xtask pattern).
//!
//! `cargo xtask run`   -- build kernel + UEFI disk image, boot in QEMU
//! `cargo xtask build` -- build only
//! `cargo xtask run-gdb` -- boot paused with a GDB server on :1234
//! `cargo xtask smoke` -- boot with captured serial output and assert that
//!                        every line in expected_boot_log.txt appears in order,
//!                        AND that nothing the file does not account for appears
//!                        at all (see check_smoke_output)
//! `cargo xtask smoke-smp` -- the same assertion battery as `smoke`, rerun
//!                        once per PLINTH_SMP core count in SMP_TEST_CORE_COUNTS
//!                        (Stage B2.4, design D8) -- the multi-core regression
//!                        net `smoke`'s own -smp 1 transcript check can't be,
//!                        since cross-core interleaving isn't deterministic.
//!                        Note the transcript check is NOT "byte-exact", a
//!                        phrase several commit messages have used for it: 172
//!                        of the 352 captured lines are excused by name in
//!                        expected_boot_log.txt's allowlist
//! `cargo xtask test`  -- build with --features tests, run the in-kernel
//!                        suite, parse [PASS]/[FAIL]/[SUITE] tags
//! `cargo xtask bench` -- build with --features bench, run the BKL contention
//!                        hammer under -smp 1/2/3/4, print the per-core report
//!                        (decides roadmap item B3 -- is splitting the lock
//!                        justified?)

use std::path::{Path, PathBuf};
use std::process::Command;

use ovmf_prebuilt::{Arch, FileType, Prebuilt, Source};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let subcmd = args.get(1).map(String::as_str).unwrap_or("run");

    match subcmd {
        "build"   => { build_all(); }
        "image"   => { let img = build_all(); stage_usb_image(&img); }
        // `run`/`run-gdb` build the kernel with the `interactive` feature, so the
        // shell waits for real keyboard input (drive the cursor yourself) instead
        // of auto-playing its scripted tour. smoke/test/bench stay scripted.
        "run"     => { let img = build_interactive(); run(&img, false); }
        "run-gdb" => { let img = build_interactive(); run(&img, true); }
        "smoke"   => { let img = build_all(); smoke(&img); }
        "smoke-smp" => { let img = build_all(); smoke_smp(&img); }
        // Boot under QEMU's emulated AMD-Vi (PLINTH_IOMMU=amd) and assert the
        // AMD-Vi backend translates block DMA -- the dual-backend lane. Positive-only
        // (D6): QEMU's amd-iommu does not fault an out-of-domain virtio DMA.
        "smoke-amd" => { let img = build_all(); amd_check(&img); }
        "bench"   => { let img = build_bench(); bench(&img); }
        "test"    => { let img = build_test(); run_tests(&img); }
        "console" => { let img = build_force_console(); console_check(&img); }
        "no-i8042" => { let img = build_all(); no_i8042_check(&img); }
        "check"   => { check_clobbers(); }
        other     => {
            eprintln!("unknown subcommand: {other}");
            eprintln!(
                "expected one of: build, image, run, run-gdb, smoke, smoke-smp, smoke-amd, bench, test, console, no-i8042, check"
            );
            std::process::exit(1);
        }
    }
}

/// User binaries built by xtask. Crate directories are named {name}-user.
/// Most are embedded into the kernel (see kernel/build.rs) and the in-kernel
/// ELF loader maps their PT_LOAD segments. `template` is the build-only
/// skeleton from GUIDE.md: compiled every build so it cannot rot, but not
/// embedded or booted.
const USER_CRATES: &[&str] = &[
    "hello", "bump", "list", "crash", "greedy", "lazy", "spawner", "grantee", "spin", "pingpong",
    "share", "rpc", "faultchild", "blk", "asyncblk", "blkwrite", "fsdemo", "diskhello", "evt",
    "evtstream", "unified", "kbd", "mouse", "rwfs", "stealer", "stealwork", "gfx", "gfxtext",
    "gfxsplit", "gfxbound", "gfxrevoke",
    "fbreclaim",
    "fbreclaimchild", "fbrelease", "fbreleasechild", "shell", "shellapp", "caprelease",
    "quietworker",
    "spawnwaitcap",
    "blkreclaim", "blkreclaimchild",
    "blkrelend", "blkrelendmid",
    "blkipclend", "blkrecvchild",
    "bind",
    "template", "bench",
];

/// Build all user crates, then the kernel + disk image.
fn build_all() -> PathBuf {
    for name in USER_CRATES {
        build_user_crate(name);
    }
    // Assemble (and round-trip-verify) the boot archive from the freshly built
    // user ELFs. The image is attached to QEMU as the archive disk once the
    // kernel can read a second virtio-blk device.
    archive_image();
    build()
}

/// Build one user crate (release: small enough to stay within its page
/// budget, and the optimizer behavior is what actually ships). The crate's
/// build.rs links it as a static non-PIE ET_EXEC with page-aligned
/// segments; the kernel embeds the ELF directly and parses it at load time,
/// so there is no flat-binary step.
fn build_user_crate(name: &str) {
    let root = workspace_root();
    let crate_dir = root.join(format!("{name}-user"));

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    // Share the workspace target dir so caching works and kernel/build.rs
    // finds binaries at predictable paths.
    let status = Command::new(&cargo)
        .current_dir(&crate_dir)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .args(["build", "--release"])
        .status()
        .unwrap_or_else(|_| panic!("failed to invoke cargo for {name}-user"));
    assert!(status.success(), "{name}-user build failed");

    let elf_path = root.join(format!("target/x86_64-unknown-none/release/{name}-user"));
    let size = std::fs::metadata(&elf_path)
        .unwrap_or_else(|e| panic!("failed to stat {name}-user ELF: {e}"))
        .len();
    let layout = check_user_elf(name, &elf_path);
    println!("{name}-user: {size} bytes (ELF, {layout})");
}

// --- static ELF layout gate ------------------------------------------------
//
// Every user binary must satisfy three properties that the linker script is
// responsible for and that nothing else checks. Run here, in build_user_crate,
// so it fires on EVERY path that builds user code (build/run/smoke/smoke-smp/
// bench/test) and cannot be forgotten by adding a crate.
//
// Why this exists (2026-08-01). All three properties were violated in this tree
// and only one of the violations was ever noticed:
//
//   1. Page-aligned PT_LOAD. An unplaced .got or an unaligned .bss becomes its
//      own misaligned segment and the kernel's loader refuses the binary at
//      spawn. This one IS caught today -- but only indirectly, and only for
//      crates the boot tour actually spawns: the demo prints "setup of process
//      0 failed" and the K-003 gate rejects it as unaccounted output. A crate
//      that is built but not booted (template) is invisible to that.
//   2. No segment both writable and executable. Never violated, checked because
//      the linker script's whole reason for aligning section groups is per-page
//      W^X, and an assertion of a property is worth more than a comment about it.
//   3. No writable .rodata. This one shipped. rwfs-user folded .got into
//      .rodata, an input section's flags propagated to the output section, and
//      744 bytes of constants carried the write bit for however long. Nothing
//      failed. Nothing could have: no test, no hash and no boot line is a
//      function of a section's write bit. It was found by reading ELF headers
//      by hand while fixing something else.
//
// The gate is deliberately over the BUILT ARTEFACT rather than over the linker
// script's text. A script can be correct and the toolchain still surprise it --
// which is exactly what an orphan section is.

/// Page size the kernel's loader enforces segment alignment against.
const PAGE_SIZE: u64 = 4096;

/// Read a little-endian integer of `N` bytes at `off`, or panic naming the file.
fn elf_num(bytes: &[u8], off: usize, n: usize, what: &str, name: &str) -> u64 {
    let end = off + n;
    assert!(end <= bytes.len(), "{name}-user: ELF truncated reading {what} at {off:#x}");
    let mut v = 0u64;
    for (i, b) in bytes[off..end].iter().enumerate() {
        v |= (*b as u64) << (8 * i);
    }
    v
}

/// Assert the three layout properties above. Returns a short description for the
/// build line so a passing check is visible rather than merely silent.
fn check_user_elf(name: &str, path: &Path) -> String {
    let b = std::fs::read(path)
        .unwrap_or_else(|e| panic!("failed to read {name}-user ELF: {e}"));

    assert!(b.len() >= 64, "{name}-user: too short to be an ELF");
    assert_eq!(&b[0..4], b"\x7fELF", "{name}-user: not an ELF");
    assert_eq!(b[4], 2, "{name}-user: not 64-bit ELF");
    assert_eq!(b[5], 1, "{name}-user: not little-endian ELF");

    let mut problems: Vec<String> = Vec::new();

    // --- program headers: alignment and W^X ---
    let phoff = elf_num(&b, 0x20, 8, "e_phoff", name) as usize;
    let phentsize = elf_num(&b, 0x36, 2, "e_phentsize", name) as usize;
    let phnum = elf_num(&b, 0x38, 2, "e_phnum", name) as usize;
    let mut loads = 0usize;
    for i in 0..phnum {
        let o = phoff + i * phentsize;
        if elf_num(&b, o, 4, "p_type", name) != 1 {
            continue; // PT_LOAD only
        }
        loads += 1;
        let flags = elf_num(&b, o + 4, 4, "p_flags", name);
        let vaddr = elf_num(&b, o + 16, 8, "p_vaddr", name);
        if vaddr % PAGE_SIZE != 0 {
            problems.push(format!(
                "PT_LOAD at {vaddr:#x} is not page-aligned (the loader will reject this \
                 binary with \"elf: segment vaddr not page-aligned\"; an unplaced section \
                 in user.ld is the usual cause)"
            ));
        }
        // PF_X = 1, PF_W = 2.
        if flags & 1 != 0 && flags & 2 != 0 {
            problems.push(format!("PT_LOAD at {vaddr:#x} is both writable and executable"));
        }
    }
    if loads == 0 {
        problems.push("no PT_LOAD segments".to_string());
    }

    // --- section headers: .rodata must not be writable ---
    let shoff = elf_num(&b, 0x28, 8, "e_shoff", name) as usize;
    let shentsize = elf_num(&b, 0x3A, 2, "e_shentsize", name) as usize;
    let shnum = elf_num(&b, 0x3C, 2, "e_shnum", name) as usize;
    let shstrndx = elf_num(&b, 0x3E, 2, "e_shstrndx", name) as usize;
    if shnum > 0 && shoff != 0 {
        let strtab = elf_num(&b, shoff + shstrndx * shentsize + 24, 8, "shstrtab off", name) as usize;
        for i in 0..shnum {
            let o = shoff + i * shentsize;
            let name_off = strtab + elf_num(&b, o, 4, "sh_name", name) as usize;
            let end = b[name_off..].iter().position(|c| *c == 0).unwrap_or(0);
            let sec = std::str::from_utf8(&b[name_off..name_off + end]).unwrap_or("");
            // SHF_WRITE = 0x1, SHF_ALLOC = 0x2.
            let flags = elf_num(&b, o + 8, 8, "sh_flags", name);
            if sec == ".rodata" && flags & 0x2 != 0 && flags & 0x1 != 0 {
                let size = elf_num(&b, o + 32, 8, "sh_size", name);
                problems.push(format!(
                    ".rodata is WRITABLE ({size} bytes) -- a writable input section such as \
                     .got has been folded into it, and its flags propagated to the output \
                     section. Put .got in .data (see user.ld), not .rodata"
                ));
            }
        }
    }

    if !problems.is_empty() {
        eprintln!("elfcheck: {name}-user FAILED:");
        for p in &problems {
            eprintln!("elfcheck:   {p}");
        }
        eprintln!("elfcheck: the layout contract lives in user.ld, shared by every user crate.");
        std::process::exit(1);
    }

    let plural = if loads == 1 { "segment" } else { "segments" };
    format!("{loads} {plural}, layout ok")
}

// Registers every non-noreturn syscall asm! block in libplinth must
// declare: the kernel ABI clobbers the argument registers, syscall itself
// clobbers rcx/r11, and the kernel dispatcher may clobber r8-r10.
const REQUIRED_CLOBBERS: &[&str] = &[
    "rax", "rdi", "rsi", "rdx", "rcx", "r8", "r9", "r10", "r11",
];

/// Lint every asm! block in libplinth/src for the full clobber set.
fn check_clobbers() {
    let root = workspace_root();
    let src_dir = root.join("libplinth/src");
    let mut violations = 0;

    for entry in std::fs::read_dir(&src_dir).expect("failed to read libplinth/src") {
        let path = entry.expect("dir entry error").path();
        if path.extension().is_some_and(|e| e == "rs") {
            violations += lint_file(&path);
        }
    }

    if violations > 0 {
        eprintln!("clobber lint: {violations} violation(s) -- see above");
        std::process::exit(1);
    }
    println!("clobber lint: ok");
}

fn lint_file(path: &Path) -> usize {
    let src = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let mut violations = 0;
    let mut search_from = 0;

    while let Some(rel) = src[search_from..].find("asm!(") {
        let block_start = search_from + rel;
        let line_no = src[..block_start].bytes().filter(|&b| b == b'\n').count() + 1;

        // Extract the block by matching the opening paren.
        let open = block_start + 4;
        let mut depth = 1usize;
        let mut block_end = open + 1;
        for (i, ch) in src[open + 1..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = open + 1 + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        let block = &src[block_start..block_end];

        // noreturn blocks are exempt: the CPU never returns to Rust, so
        // there is no live register state to protect.
        if !block.contains("noreturn") {
            for reg in REQUIRED_CLOBBERS {
                if !block.contains(&format!("\"{reg}\"")) {
                    eprintln!(
                        "{}:{line_no}: asm! block missing clobber for \"{reg}\"",
                        path.display()
                    );
                    violations += 1;
                }
            }
        }
        search_from = block_end;
    }
    violations
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is xtask/; the workspace root is one level up.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.parent().unwrap().to_path_buf()
}

/// Path to the deterministic Stage-1 block image, created on demand. Each
/// 512-byte sector carries a distinguishable ramp: byte j of sector s is
/// (s + j) & 0xFF. So a read of sector s is verifiable against a trivial
/// formula AND distinguishable from every other sector (which the BlockRange
/// demo relies on -- a whole-disk ramp of i%256 would make every sector
/// identical, since 512 is a multiple of 256). Sector 0 is still j & 0xFF, so
/// the milestone-2 self-test (which checks sector 0 against i%256) still holds.
/// 1 MiB; content-stable, so QEMU sees identical bytes every run.
fn block_image() -> PathBuf {
    let out_dir = workspace_root().join("target/disk-images");
    std::fs::create_dir_all(&out_dir).unwrap();
    let path = out_dir.join("blk.img");
    let data: Vec<u8> = (0..1024u64 * 1024)
        .map(|i| ((i / 512 + i % 512) & 0xFF) as u8)
        .collect();
    std::fs::write(&path, &data).expect("failed to write Stage-1 block image");
    path
}

/// The directly-bound device's backing image (direct-binding slice 2). Same
/// deterministic ramp as `block_image` (sector 0 byte j is j & 0xFF), so the
/// bound selftest verifies it exactly as the ramp-disk selftest does. A separate
/// file from `blk.img` so QEMU is never asked to open one image on two drives.
/// 1 MiB; content-stable.
fn bind_image() -> PathBuf {
    let out_dir = workspace_root().join("target/disk-images");
    std::fs::create_dir_all(&out_dir).unwrap();
    let path = out_dir.join("bind.img");
    let data: Vec<u8> = (0..1024u64 * 1024)
        .map(|i| ((i / 512 + i % 512) & 0xFF) as u8)
        .collect();
    std::fs::write(&path, &data).expect("failed to write bind image");
    path
}

// Mirror of libfs::archive (the CANONICAL format definition; see that module).
// The on-disk layout is intentionally trivial so this writer stays a faithful
// mirror of the bare-target reader; archive_image round-trips the result
// through libfs to catch any drift.
const ARC_SECTOR: usize = 512;
const ARC_MAGIC: &[u8; 8] = b"PLNTHAR1";
const ARC_ENTRY_SIZE: usize = 40;
const ARC_NAME_LEN: usize = 32;

/// Programs assembled into the read-only boot archive, looked up by these names
/// through the FS libOS. Each maps to the `{name}-user` release ELF. These are
/// loaded from disk (the point of the milestone), as opposed to the binaries
/// embedded in the kernel via kernel/build.rs.
const ARCHIVE_PROGRAMS: &[&str] = &["diskhello", "hello"];

/// Assemble the read-only boot archive from the built user ELFs: a superblock,
/// a packed directory of `(name, first_sector, byte_len)`, then each program's
/// ELF blob on a sector boundary. The result is parsed back with libfs (the
/// canonical reader) before it is written, so the host writer and the
/// bare-target reader can never disagree about the format.
fn archive_image() -> PathBuf {
    let root = workspace_root();

    // Gather (name, ELF bytes) for each program. The user crates are built by
    // build_all before this runs (every build path that needs the archive
    // builds the user crates first).
    let mut progs: Vec<(&str, Vec<u8>)> = Vec::new();
    for name in ARCHIVE_PROGRAMS {
        assert!(name.len() <= ARC_NAME_LEN, "archive program name too long: {name}");
        let elf_path = root.join(format!("target/x86_64-unknown-none/release/{name}-user"));
        let bytes = std::fs::read(&elf_path)
            .unwrap_or_else(|e| panic!("failed to read {name}-user ELF for archive: {e}"));
        progs.push((name, bytes));
    }

    // Layout: superblock (1 sector) + directory + sector-aligned blobs.
    let dir_bytes = progs.len() * ARC_ENTRY_SIZE;
    let dir_sectors = dir_bytes.div_ceil(ARC_SECTOR);
    let mut blob_cursor = 1 + dir_sectors; // first blob's sector

    // Build the directory and the blob region together, tracking each blob's
    // assigned sector as the cursor advances.
    let mut directory = vec![0u8; dir_sectors * ARC_SECTOR];
    let mut blobs: Vec<u8> = Vec::new();
    for (i, (name, bytes)) in progs.iter().enumerate() {
        let rec = &mut directory[i * ARC_ENTRY_SIZE..(i + 1) * ARC_ENTRY_SIZE];
        let nb = name.as_bytes();
        rec[0..nb.len()].copy_from_slice(nb); // name, NUL-padded by the zeroed buffer
        rec[32..36].copy_from_slice(&(blob_cursor as u32).to_le_bytes()); // first_sector
        rec[36..40].copy_from_slice(&(bytes.len() as u32).to_le_bytes()); // byte_len

        blobs.extend_from_slice(bytes);
        let pad = bytes.len().next_multiple_of(ARC_SECTOR) - bytes.len();
        blobs.extend(std::iter::repeat(0u8).take(pad));
        blob_cursor += bytes.len().div_ceil(ARC_SECTOR);

        println!(
            "archive: {name} at sector {} ({} bytes)",
            blob_cursor - bytes.len().div_ceil(ARC_SECTOR),
            bytes.len()
        );
    }

    let total_sectors = blob_cursor as u32;

    // Superblock sector.
    let mut superblock = vec![0u8; ARC_SECTOR];
    superblock[0..8].copy_from_slice(ARC_MAGIC);
    superblock[8..12].copy_from_slice(&(progs.len() as u32).to_le_bytes());
    superblock[12..16].copy_from_slice(&(dir_sectors as u32).to_le_bytes());
    superblock[16..20].copy_from_slice(&total_sectors.to_le_bytes());

    let mut image = superblock;
    image.extend_from_slice(&directory);
    image.extend_from_slice(&blobs);
    assert_eq!(image.len(), total_sectors as usize * ARC_SECTOR, "archive size mismatch");

    // Structural self-check: the writer and the canonical reader (libfs) cannot
    // share a crate (host vs. bare target), so verify here that what was just
    // laid out is internally consistent -- every directory entry's blob lands
    // on its recorded sector with its recorded length. The authoritative
    // writer-vs-reader cross-check is the kernel selftest in the next
    // milestone: it reads this image off the virtio device and parses it with
    // libfs, so any format drift surfaces there against real device bytes.
    for (i, (name, bytes)) in progs.iter().enumerate() {
        let rec = &directory[i * ARC_ENTRY_SIZE..(i + 1) * ARC_ENTRY_SIZE];
        let first_sector = u32::from_le_bytes(rec[32..36].try_into().unwrap()) as usize;
        let byte_len = u32::from_le_bytes(rec[36..40].try_into().unwrap()) as usize;
        assert_eq!(byte_len, bytes.len(), "archive {name}: byte_len mismatch");
        let off = first_sector * ARC_SECTOR;
        assert_eq!(&image[off..off + byte_len], bytes.as_slice(), "archive {name}: blob misplaced");
    }

    let out_dir = root.join("target/disk-images");
    std::fs::create_dir_all(&out_dir).unwrap();
    let path = out_dir.join("archive.img");
    std::fs::write(&path, &image).expect("failed to write boot archive image");
    println!("archive image: {} ({} sectors)", path.display(), total_sectors);
    path
}

/// Build the full userspace + the kernel with the `interactive` feature, into a
/// separate image (so it never clobbers the smoke/run image). Used by `run`:
/// the shell then waits for real keyboard input instead of its scripted tour.
fn build_interactive() -> PathBuf {
    for name in USER_CRATES {
        build_user_crate(name);
    }
    archive_image();

    let root = workspace_root();
    let kernel_dir = root.join("kernel");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(&cargo)
        .current_dir(&kernel_dir)
        .args(["build", "--features", "interactive"])
        .status()
        .expect("failed to invoke cargo for interactive kernel build");
    assert!(status.success(), "interactive kernel build failed");

    let kernel_bin = root.join("target/x86_64-unknown-none/debug/kernel");
    let out_dir = root.join("target/disk-images");
    std::fs::create_dir_all(&out_dir).unwrap();

    let uefi_path = out_dir.join("uefi-interactive.img");
    bootloader::UefiBoot::new(&kernel_bin)
        .create_disk_image(&uefi_path)
        .unwrap();

    println!("interactive disk image: {}", uefi_path.display());
    uefi_path
}

/// Copy the built image to a clearly named artifact for writing to a USB stick,
/// and print what to do with it.
///
/// The image `build` already produces is UEFI-bootable as it stands -- the
/// bootloader crate writes a FAT32 filesystem with the kernel at
/// `\EFI\BOOT\BOOTX64.EFI`, which is the removable-media path firmware looks for
/// with no NVRAM boot entry involved. So this adds no new image format; it gives
/// the artifact a name that says what it is for, and states the things that are
/// easy to get wrong.
///
/// **This deliberately does not write to any device.** Writing a raw image to
/// the wrong disk destroys it irrecoverably, and picking the right one needs a
/// human looking at the machine. The command prints what to run and stops.
///
/// The SCRIPTED build is staged, not the `interactive` one, and on a machine
/// without PS/2 that is not a preference. The interactive shell waits for real
/// arrow keys; Plinth has no USB HID driver, so on a target whose only input is
/// USB that key can never arrive and the shell waits forever. The scripted build
/// drives itself from the kernel's synthetic injection and then exits, which on
/// hardware means ACPI soft-off -- so the machine powering itself down is the
/// signal that it reached the end.
fn stage_usb_image(built: &Path) {
    let out = built.with_file_name("plinth-usb.img");
    std::fs::copy(built, &out).expect("failed to stage USB image");
    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);

    println!();
    println!("USB image: {}", out.display());
    println!("  size: {} bytes ({:.1} MiB)", bytes, bytes as f64 / (1024.0 * 1024.0));
    println!("  contents: FAT32, UEFI removable-media path \\EFI\\BOOT\\BOOTX64.EFI");
    println!("  build: scripted (self-driving). The `interactive` build needs a");
    println!("         keyboard Plinth can read, which means PS/2 -- there is no");
    println!("         USB HID driver, so do not put that build on a stick unless");
    println!("         the target has a PS/2 port.");
    println!();
    println!("To write it, identify the device FIRST and check it twice:");
    println!("  Windows : use Rufus in DD mode, or `wmic diskdrive list brief`");
    println!("            to find the disk number, then a raw writer of choice.");
    println!("  Linux   : lsblk, then");
    println!("            sudo dd if={} of=/dev/sdX bs=4M status=progress conv=fsync", out.display());
    println!();
    println!("  Writing to the wrong device destroys it. This command will not do");
    println!("  it for you on purpose.");
    println!();
    println!("The target must boot UEFI (not CSM/legacy), and Secure Boot must be");
    println!("off -- the kernel is unsigned.");
    println!();
    println!("Not yet booted on physical hardware. There is no claim here that it");
    println!("works on any given machine; that is what the first run finds out.");
}

/// Build the kernel and produce a UEFI-bootable disk image.
fn build() -> PathBuf {
    let root = workspace_root();
    let kernel_dir = root.join("kernel");

    // Run cargo inside kernel/ so it picks up kernel/.cargo/config.toml,
    // which sets build-std and the x86_64-unknown-none target. This is a
    // separate cargo invocation from the workspace build of xtask itself.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(&cargo)
        .current_dir(&kernel_dir)
        .args(["build"])
        .status()
        .expect("failed to invoke cargo for kernel build");
    assert!(status.success(), "kernel build failed");

    // bootloader::UefiBoot writes a FAT32 image with the kernel exposed as
    // \EFI\BOOT\BOOTX64.EFI; OVMF finds and loads it automatically.
    let kernel_bin = root.join("target/x86_64-unknown-none/debug/kernel");
    let out_dir = root.join("target/disk-images");
    std::fs::create_dir_all(&out_dir).unwrap();

    let uefi_path = out_dir.join("uefi.img");
    bootloader::UefiBoot::new(&kernel_bin)
        .create_disk_image(&uefi_path)
        .unwrap();

    println!("disk image: {}", uefi_path.display());
    uefi_path
}

/// Compose the QEMU command line shared by run and smoke. `exit_on_debug` adds
/// the isa-debug-exit device, which lets the kernel terminate QEMU by writing to
/// port 0xF4. Every path passes true as of 2026-07-25: the capture paths need it
/// so the kernel self-terminates and the harness can collect serial, and `run`
/// needs it so quitting the shell actually closes the window. The parameter is
/// kept rather than inlined because omitting the device is exactly how you get a
/// window that survives boot, if inspecting a final frame is ever worth more than
/// a clean exit (it was, briefly -- see `run`).
fn build_qemu_cmd(uefi_path: &Path, gdb: bool, exit_on_debug: bool, machine_extra: &str) -> Command {
    let root = workspace_root();

    // OVMF provides separate code (read-only) and vars (read-write)
    // firmware volumes, mounted as pflash devices -- the standard
    // UEFI-on-QEMU configuration.
    let ovmf_dir = root.join("target/ovmf");
    let prebuilt = Prebuilt::fetch(Source::LATEST, &ovmf_dir)
        .expect("failed to fetch OVMF prebuilt firmware");
    let code = prebuilt.get_file(Arch::X64, FileType::Code);
    let vars_template = prebuilt.get_file(Arch::X64, FileType::Vars);

    // OVMF_VARS is mutable; copy the cached template to an active location
    // so the template stays clean across runs.
    let vars = root.join("target/ovmf/OVMF_VARS-active.fd");
    if !vars.exists() {
        std::fs::copy(&vars_template, &vars)
            .expect("failed to copy OVMF_VARS template to active location");
    }

    // q35 plus any caller-supplied properties (e.g. ",i8042=off" to emulate a
    // machine with no PS/2 controller). Built before the args array so it
    // outlives the borrow.
    let machine = format!("q35{machine_extra}");
    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.args([
        // q35: modern chipset, PCIe-native, publishes MCFG in ACPI.
        "-machine", &machine,
        "-drive", &format!("if=pflash,format=raw,readonly=on,file={}", code.display()),
        "-drive", &format!("if=pflash,format=raw,file={}", vars.display()),
        "-drive", &format!("format=raw,file={}", uefi_path.display()),
        "-serial", "stdio",
        "-no-reboot",
        "-m", "256M",
        "-cpu", "qemu64",
        // Pin the display adapter so OVMF's GOP hands the bootloader a linear
        // framebuffer with stable geometry (D6, Stage 1).
        // Smoke/test add `-display none` (headless) in run_capture; the device
        // stays present either way, so the framebuffer exists with or without a
        // host window -- and `run` and `smoke` see identical pixels, so the
        // framebuffer hash matches across both. Pinned alongside the OVMF 0.2.8
        // firmware: re-check boot if either changes.
        "-vga", "std",
    ]);

    // Expose a VT-d IOMMU so the guest can discover a DMAR table and enforce DMA
    // translation (protected-DMA milestone). Added before the virtio-blk devices
    // below so the vIOMMU is realized ahead of the PCI devices it governs.
    // Interrupt remapping stays off (its default), so no split irqchip is needed.
    // caching-mode=on is REQUIRED for QEMU to actually enforce per-DMA
    // translation on emulated devices (without it an out-of-domain access is not
    // faulted -- the positive "DMA still works" would be vacuous); it also makes
    // IOTLB invalidation mandatory after a mapping change, which the kernel does
    // (Design/iommu.md D4).
    // The vIOMMU device is selectable so the AMD-Vi backend can run against QEMU's
    // emulated AMD-Vi (PLINTH_IOMMU=amd) as well as the Intel VT-d default. AMD-Vi
    // is itself a PCI device that auto-takes the lowest free slot (3), so the
    // virtio-blk devices below shift to 6/7/8 in amd mode (order preserved, so
    // device indices stay 0/1/2). dma-remap=on is the AMD analogue of intel-iommu's
    // caching-mode=on: without it QEMU presents the unit but does not enforce
    // translation (a vacuous proof). Default is unchanged, so smoke/CI stay green.
    match std::env::var("PLINTH_IOMMU").as_deref() {
        Ok("amd") => {
            cmd.args(["-device", "amd-iommu,dma-remap=on"]);
        }
        _ => {
            cmd.args(["-device", "intel-iommu,caching-mode=on"]);
        }
    }

    if exit_on_debug {
        // isa-debug-exit: the kernel writes N to port 0xF4 and QEMU exits with
        // status (N << 1) | 1. Kernel success (N=0) -> exit code 1.
        cmd.args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]);
    }

    // Plinth is deliberately uniprocessor by default: deterministic serial
    // output is a feature, not a limitation, and `-smp 1` is the regression
    // net every other check still runs against (D8). PLINTH_SMP=N opts into
    // N cores for testing AP bring-up (Stage B1)
    // and, later, real cross-core scheduling (Stage B2) -- off by default, and
    // the kernel does not depend on it for anything `smoke`/`test` assert.
    let smp = std::env::var("PLINTH_SMP").unwrap_or_else(|_| "1".to_string());
    cmd.args(["-smp", &smp]);

    // virtio-blk base slot: 3 normally, or 6 under AMD-Vi (whose AMDVI-PCI device
    // takes slot 3). Kept contiguous so PCI-slot-order enumeration still yields
    // device indices 0/1/2 for blk0/blk1/blk2.
    let blk_base = if std::env::var("PLINTH_IOMMU").as_deref() == Ok("amd") { 6 } else { 3 };
    let blk0_addr = format!("virtio-blk-pci,drive=blk0,addr=0x{:x},disable-legacy=on,iommu_platform=on", blk_base);
    let blk1_addr = format!("virtio-blk-pci,drive=blk1,addr=0x{:x},disable-legacy=on,iommu_platform=on", blk_base + 1);
    let blk2_addr = format!("virtio-blk-pci,drive=blk2,addr=0x{:x},disable-legacy=on,iommu_platform=on", blk_base + 2);

    // Stage 1 storage: a deterministic raw disk behind a modern virtio-blk-pci
    // device, pinned to slot 3 so discovery output is stable across runs.
    // disable-legacy=on forces the modern (MMIO-capability) device the driver
    // targets, rather than a transitional device with a legacy PIO BAR.
    let blk = block_image();
    cmd.args([
        "-drive",
        &format!("if=none,format=raw,file={},id=blk0", blk.display()),
        "-device",
        // iommu_platform=on routes this device's DMA through the vIOMMU (it then
        // offers VIRTIO_F_ACCESS_PLATFORM); without it QEMU's virtio bypasses the
        // IOMMU and the domain enforcement would be vacuous.
        &blk0_addr,
    ]);

    // Storage device 1: the read-only boot archive. Pinned to slot 4 (just past
    // the ramp disk's slot 3) so the kernel's PCI-slot-order enumeration always
    // gives it device index 1, and so discovery output is stable across runs.
    let archive = archive_image();
    cmd.args([
        "-drive",
        &format!("if=none,format=raw,file={},id=blk1", archive.display()),
        "-device",
        &blk1_addr,
    ]);

    // Storage device 2: the directly-bound device (direct-binding slice 2),
    // pinned to slot 5. The kernel claims it with its OWN non-identity IOMMU
    // domain (its virtqueue at opaque IOVAs) rather than the shared kernel-bridged
    // domain the other two use. iommu_platform=on so its DMA is governed by the
    // vIOMMU, like the others -- direct binding relies on that confinement.
    let bind_disk = bind_image();
    cmd.args([
        "-drive",
        &format!("if=none,format=raw,file={},id=blk2", bind_disk.display()),
        "-device",
        &blk2_addr,
    ]);

    // Log CPU resets and exceptions for post-mortem debugging.
    cmd.args(["-D", "qemu_debug.log", "-d", "cpu_reset,int"]);

    // Opt-in deterministic timing: PLINTH_ICOUNT=N ties the guest clock to
    // retired instructions (shift=N), so timer interrupts fire at the same
    // instruction every run -- reproducible preemption and reverse-debugging.
    // Off by default; the kernel never depends on it (it must be correct
    // under real, nondeterministic timing). PLINTH_ICOUNT set but empty -> 5.
    if let Ok(v) = std::env::var("PLINTH_ICOUNT") {
        let shift = if v.trim().is_empty() { "5".to_string() } else { v };
        cmd.args(["-icount", &format!("shift={shift}")]);
    }

    if gdb {
        // -s: GDB server on :1234; -S: pause until GDB sends 'continue'.
        cmd.args(["-s", "-S"]);
        eprintln!("QEMU paused -- attach GDB with:");
        eprintln!("  target remote :1234");
    }

    cmd
}

/// Default QEMU timeout; override with PLINTH_QEMU_TIMEOUT (seconds) on
/// slow machines or loaded CI runners, where TCG boot can take longer.
const TIMEOUT_SECS: u64 = 60;

fn qemu_timeout() -> u64 {
    std::env::var("PLINTH_QEMU_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(TIMEOUT_SECS)
}

/// Wait for QEMU with a hard timeout. Returns the exit code, or i32::MIN
/// if the process was killed because it timed out.
fn wait_qemu(mut child: std::process::Child) -> i32 {
    let timeout = qemu_timeout();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    loop {
        if let Some(s) = child.try_wait().expect("failed to wait on qemu") {
            return s.code().unwrap_or(1);
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("QEMU timed out after {timeout}s -- killing");
            let _ = child.kill();
            let _ = child.wait();
            return i32::MIN;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Boot interactively: a real window, real keyboard, no captured serial.
///
/// The isa-debug-exit device IS attached here (2026-07-25, reversing Task 6 of
/// 06-27, which omitted it so the window would survive boot for framebuffer
/// inspection). On the `interactive` kernel build the shell waits for real
/// keypresses, so the kernel does not reach `qemu_exit` until you press Q --
/// meaning the device no longer cuts the session short, it just lets Q actually
/// close the window instead of parking the kernel in a `hlt` loop that is
/// indistinguishable from a hang. Inspecting a final frame is now a thing to add
/// back deliberately if a log ever proves insufficient, rather than a permanent
/// cost paid on every run.
///
/// No timeout: an interactive session lasts as long as you want it to.
fn run(uefi_path: &Path, gdb: bool) {
    let mut child = build_qemu_cmd(uefi_path, gdb, true, "")
        .spawn()
        .expect("failed to launch qemu-system-x86_64");
    eprintln!("QEMU open -- press Q in the shell to exit, or close the window.");
    // `wait`, not `wait_with_output`: serial is on inherited stdio (`-serial
    // stdio`) so the log streams straight to this terminal; capturing it here
    // would swallow it.
    let status = child.wait().expect("failed to wait on qemu");
    // isa-debug-exit reports (N << 1) | 1, so kernel Success -> 1, Failure -> 3.
    // Closing the window by hand terminates QEMU normally -> 0.
    match status.code() {
        Some(0) => eprintln!("QEMU window closed."),
        Some(1) => eprintln!("kernel exited cleanly."),
        Some(3) => eprintln!("kernel exited reporting FAILURE (panic, or a failed suite)."),
        Some(c) => eprintln!("QEMU exited with unexpected code: {c}"),
        None => eprintln!("QEMU terminated by signal."),
    }
}

/// Boot with captured stdout and return the serial output. A reader thread
/// drains the pipe so a full buffer never stalls QEMU.
fn run_capture(uefi_path: &Path) -> String {
    run_capture_machine(uefi_path, "")
}

/// `run_capture` with extra `-machine` properties -- e.g. ",i8042=off" to boot
/// a machine with no PS/2 controller and exercise the absent-hardware paths.
fn run_capture_machine(uefi_path: &Path, machine_extra: &str) -> String {
    use std::io::Read;
    let mut cmd = build_qemu_cmd(uefi_path, false, true, machine_extra);
    // Headless: all output we care about arrives over serial. Without
    // this, QEMU tries to open its default (GTK) display and dies on
    // CI runners that have no display server at all.
    cmd.args(["-display", "none"]);
    cmd.stdout(std::process::Stdio::piped());

    let mut child = cmd.spawn().expect("failed to launch qemu-system-x86_64");
    let mut stdout = child.stdout.take().expect("no stdout handle");

    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        stdout.read_to_string(&mut buf).ok();
        buf
    });

    let code = wait_qemu(child);
    let output = reader.join().expect("reader thread panicked");
    if code == i32::MIN || (code != 0 && code != 1) {
        if code != i32::MIN {
            eprintln!("QEMU exited with unexpected code: {code}");
        }
        eprintln!("--- captured output ---");
        eprintln!("{output}");
        eprintln!("--- end output ---");
        std::process::exit(if code == i32::MIN { 2 } else { code });
    }
    output
}

/// Assert that the captured serial output matches expected_boot_log.txt, in
/// both directions:
///
/// 1. **Every expectation appears, in order.** Non-blank, non-comment lines in
///    the expectations file are matched as substrings against `actual`,
///    advancing a cursor, so partial-line merges still count and expected
///    strings need only be specific enough to avoid false positives.
/// 2. **Nothing else appears.** Any actual line that satisfied no expectation
///    and matches no `#!allow` pattern fails the run.
///
/// Direction 2 is K-003, and it is the whole point of this function being
/// longer than it looks like it should be. Without it the gate was a plain
/// subsequence match: unmatched output was skipped over silently, so a whole
/// new demo could print eight lines of new kernel output and smoke stayed
/// green. That is what happened while adding `fbreclaim`. New kernel output is
/// now unverified-by-default and has to be either asserted or explicitly
/// excused.
///
/// `#!allow <substring>` lines in expected_boot_log.txt declare output that is
/// real but not deterministic enough to assert -- addresses, frame counts,
/// geometry that shifts with the QEMU version. They are ordinary `#` comments
/// to the expectation parser, so the allowlist lives in the same file as the
/// thing it excuses, and every entry is a written-down admission of what this
/// gate does not cover.
fn check_smoke_output(actual: &str, expected_path: &Path) {
    let src = std::fs::read_to_string(expected_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", expected_path.display(), e));

    let mut expected: Vec<&str> = Vec::new();
    let mut allowed: Vec<&str> = Vec::new();
    for line in src.lines().map(str::trim) {
        if let Some(pattern) = line.strip_prefix("#!allow ") {
            let pattern = pattern.trim();
            assert!(
                !pattern.is_empty(),
                "empty #!allow pattern in {} -- an empty substring matches every line, \
                 which would disable the unexpected-output check entirely",
                expected_path.display()
            );
            allowed.push(pattern);
        } else if !line.is_empty() && !line.starts_with('#') {
            expected.push(line);
        }
    }

    let actual_lines: Vec<&str> = actual.lines().collect();
    let mut satisfied = vec![false; actual_lines.len()];
    let mut cursor = 0;
    let mut failed = false;

    for want in &expected {
        match actual_lines[cursor..].iter().position(|l| l.contains(want)) {
            Some(pos) => {
                satisfied[cursor + pos] = true;
                cursor += pos + 1;
            }
            None => {
                eprintln!("smoke: missing: {want:?}");
                failed = true;
            }
        }
    }

    // Expectations are checked first and win ties, so an `#!allow` pattern can
    // never swallow a line an expectation was counting on.
    let mut unexpected: Vec<(usize, &str)> = Vec::new();
    let mut allowed_count = 0usize;
    for (i, line) in actual_lines.iter().enumerate() {
        if satisfied[i] || line.trim().is_empty() {
            continue;
        }
        if allowed.iter().any(|pattern| line.contains(*pattern)) {
            allowed_count += 1;
            continue;
        }
        unexpected.push((i + 1, *line));
    }

    if !unexpected.is_empty() {
        eprintln!(
            "smoke: {} unexpected line(s) -- output that no expectation covers \
             and no #!allow excuses:",
            unexpected.len()
        );
        for (lineno, line) in &unexpected {
            eprintln!("smoke:   line {lineno}: {line:?}");
        }
        eprintln!(
            "smoke: assert these in {} if they are deterministic, or add a \
             `#!allow <substring>` line there -- with a comment saying why they \
             cannot be asserted -- if they are not.",
            expected_path.display()
        );
        failed = true;
    }

    if failed {
        eprintln!("smoke: FAIL");
        eprintln!("--- captured output ---");
        for line in &actual_lines {
            eprintln!("{line}");
        }
        eprintln!("--- end output ---");
        std::process::exit(1);
    }
    // Deliberately not "N lines verified", which counted the expectations file
    // and told you nothing about the run. These three numbers describe the boot
    // that actually happened, and the middle one is the honest measure of how
    // much of it this gate does not check.
    println!(
        "smoke: ok ({} expectations matched, {allowed_count} lines allowed, {} lines captured)",
        expected.len(),
        actual_lines.len()
    );
}

/// Number of scheduler-demo processes and lines each prints. Must match
/// main.rs (3 instances of spin-user) and spin-user's ITER.
const SCHED_PROCESSES: u64 = 3;
const SCHED_ITERS: u64 = 6;

/// Rounds the IPC ping-pong demo runs. Must match pingpong-user's ROUNDS.
const IPC_ROUNDS: u64 = 4;

/// The value share-user's producer writes into the frame it hands off; the
/// consumer must read exactly this back (proving the capability transfer moved
/// a usable frame, with the producer's data intact). Must match PATTERN.
const SHARE_PATTERN: u64 = 12345;

/// RPC demo parameters. Must match rpc-user's N and RESP_OFFSET.
const RPC_CALLS: u64 = 3;
const RPC_OFFSET: u64 = 1000;

/// The result the spawned worker sends back; the parent must report it. Must
/// match grantee-user's RESULT.
const SPAWN_RESULT: u64 = 42;

/// Workers the steal demo spawns; must match stealer-user's WORKERS. Each
/// prints one `stealwork[id] done` line and the parent joins all of them.
const STEAL_WORKERS: u64 = 3;

/// Spawn round-trips the cap_release demo runs; must match caprelease-user's
/// ROUNDS. It has to exceed the eight-endpoint pool for the test to mean
/// anything -- a build that leaks the spent wait handle dies partway through.
/// (It clears the 16-slot capability table too, but the pool is what binds;
/// this comment said otherwise until 2026-08-10, see A-13.)
///
/// The demo now derives its own count from `libplinth::REUSE_ROUNDS` rather
/// than a literal, and this crate is host-side so it cannot read that. It stays
/// a mirror on purpose: raising a kernel limit fails the kernel build first
/// (the const assert beside `MAX_ENDPOINTS`), and following that instruction
/// changes the demo's derived count, which turns this check red until the
/// number here is raised too. Red in a known place beats a literal that drifts
/// quietly.
const CAPRELEASE_ROUNDS: u64 = 20;

/// Assert each scheduled process printed its own lines in program order.
/// Under preemption the processes' lines interleave arbitrarily, but a single
/// process's output is always in program order -- so for each id the counters
/// it printed must be exactly 0, 1, ..., iters-1. This is the interleaving-
/// robust replacement for an exact-trace assertion (Design section 2): it does
/// not care HOW the lines interleave, only that each process's are in order.
fn check_per_process_order(actual: &str, num_processes: u64, iters: u64) {
    let lines: Vec<&str> = actual.lines().map(str::trim).collect();
    let mut failed = false;
    for id in 0..num_processes {
        let prefix = format!("spin[{id}] ");
        let seq: Vec<u64> = lines
            .iter()
            .filter_map(|l| l.strip_prefix(&prefix))
            .filter_map(|rest| rest.trim().parse::<u64>().ok())
            .collect();
        let want: Vec<u64> = (0..iters).collect();
        if seq != want {
            eprintln!("smoke: process {id} out of order: got {seq:?}, want {want:?}");
            failed = true;
        }
    }
    if failed {
        eprintln!("smoke: FAIL (per-process ordering)");
        eprintln!("--- captured output ---");
        for line in &lines {
            eprintln!("{line}");
        }
        eprintln!("--- end output ---");
        std::process::exit(1);
    }
    println!("smoke: per-process ordering ok ({num_processes} processes x {iters} lines)");
}

/// The free-frame count printed before and after a demo named `name` must be
/// identical: every process is fully reclaimed, so the system returns to
/// baseline at quiescence (the no-leak invariant, Design section 2).
fn check_frames_baseline(actual: &str, name: &str) {
    let before = find_frame_count(actual, &format!("frames free before {name}"));
    let after = find_frame_count(actual, &format!("frames free after {name}"));
    match (before, after) {
        (Some(b), Some(a)) if a == b => {
            println!("smoke: {name} frame baseline ok ({b} free, no leak)");
        }
        (Some(b), Some(a)) => {
            eprintln!("smoke: FAIL frames leaked across {name}: before={b}, after={a}");
            std::process::exit(1);
        }
        _ => {
            eprintln!("smoke: FAIL could not find {name} frame-baseline lines");
            std::process::exit(1);
        }
    }
}

/// The free-endpoint count printed before and after an IPC demo must match:
/// every endpoint the demo created (granted to its processes, or made by
/// sys_spawn) is reclaimed once the last capability referencing it is dropped
/// at teardown (Stage B endpoint freeing). Mirrors the frame baseline; this is
/// what proves the endpoint-table leak is actually fixed.
fn check_endpoints_baseline(actual: &str, name: &str) {
    let before = find_frame_count(actual, &format!("endpoints free before {name}"));
    let after = find_frame_count(actual, &format!("endpoints free after {name}"));
    match (before, after) {
        (Some(b), Some(a)) if a == b => {
            println!("smoke: {name} endpoint baseline ok ({b} free, no leak)");
        }
        (Some(b), Some(a)) => {
            eprintln!("smoke: FAIL endpoints leaked across {name}: before={b}, after={a}");
            std::process::exit(1);
        }
        _ => {
            eprintln!("smoke: FAIL could not find {name} endpoint-baseline lines");
            std::process::exit(1);
        }
    }
}

/// Verify the IPC ping-pong rendezvous: for each role the round counter must
/// run 0..rounds in program order (interleaving-robust), and the exchanged
/// value must be right -- the ponger replies `i + 100`, so the pinger sees
/// `i + 100` and the ponger sees `i`. Checking the values proves the
/// rendezvous actually moved the right data, not just that lines appeared.
fn check_ipc_order(actual: &str, rounds: u64) {
    let lines: Vec<&str> = actual.lines().map(str::trim).collect();
    let mut failed = false;

    for &(tag, offset) in &[("ping", 100u64), ("pong", 0u64)] {
        let prefix = format!("{tag} ");
        let mut round = 0u64;
        for line in &lines {
            let Some(rest) = line.strip_prefix(&prefix) else {
                continue;
            };
            // rest is "<i> got <v>"
            let Some((i_str, v_str)) = rest.split_once(" got ") else {
                continue;
            };
            let (Ok(i), Ok(v)) = (i_str.trim().parse::<u64>(), v_str.trim().parse::<u64>()) else {
                continue;
            };
            if i != round {
                eprintln!("smoke: {tag} out of order: saw round {i}, expected {round}");
                failed = true;
            }
            if v != round + offset {
                eprintln!("smoke: {tag} round {round}: got {v}, expected {}", round + offset);
                failed = true;
            }
            round += 1;
        }
        if round != rounds {
            eprintln!("smoke: {tag}: saw {round} rounds, expected {rounds}");
            failed = true;
        }
    }

    if failed {
        eprintln!("smoke: FAIL (ipc rendezvous)");
        eprintln!("--- captured output ---");
        for line in &lines {
            eprintln!("{line}");
        }
        eprintln!("--- end output ---");
        std::process::exit(1);
    }
    println!("smoke: ipc rendezvous ok (ping/pong x {rounds} rounds, values verified)");
}

/// Pull the integer out of the first line containing `marker`.
/// Anchored at the END of the line, not `contains`. Every bracket line this
/// reads is written as `plinth: <n> <kind> free <before|after> <name>`, so the
/// demo name is the last thing on it -- which means a `contains` match lets one
/// demo's marker match a LONGER demo's line. That is not hypothetical: `blk` is
/// a prefix of `blkwrite` and `evt` of `evtstream`, and both pairs have been in
/// this tree for weeks reading the right numbers purely because the shorter demo
/// happens to boot first and `find` stops at the first hit. Reordering the boot
/// sequence would have silently pointed a baseline at another demo's counts --
/// still green, still "passing", checking the wrong thing. Anchoring costs
/// nothing and removes the whole class.
fn find_frame_count(actual: &str, marker: &str) -> Option<u64> {
    actual
        .lines()
        .find(|l| l.trim_end().ends_with(marker))
        .and_then(|l| l.split_whitespace().find_map(|t| t.parse::<u64>().ok()))
}

/// Verify the capability-transfer demo: the consumer must report reading the
/// exact value the producer wrote into the handed-off frame. That proves the
/// transferred capability named a usable frame whose data survived the move.
fn check_share(actual: &str, pattern: u64) {
    let marker = "share: consumer got ";
    let read = actual
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix(marker))
        .and_then(|rest| rest.trim().parse::<u64>().ok());
    match read {
        Some(v) if v == pattern => {
            println!("smoke: cap-transfer ok (consumer read {v} from the handed-off frame)");
        }
        Some(v) => {
            eprintln!("smoke: FAIL cap-transfer: consumer read {v}, expected {pattern}");
            std::process::exit(1);
        }
        None => {
            eprintln!("smoke: FAIL cap-transfer: no consumer read-back line found");
            std::process::exit(1);
        }
    }
}

/// Verify the RPC demo: the client's `call N` results must run 0..calls in
/// program order, each returning `N + offset` -- proving the request reached
/// the server and the right reply came back to the right caller.
fn check_rpc(actual: &str, calls: u64, offset: u64) {
    let lines: Vec<&str> = actual.lines().map(str::trim).collect();
    let mut n = 0u64;
    let mut failed = false;
    for line in &lines {
        let Some(rest) = line.strip_prefix("client: call ") else {
            continue;
        };
        let Some((i_str, got_str)) = rest.split_once(" got ") else {
            continue;
        };
        let (Ok(i), Ok(got)) = (i_str.trim().parse::<u64>(), got_str.trim().parse::<u64>()) else {
            continue;
        };
        if i != n {
            eprintln!("smoke: rpc out of order: saw call {i}, expected {n}");
            failed = true;
        }
        if got != n + offset {
            eprintln!("smoke: rpc call {n}: got {got}, expected {}", n + offset);
            failed = true;
        }
        n += 1;
    }
    if n != calls {
        eprintln!("smoke: rpc: saw {n} calls, expected {calls}");
        failed = true;
    }
    if failed {
        eprintln!("smoke: FAIL (rpc call/reply)");
        std::process::exit(1);
    }
    println!("smoke: rpc call/reply ok ({calls} calls, replies verified)");
}

/// Verify spawn + wait: the parent must report the value its spawned worker
/// sent back over the result channel -- proving the child ran as a scheduled
/// process and the join (recv on spawn's handle) collected its result.
fn check_spawn(actual: &str, result: u64) {
    let marker = "spawner: worker returned ";
    let got = actual
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix(marker))
        .and_then(|rest| rest.trim().parse::<u64>().ok());
    match got {
        Some(v) if v == result => {
            println!("smoke: spawn+wait ok (parent collected worker result {v})");
        }
        Some(v) => {
            eprintln!("smoke: FAIL spawn+wait: parent got {v}, expected {result}");
            std::process::exit(1);
        }
        None => {
            eprintln!("smoke: FAIL spawn+wait: no parent result line found");
            std::process::exit(1);
        }
    }
}

/// Verify crash reaping: the parent waits on a child that faults before
/// sending, and must report that the dead child was reaped -- i.e. its wait was
/// woken with `IPC_PEER_DIED` instead of blocking forever. The spawn frame and
/// endpoint baselines additionally prove the crashed child leaked nothing.
fn check_reap(actual: &str) {
    let marker = "spawner: dead child reaped";
    if actual.lines().any(|l| l.contains(marker)) {
        println!("smoke: crash reaping ok (parent observed IPC_PEER_DIED, did not hang)");
    } else {
        eprintln!("smoke: FAIL crash reaping: no '{marker}' line -- the parent hung or mis-reported");
        std::process::exit(1);
    }
}

/// Verify the cap_release regression: a released
/// capability slot is reusable, so a process can spawn and join more times than
/// the system could ever have wait handles outstanding at once.
///
/// The demo exits non-zero partway through if any round fails, so the summary
/// line existing at all is most of the assertion; the round count is checked
/// too, so a demo silently shortened below the limit cannot pass while proving
/// nothing. This is the one check that would have caught the 2026-06-27 crash --
/// the shell tour's single launch never came close.
///
/// **The limit reached first is the eight-endpoint pool, not the 16-slot table**,
/// and both this function and the demo said otherwise until 2026-08-10. Measured
/// by deleting the demo's release: it dies at round 8 with fifteen cap slots
/// still free, because a leaked handle keeps its result endpoint referenced and
/// `MAX_ENDPOINTS` is 8. The regression is caught either way -- `ROUNDS` clears
/// both -- but this check should not be cited as evidence about table capacity.
/// See A-13.
fn check_caprelease(actual: &str, rounds: u64) {
    let marker = "caprelease: ";
    let got = actual
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix(marker))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse::<u64>().ok());
    match got {
        Some(v) if v == rounds => {
            println!("smoke: cap_release ok ({v} spawn round-trips, handles reused)");
        }
        Some(v) => {
            eprintln!("smoke: FAIL cap_release: {v} round-trips, expected {rounds}");
            std::process::exit(1);
        }
        None => {
            eprintln!(
                "smoke: FAIL cap_release: no summary line -- a round failed \
                 (a leaked wait handle exhausts the 8 endpoints at round 8)"
            );
            std::process::exit(1);
        }
    }
}

/// Verify the work-stealing demo (S4, section 6). Two
/// facts: (1) every worker completed -- `workers` distinct `stealwork[id] done`
/// lines AND the parent's `stealer: joined <workers> workers` line; (2) at
/// least one process actually moved to another core's array -- the kernel's
/// `steal demo: <n> steals across <c> cores` line, with `n >= 1` REQUIRED when
/// `c >= 2` (the only configuration where stealing is possible at all; under a
/// single online core the demo still completes with zero steals, which is
/// correct, not a failure). Interleaving-robust: it counts a role's own lines
/// and a before/after-style fact, never an exact global position -- so it holds
/// at -smp 1 and under real cross-core reordering at -smp N alike.
fn check_steal(actual: &str, workers: u64) {
    let lines: Vec<&str> = actual.lines().map(str::trim).collect();

    // (1a) every worker printed its own completion line, exactly once each.
    let done = lines
        .iter()
        .filter(|l| l.starts_with("stealwork[") && l.ends_with("] done"))
        .count() as u64;
    if done != workers {
        eprintln!("smoke: FAIL steal: saw {done} 'stealwork[..] done' lines, expected {workers}");
        std::process::exit(1);
    }

    // (1b) the parent joined them all.
    let joined = lines
        .iter()
        .find_map(|l| l.strip_prefix("stealer: joined "))
        .and_then(|rest| rest.strip_suffix(" workers"))
        .and_then(|n| n.trim().parse::<u64>().ok());
    match joined {
        Some(n) if n == workers => {}
        Some(n) => {
            eprintln!("smoke: FAIL steal: parent joined {n} workers, expected {workers}");
            std::process::exit(1);
        }
        None => {
            eprintln!("smoke: FAIL steal: no 'stealer: joined N workers' line -- parent hung?");
            std::process::exit(1);
        }
    }

    // (2) the cross-core move actually happened (when >= 2 cores are online).
    // Key on "steals across" so this matches the kernel's steal-count line and
    // not run()'s own "plinth: steal demo: N processes" launch banner.
    let parse = |l: &str| -> Option<(u64, u64)> {
        let (before, after) = l.split_once(" steals across ")?;
        let n = before.split_whitespace().last()?.parse().ok()?;
        let (c_str, _) = after.split_once(" cores")?;
        Some((n, c_str.trim().parse().ok()?))
    };
    let Some((steals, cores)) = lines.iter().find_map(|l| parse(l)) else {
        eprintln!("smoke: FAIL steal: no 'N steals across C cores' line");
        std::process::exit(1);
    };
    if cores >= 2 && steals == 0 {
        eprintln!("smoke: FAIL steal: {cores} cores online but 0 steals -- stealing did not fire");
        std::process::exit(1);
    }
    println!(
        "smoke: work-stealing ok ({workers} workers joined, {steals} steals across {cores} cores)"
    );
}

/// Verify the sub-region split demo (Stage 4): both band holders mapped their
/// disjoint band and drew it. The two `gfxsplit[N]: ok` lines come from
/// concurrent processes, so their order is nondeterministic -- assert presence,
/// not position (like check_steal's per-worker lines). Reaching `ok` means the
/// draw stayed inside the grant (an out-of-band write would have faulted first);
/// the boundary itself is exercised by gfxbound.
fn check_gfxsplit(actual: &str) {
    for want in ["gfxsplit[0]: ok", "gfxsplit[1]: ok"] {
        if !actual.lines().any(|l| l.contains(want)) {
            eprintln!("smoke: FAIL gfxsplit: missing {want:?} -- a band holder did not confine + draw");
            std::process::exit(1);
        }
    }
    println!("smoke: gfxsplit ok (both band holders confined to their grant and drew)");
}

fn smoke(uefi_path: &Path) {
    run_smoke_checks(uefi_path, true);
}

/// The full assertion battery `smoke` runs, factored out so `smoke_smp` (Stage
/// B2.4) can rerun most of it unchanged under `PLINTH_SMP`. Every check below
/// `check_smoke_output` is already interleaving-robust by construction
/// (Design section 2): each asserts a single process's/role's own values are
/// in order, or a before/after count matches, never an exact global line
/// position -- the same property that already lets them survive real timer-
/// preemption reordering at -smp 1 extends to real cross-core reordering at
/// -smp N.
///
/// `check_smoke_output` itself does NOT carry over: `expected_boot_log.txt`
/// hardcodes "acpi: 1 cpu(s), 1 ioapic(s)" (true, and asserted, only under
/// -smp 1 -- see that file's own comment), so `with_transcript` is false for
/// `smoke_smp`'s runs and true for `smoke`'s.
fn run_smoke_checks(uefi_path: &Path, with_transcript: bool) {
    let actual = run_capture(uefi_path);
    if with_transcript {
        let expected_path = workspace_root().join("expected_boot_log.txt");
        check_smoke_output(&actual, &expected_path);
    }
    check_per_process_order(&actual, SCHED_PROCESSES, SCHED_ITERS);
    check_frames_baseline(&actual, "scheduler");
    check_ipc_order(&actual, IPC_ROUNDS);
    check_frames_baseline(&actual, "ipc");
    check_endpoints_baseline(&actual, "ipc");
    check_share(&actual, SHARE_PATTERN);
    check_frames_baseline(&actual, "share");
    check_endpoints_baseline(&actual, "share");
    check_rpc(&actual, RPC_CALLS, RPC_OFFSET);
    check_frames_baseline(&actual, "rpc");
    check_endpoints_baseline(&actual, "rpc");
    check_spawn(&actual, SPAWN_RESULT);
    check_reap(&actual);
    check_frames_baseline(&actual, "spawn");
    check_endpoints_baseline(&actual, "spawn");
    check_caprelease(&actual, CAPRELEASE_ROUNDS);
    check_frames_baseline(&actual, "caprelease");
    check_endpoints_baseline(&actual, "caprelease");
    check_steal(&actual, STEAL_WORKERS);
    check_frames_baseline(&actual, "steal");
    check_endpoints_baseline(&actual, "steal");
    check_frames_baseline(&actual, "blk");
    check_frames_baseline(&actual, "asyncblk");
    check_frames_baseline(&actual, "blkwrite");
    check_frames_baseline(&actual, "fs");
    check_frames_baseline(&actual, "evt");
    check_frames_baseline(&actual, "evtstream");
    check_frames_baseline(&actual, "kbd");
    check_frames_baseline(&actual, "unified");
    check_frames_baseline(&actual, "mouse");
    check_frames_baseline(&actual, "rwfs");
    check_frames_baseline(&actual, "gfx");
    check_frames_baseline(&actual, "gfxtext");
    check_gfxsplit(&actual);
    check_frames_baseline(&actual, "gfxsplit");
    check_frames_baseline(&actual, "gfxbound");
    // The D7 hazard: unmapping a framebuffer must not
    // hand its firmware MMIO pages to the frame allocator. If the fb release
    // path ever reuses the Frame path's unmap-AND-deallocate, `after` climbs by
    // ~1000 and this is what catches it.
    check_frames_baseline(&actual, "gfxrevoke");
    // Same D7 hazard, one step further out: reclamation moves a framebuffer
    // capability between tables at death, so a path that reused the Frame arm's
    // unmap-AND-deallocate would leak the firmware's MMIO into the allocator here
    // too. Note what this does NOT prove -- gfxrevoke's baseline was flat in both
    // arms of its own negative control, so a flat count says nothing about whether
    // reclamation worked. The two differing hashes in the boot log are what say
    // that; this only says nothing was leaked while doing it.
    check_frames_baseline(&actual, "fbreclaim");
    // The BlockRange counterpart to fbreclaim (slice 4): the first
    // non-framebuffer lender. A BlockRange owns no frames (inline data), so the
    // baseline proves only that the parent's and child's I/O frames are both
    // reclaimed at teardown -- the homecoming-to-reserved-slot proof is the
    // asserted `range came back at slot ...` line in expected_boot_log.txt, which
    // names BLOCK_SLOT and goes red if the reservation is disabled.
    check_frames_baseline(&actual, "blkreclaim");
    // The A -> B -> C re-lend chain (slice 4 step 2 / K-025). Three
    // processes touch the range; a BlockRange owns no frames, so the baseline
    // proves every I/O frame is reclaimed at teardown. The K-025 proof is the
    // asserted `range came home to root at slot ...` line, which goes to NO_CAP if
    // the origin is laundered.
    check_frames_baseline(&actual, "blkrelend");
    // The IPC blocked-sender reclamation (slice 4 / K-026). The
    // sender lends a BlockRange over `send_cap` while blocked; the frame baseline
    // proves its and the receiver's I/O frames are reclaimed, and the endpoint
    // baseline proves the shared endpoint plus the spawn result channel are freed.
    // The K-026 proof is the asserted "range came home to sender at slot 1" line.
    check_frames_baseline(&actual, "blkipclend");
    check_endpoints_baseline(&actual, "blkipclend");
    // The same reclamation driven through `spawn_and_wait_cap` (K-012). The frame
    // baseline carries the D7 hazard exactly as fbreclaim's does. The endpoint
    // baseline proves only that the spawn endpoint does not outlive the demo --
    // NOT that the helper released its handle, which was checked by deleting that
    // release and watching every one of these stay green. Teardown frees the
    // process's whole table at exit, so a leak inside its life is invisible here.
    //
    // What does catch it is the demo's own part 2, added 2026-08-10: a loop of
    // handle-only round-trips whose asserted summary line in expected_boot_log.txt
    // disappears when the release does (it dies at round 7 instead). That is a
    // transcript assertion rather than a check here, so there is deliberately no
    // extra function for it.
    check_frames_baseline(&actual, "spawnwaitcap");
    check_endpoints_baseline(&actual, "spawnwaitcap");
    // The voluntary-release counterpart to fbreclaim (cap_release-on-reserved). Same
    // D7 hazard on the frame baseline; the endpoint baseline brackets the spawn the
    // lender does to hand the screen to the releasing child. The proof that the
    // screen actually came home on release is the two differing hashes in the boot
    // log (unreachable on the pre-ruling kernel, which dropped the released cap and
    // stranded the reserved slot) -- these only say nothing leaked doing it.
    check_frames_baseline(&actual, "fbrelease");
    check_endpoints_baseline(&actual, "fbrelease");
    check_frames_baseline(&actual, "shell");
}

/// Core counts `smoke-smp` boots under. 2 is the minimum that exercises any
/// AP at all; 3 and 4 are kept because Stage B2.3's real bugs needed 2+ APs
/// contending (never reproduced at -smp 2) to surface at all.
const SMP_TEST_CORE_COUNTS: &[u32] = &[2, 3, 4];

/// The multi-core regression lane (Stage B2.4, design D8): reruns `smoke`'s
/// own assertion battery once per count in `SMP_TEST_CORE_COUNTS`, so a
/// future concurrency regression is caught automatically instead of relying
/// on a manual `PLINTH_SMP` stress run the way Stage B2.3's bugs were found
/// and fixed. Deliberately a separate command from `smoke`, not folded into
/// it: -smp 1 stays the fast, fully deterministic (PLINTH_ICOUNT-compatible)
/// default every other check runs against.
fn smoke_smp(uefi_path: &Path) {
    for &cores in SMP_TEST_CORE_COUNTS {
        println!("smoke-smp: -smp {cores}");
        std::env::set_var("PLINTH_SMP", cores.to_string());
        run_smoke_checks(uefi_path, false);
    }
    std::env::remove_var("PLINTH_SMP");
}

/// Core counts `bench` runs the BKL contention hammer under. 1 is the
/// no-contention baseline (a single core can never contend with itself -- a
/// sanity check on the instrumentation); 2/3/4 show whether, and how much, the
/// single big kernel lock contends as cores are added.
const BENCH_CORE_COUNTS: &[u32] = &[1, 2, 3, 4];

/// Build the kernel with the BKL instrumentation + contention hammer compiled
/// in (`--features bench`). Like `build_all` -- it runs the full demo battery
/// before the hammer, so it needs every user crate and the boot archive -- but
/// writes a separate image so it never clobbers the smoke/run image.
fn build_bench() -> PathBuf {
    for name in USER_CRATES {
        build_user_crate(name);
    }
    archive_image();

    let root = workspace_root();
    let kernel_dir = root.join("kernel");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(&cargo)
        .current_dir(&kernel_dir)
        .args(["build", "--features", "bench"])
        .status()
        .expect("failed to invoke cargo for kernel bench build");
    assert!(status.success(), "kernel bench build failed");

    let kernel_bin = root.join("target/x86_64-unknown-none/debug/kernel");
    let out_dir = root.join("target/disk-images");
    std::fs::create_dir_all(&out_dir).unwrap();

    let uefi_path = out_dir.join("uefi-bench.img");
    bootloader::UefiBoot::new(&kernel_bin)
        .create_disk_image(&uefi_path)
        .unwrap();

    println!("bench disk image: {}", uefi_path.display());
    uefi_path
}

/// Run the BKL contention micro-benchmark once per core count and print the
/// kernel's report lines. The question it answers: is roadmap item B3 (splitting
/// the single big kernel lock) justified? If a pathological all-cores
/// kernel-entry hammer barely contends even at -smp 4, it is not.
fn bench(uefi_path: &Path) {
    // Default to the full 1/2/3/4 progression; PLINTH_BENCH_SMP (e.g. "3", or a
    // comma list) overrides it, so the residency sweep can pin one core count
    // and keep each step to a single boot.
    let counts: Vec<u32> = match std::env::var("PLINTH_BENCH_SMP") {
        Ok(s) => s.split(',').filter_map(|x| x.trim().parse().ok()).collect(),
        Err(_) => BENCH_CORE_COUNTS.to_vec(),
    };
    for &cores in &counts {
        println!("bench: -smp {cores}");
        std::env::set_var("PLINTH_SMP", cores.to_string());
        let out = run_capture(uefi_path);
        let mut saw_report = false;
        for line in out.lines() {
            if let Some(rest) = line.trim_end().strip_prefix("plinth: bkl bench: ") {
                println!("  {rest}");
                saw_report = true;
            }
        }
        if !saw_report {
            eprintln!("  (no bkl bench report captured -- the run may have hung; check serial)");
        }
    }
    std::env::remove_var("PLINTH_SMP");
}

/// Build the kernel with the test suite compiled in. Uses a separate
/// image path so it never clobbers the smoke/run image.
fn build_test() -> PathBuf {
    for name in USER_CRATES {
        build_user_crate(name);
    }

    let root = workspace_root();
    let kernel_dir = root.join("kernel");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(&cargo)
        .current_dir(&kernel_dir)
        .args(["build", "--features", "tests"])
        .status()
        .expect("failed to invoke cargo for kernel test build");
    assert!(status.success(), "kernel test build failed");

    let kernel_bin = root.join("target/x86_64-unknown-none/debug/kernel");
    let out_dir = root.join("target/disk-images");
    std::fs::create_dir_all(&out_dir).unwrap();

    let uefi_path = out_dir.join("uefi-test.img");
    bootloader::UefiBoot::new(&kernel_bin)
        .create_disk_image(&uefi_path)
        .unwrap();

    println!("test disk image: {}", uefi_path.display());
    uefi_path
}

fn run_tests(uefi_path: &Path) {
    let output = run_capture(uefi_path);
    parse_test_output(&output);
}

/// Boot with the i8042 removed (`-machine q35,i8042=off`) and assert the
/// absent-hardware path (real_hardware.md D7): the kernel must detect the
/// missing controller, report it, arm nothing, and still run the scripted tour
/// to completion on synthetic input. QEMU always emulates a working i8042 under
/// the normal machine, so this is the only lane that exercises the branch a
/// serial-less, keyboard-less real machine takes.
fn no_i8042_check(uefi_path: &Path) {
    let output = run_capture_machine(uefi_path, ",i8042=off");
    let reported_absent = output.contains("i8042 absent, input disabled");
    let armed_keyboard = output.contains("keyboard ready");
    let booted = output.contains("boot ok");
    if reported_absent && booted && !armed_keyboard {
        println!(
            "no-i8042: ok (controller absence reported, IRQ1 not armed, boot ran to completion)"
        );
        return;
    }
    eprintln!("no-i8042: FAIL");
    eprintln!("  \"i8042 absent, input disabled\" present: {reported_absent} (want true)");
    eprintln!("  \"boot ok\" present:                      {booted} (want true)");
    eprintln!("  \"keyboard ready\" present:               {armed_keyboard} (want false)");
    eprintln!("--- captured output ---");
    eprintln!("{output}");
    eprintln!("--- end output ---");
    std::process::exit(1);
}

/// Probe whether this host's QEMU exposes the `dma-remap` property on the
/// emulated `amd-iommu` device. That property is what makes QEMU actually enforce
/// AMD-Vi DMA translation on emulated devices -- the AMD analogue of
/// intel-iommu's `caching-mode` (see build_qemu_cmd). It was added in QEMU 10.1;
/// older QEMU (e.g. Debian trixie's 10.0.x, which the CI container installs)
/// realizes the amd-iommu unit but rejects the property, so booting with
/// `dma-remap=on` dies at device init with "Property 'amd-iommu.dma-remap' not
/// found". Dropping the property is NOT an option: without it QEMU does not
/// enforce translation, so the AMD lane's "block reads verified" would pass
/// whether or not the backend works -- a vacuous proof of a security property.
/// So amd_check gates on this probe and skips (green) when the property is
/// absent; the moment the QEMU is >= 10.1 this returns true and the real lane
/// runs again, with no code change.
fn amd_iommu_dma_remap_supported() -> bool {
    match Command::new("qemu-system-x86_64")
        .args(["-device", "amd-iommu,help"])
        .output()
    {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            text.contains("dma-remap")
        }
        // Probe could not run at all -> treat the feature as absent and let
        // amd_check skip with a reason rather than hard-failing the caller.
        Err(_) => false,
    }
}

/// First line of `qemu-system-x86_64 --version`, for the skip message.
fn qemu_version_line() -> String {
    Command::new("qemu-system-x86_64")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Boot under QEMU's emulated AMD-Vi and assert the AMD-Vi backend end to end:
/// the IVRS unit is discovered, translation is enabled, and both kernel-bridged
/// block devices read correctly under AMD-Vi translation. The dual-backend lane
/// alongside the default VT-d `smoke`. Positive-only (D6): QEMU's amd-iommu does
/// not fault an out-of-domain virtio DMA, so there is no negative to assert here.
fn amd_check(uefi_path: &Path) {
    // Capability gate: this lane needs `amd-iommu,dma-remap=on` (QEMU >= 10.1).
    // On older QEMU, skip with a reason instead of failing -- see
    // amd_iommu_dma_remap_supported. Probe before setting PLINTH_IOMMU so a skip
    // leaves no env state behind.
    if !amd_iommu_dma_remap_supported() {
        println!(
            "amd: SKIP -- this QEMU lacks the amd-iommu 'dma-remap' property \
             (added in QEMU 10.1); found: {}. The AMD-Vi integration lane is not \
             run here; PteFmt::AmdVi encode/decode stays covered by the \
             amdvi_map_translate_roundtrip unit test in `cargo xtask test`. This \
             lane runs for real once the QEMU is >= 10.1.",
            qemu_version_line()
        );
        return;
    }

    // build_qemu_cmd reads PLINTH_IOMMU to pick amd-iommu and shift the virtio slots.
    std::env::set_var("PLINTH_IOMMU", "amd");
    let output = run_capture(uefi_path);

    let discovered = output.contains("vendor amd-vi");
    let one_unit = output.contains("1 dma remapping unit(s)");
    let enabled = output.contains("plinth: iommu: translation enabled");
    let blk0 = output.contains("virtio-blk[0] sector 0 read ok");
    let blk1 = output.contains("virtio-blk[1] sector 0 read ok");
    let booted = output.contains("shell: quit");

    if discovered && one_unit && enabled && blk0 && blk1 && booted {
        println!(
            "amd: ok (AMD-Vi discovered, translation enabled, block reads verified under amd-iommu)"
        );
        return;
    }
    eprintln!("amd: FAIL");
    eprintln!("  amd-vi discovered:     {discovered} (want true)");
    eprintln!("  1 remapping unit:      {one_unit} (want true)");
    eprintln!("  translation enabled:   {enabled} (want true)");
    eprintln!("  blk0 read ok:          {blk0} (want true)");
    eprintln!("  blk1 read ok:          {blk1} (want true)");
    eprintln!("  booted to shell quit:  {booted} (want true)");
    eprintln!("--- captured output ---");
    eprintln!("{output}");
    eprintln!("--- end output ---");
    std::process::exit(1);
}

/// Build the kernel with the framebuffer console forced on (D11 `force_console`
/// feature), so the no-serial rendering path runs under QEMU and reports a hash.
fn build_force_console() -> PathBuf {
    for name in USER_CRATES {
        build_user_crate(name);
    }
    archive_image();

    let root = workspace_root();
    let kernel_dir = root.join("kernel");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(&cargo)
        .current_dir(&kernel_dir)
        .args(["build", "--features", "force_console"])
        .status()
        .expect("failed to invoke cargo for force_console kernel build");
    assert!(status.success(), "force_console kernel build failed");

    let kernel_bin = root.join("target/x86_64-unknown-none/debug/kernel");
    let out_dir = root.join("target/disk-images");
    std::fs::create_dir_all(&out_dir).unwrap();

    let uefi_path = out_dir.join("uefi-console.img");
    bootloader::UefiBoot::new(&kernel_bin)
        .create_disk_image(&uefi_path)
        .unwrap();

    println!("console disk image: {}", uefi_path.display());
    uefi_path
}

/// The frame hash the forced console produces for its fixed test string over
/// the pinned QEMU/OVMF framebuffer. Deterministic for the same reason the gfx
/// demo hashes are (fixed geometry + a fixed origin square); if QEMU or OVMF
/// moves, re-derive it from the printed line, exactly as for the gfx hashes.
const CONSOLE_EXPECT_HASH: &str = "console: framebuffer hash 0x3f7b275d1ca6a727";

/// Boot the forced-console build and assert the framebuffer console rendered the
/// expected frame. This is the D11 requirement that the no-serial path be
/// exercised and *asserted*, not eyeballed.
fn console_check(uefi_path: &Path) {
    let output = run_capture(uefi_path);
    let line = output
        .lines()
        .find(|l| l.contains("console: framebuffer hash "));
    match line {
        Some(l) => {
            let l = l.trim();
            println!("{l}");
            if l != CONSOLE_EXPECT_HASH {
                eprintln!("console: FAIL -- hash mismatch");
                eprintln!("  expected: {CONSOLE_EXPECT_HASH}");
                eprintln!("  actual:   {l}");
                std::process::exit(1);
            }
            println!("console: ok (framebuffer diagnostic console rendered the expected frame)");
        }
        None => {
            eprintln!("console: FAIL -- no framebuffer hash line in boot output");
            eprintln!("--- captured output ---");
            eprintln!("{output}");
            eprintln!("--- end output ---");
            std::process::exit(1);
        }
    }
}

/// Scan captured serial output for the harness tags and print a result
/// table. Fails if any test failed or if the [SUITE] line is missing
/// (which means the kernel panicked mid-suite).
fn parse_test_output(output: &str) {
    let mut results: Vec<(String, bool, String)> = Vec::new();
    let mut suite_line: Option<String> = None;

    for line in output.lines() {
        if let Some(name) = line.strip_prefix("[PASS] ") {
            results.push((name.trim().to_string(), true, String::new()));
        } else if let Some(rest) = line.strip_prefix("[FAIL] ") {
            let (name, reason) = rest.split_once(": ").unwrap_or((rest, "unknown"));
            results.push((name.trim().to_string(), false, reason.trim().to_string()));
        } else if line.starts_with("[SUITE] ") {
            suite_line = Some(line.to_string());
        }
    }

    println!("\nTest Results:");
    println!("{}", "-".repeat(60));
    for (name, passed, reason) in &results {
        if *passed {
            println!("  PASS  {name}");
        } else {
            println!("  FAIL  {name}  -- {reason}");
        }
    }
    println!("{}", "-".repeat(60));

    if let Some(ref suite) = suite_line {
        println!("{suite}");
    }

    let any_failed = results.iter().any(|(_, passed, _)| !passed);
    let no_suite = suite_line.is_none();

    if any_failed || no_suite {
        eprintln!("test: FAIL");
        if no_suite {
            eprintln!("test: [SUITE] line not found -- kernel may have panicked");
            eprintln!("--- captured output ---");
            eprintln!("{output}");
            eprintln!("--- end output ---");
        }
        std::process::exit(1);
    }
    println!("test: ok");
}
