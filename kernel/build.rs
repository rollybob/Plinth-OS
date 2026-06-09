// Expose the user binaries (built by xtask into target/user/) as env vars
// so main.rs can embed them with include_bytes!. Run `cargo xtask build`
// at least once before building the kernel directly.

fn main() {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap();

    let hello_bin = workspace_root.join("target/user/hello.bin");
    println!("cargo:rerun-if-changed={}", hello_bin.display());
    println!("cargo:rustc-env=HELLO_BIN={}", hello_bin.display());
}
