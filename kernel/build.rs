// Expose the user binaries (built by xtask into target/user/) as env vars
// so main.rs can embed them with include_bytes!. Run `cargo xtask build`
// at least once before building the kernel directly.

const USER_BINARIES: &[&str] = &["hello", "bump", "list", "crash"];

fn main() {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap();

    for name in USER_BINARIES {
        let bin = workspace_root.join(format!("target/user/{name}.bin"));
        println!("cargo:rerun-if-changed={}", bin.display());
        println!("cargo:rustc-env={}_BIN={}", name.to_uppercase(), bin.display());
    }
}
