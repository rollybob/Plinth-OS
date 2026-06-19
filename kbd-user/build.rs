fn main() {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let linker_script = manifest_dir.join("linker.ld");
    println!("cargo:rerun-if-changed={}", linker_script.display());
    println!("cargo:rustc-link-arg=-T{}", linker_script.display());
    // Static, non-PIE ET_EXEC: the kernel's loader accepts only ET_EXEC and
    // applies no relocations.
    println!("cargo:rustc-link-arg=-no-pie");
}
