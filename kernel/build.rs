// Expose the user-crate release ELFs as env vars so main.rs can embed them
// with include_bytes!. The kernel's ELF loader parses each at load time.
// Run `cargo xtask build` at least once before building the kernel directly.

const USER_BINARIES: &[&str] = &[
    "hello", "bump", "list", "crash", "greedy", "lazy", "spawner", "grantee", "spin", "pingpong",
    "share", "rpc",
];

fn main() {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap();

    for name in USER_BINARIES {
        let elf = workspace_root.join(format!("target/x86_64-unknown-none/release/{name}-user"));
        println!("cargo:rerun-if-changed={}", elf.display());
        println!("cargo:rustc-env={}_BIN={}", name.to_uppercase(), elf.display());
    }
}
