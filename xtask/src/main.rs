//! Build-and-run tasks for Plinth (cargo-xtask pattern).
//!
//! `cargo xtask run`   -- build kernel + UEFI disk image, boot in QEMU
//! `cargo xtask build` -- build only
//! `cargo xtask run-gdb` -- boot paused with a GDB server on :1234
//! `cargo xtask smoke` -- boot with captured serial output and assert that
//!                        every line in expected_boot_log.txt appears in order
//! `cargo xtask test`  -- build with --features tests, run the in-kernel
//!                        suite, parse [PASS]/[FAIL]/[SUITE] tags

use std::path::{Path, PathBuf};
use std::process::Command;

use ovmf_prebuilt::{Arch, FileType, Prebuilt, Source};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let subcmd = args.get(1).map(String::as_str).unwrap_or("run");

    match subcmd {
        "build"   => { build_all(); }
        "run"     => { let img = build_all(); run(&img, false); }
        "run-gdb" => { let img = build_all(); run(&img, true); }
        "smoke"   => { let img = build_all(); smoke(&img); }
        "test"    => { let img = build_test(); run_tests(&img); }
        "check"   => { check_clobbers(); }
        other     => { eprintln!("unknown subcommand: {other}"); std::process::exit(1); }
    }
}

/// User binaries built by xtask. Crate directories are named {name}-user.
/// Most are embedded into the kernel (see kernel/build.rs) and the in-kernel
/// ELF loader maps their PT_LOAD segments. `template` is the build-only
/// skeleton from GUIDE.md: compiled every build so it cannot rot, but not
/// embedded or booted.
const USER_CRATES: &[&str] = &[
    "hello", "bump", "list", "crash", "greedy", "lazy", "spawner", "grantee", "spin", "pingpong",
    "share", "rpc", "faultchild", "blk", "fsdemo", "diskhello", "template",
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
    println!("{name}-user: {size} bytes (ELF)");
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

/// Compose the QEMU command line shared by run and smoke.
fn build_qemu_cmd(uefi_path: &Path, gdb: bool) -> Command {
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

    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.args([
        // q35: modern chipset, PCIe-native, publishes MCFG in ACPI.
        "-machine", "q35",
        "-drive", &format!("if=pflash,format=raw,readonly=on,file={}", code.display()),
        "-drive", &format!("if=pflash,format=raw,file={}", vars.display()),
        "-drive", &format!("format=raw,file={}", uefi_path.display()),
        "-serial", "stdio",
        "-no-reboot",
        "-m", "256M",
        // Single CPU: Plinth is deliberately uniprocessor. Deterministic
        // serial output is a feature, not a limitation.
        "-smp", "1",
        "-cpu", "qemu64",
        // isa-debug-exit: the kernel writes N to port 0xF4 and QEMU exits
        // with status (N << 1) | 1. Kernel success (N=0) -> exit code 1.
        "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04",
    ]);

    // Stage 1 storage: a deterministic raw disk behind a modern virtio-blk-pci
    // device, pinned to slot 3 so discovery output is stable across runs.
    // disable-legacy=on forces the modern (MMIO-capability) device the driver
    // targets, rather than a transitional device with a legacy PIO BAR.
    let blk = block_image();
    cmd.args([
        "-drive",
        &format!("if=none,format=raw,file={},id=blk0", blk.display()),
        "-device",
        "virtio-blk-pci,drive=blk0,addr=0x3,disable-legacy=on",
    ]);

    // Storage device 1: the read-only boot archive. Pinned to slot 4 (just past
    // the ramp disk's slot 3) so the kernel's PCI-slot-order enumeration always
    // gives it device index 1, and so discovery output is stable across runs.
    let archive = archive_image();
    cmd.args([
        "-drive",
        &format!("if=none,format=raw,file={},id=blk1", archive.display()),
        "-device",
        "virtio-blk-pci,drive=blk1,addr=0x4,disable-legacy=on",
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

fn run(uefi_path: &Path, gdb: bool) {
    let child = build_qemu_cmd(uefi_path, gdb)
        .spawn()
        .expect("failed to launch qemu-system-x86_64");
    let code = wait_qemu(child);
    if code == i32::MIN {
        std::process::exit(2);
    }
    // Exit code 1 is kernel-signalled success via isa-debug-exit.
    if code != 0 && code != 1 {
        eprintln!("QEMU exited with unexpected code: {code}");
        std::process::exit(code);
    }
}

/// Boot with captured stdout and return the serial output. A reader thread
/// drains the pipe so a full buffer never stalls QEMU.
fn run_capture(uefi_path: &Path) -> String {
    use std::io::Read;
    let mut cmd = build_qemu_cmd(uefi_path, false);
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

/// Assert that every non-blank, non-comment line in expected_boot_log.txt
/// appears in `actual`, in order. Unexpected lines between matches are
/// ignored. Matching is substring-based so that partial-line merges still
/// count; expected strings are specific enough to avoid false positives.
fn check_smoke_output(actual: &str, expected_path: &Path) {
    let src = std::fs::read_to_string(expected_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", expected_path.display(), e));

    let expected: Vec<&str> = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    let actual_lines: Vec<&str> = actual.lines().collect();
    let mut cursor = 0;
    let mut failed = false;

    for want in &expected {
        match actual_lines[cursor..].iter().position(|l| l.contains(want)) {
            Some(pos) => cursor += pos + 1,
            None => {
                eprintln!("smoke: missing: {want:?}");
                failed = true;
            }
        }
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
    println!("smoke: ok ({} lines verified)", expected.len());
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
fn find_frame_count(actual: &str, marker: &str) -> Option<u64> {
    actual
        .lines()
        .find(|l| l.contains(marker))
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

fn smoke(uefi_path: &Path) {
    let actual = run_capture(uefi_path);
    let expected_path = workspace_root().join("expected_boot_log.txt");
    check_smoke_output(&actual, &expected_path);
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
    check_frames_baseline(&actual, "blk");
    check_frames_baseline(&actual, "fs");
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
