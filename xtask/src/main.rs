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
        "build"   => { build(); }
        "run"     => { let img = build(); run(&img, false); }
        "run-gdb" => { let img = build(); run(&img, true); }
        "smoke"   => { let img = build(); smoke(&img); }
        "test"    => { let img = build_test(); run_tests(&img); }
        other     => { eprintln!("unknown subcommand: {other}"); std::process::exit(1); }
    }
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

const TIMEOUT_SECS: u64 = 60;

/// Wait for QEMU with a hard timeout. Returns the exit code, or i32::MIN
/// if the process was killed because it timed out.
fn wait_qemu(mut child: std::process::Child) -> i32 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);
    loop {
        if let Some(s) = child.try_wait().expect("failed to wait on qemu") {
            return s.code().unwrap_or(1);
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("QEMU timed out after {TIMEOUT_SECS}s -- killing");
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
