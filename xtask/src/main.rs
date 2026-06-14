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
const USER_CRATES: &[&str] =
    &["hello", "bump", "list", "crash", "greedy", "lazy", "spawner", "grantee", "template"];

/// Build all user crates, then the kernel + disk image.
fn build_all() -> PathBuf {
    for name in USER_CRATES {
        build_user_crate(name);
    }
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

    // Log CPU resets and exceptions for post-mortem debugging.
    cmd.args(["-D", "qemu_debug.log", "-d", "cpu_reset,int"]);

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

fn smoke(uefi_path: &Path) {
    let actual = run_capture(uefi_path);
    let expected_path = workspace_root().join("expected_boot_log.txt");
    check_smoke_output(&actual, &expected_path);
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
