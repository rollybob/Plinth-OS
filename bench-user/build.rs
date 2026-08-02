fn main() {
    // The linker script is SHARED by every user crate: ../user.ld, one level up
    // from this crate's manifest dir. It used to be a per-crate copy; see the
    // note at the top of user.ld for why that stopped being acceptable.
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let linker_script = manifest_dir
        .parent()
        .expect("a user crate sits one level below the repo root")
        .join("user.ld");
    println!("cargo:rerun-if-changed={}", linker_script.display());
    println!("cargo:rustc-link-arg=-T{}", linker_script.display());
    // Link as a static, non-PIE ET_EXEC. The kernel's ELF loader accepts
    // only ET_EXEC and applies no relocations; without -no-pie the
    // bare-metal target emits a PIE (ET_DYN), which the loader rejects.
    println!("cargo:rustc-link-arg=-no-pie");
    // The residency sweep bakes the userspace-work amount in from this env var
    // (read via option_env! in main.rs); rebuild when it changes so each sweep
    // step gets a fresh binary.
    println!("cargo:rerun-if-env-changed=PLINTH_BENCH_WORK");
}
