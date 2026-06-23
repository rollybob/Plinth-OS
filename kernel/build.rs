// Copy each user-crate release ELF into OUT_DIR so main.rs can embed it with
// include_bytes!(concat!(env!("OUT_DIR"), "/<name>-user")). Embedding from
// OUT_DIR -- which cargo recomputes on every build -- means no absolute path is
// ever baked into the build cache. Moving or renaming the project tree can
// therefore never leave the kernel pointing at a vanished location; at worst it
// triggers a rebuild. (The old approach emitted absolute paths via
// cargo:rustc-env, which cargo cached and happily reused after a rename.)
//
// Run `cargo xtask build` at least once before building the kernel directly, so
// the source *-user ELFs exist for this script to copy.

use std::path::PathBuf;

// Embedded in the kernel. fsdemo (the FS library-OS loader) is embedded as the
// bootstrap loader; diskhello is deliberately absent -- it lives only in the
// boot archive, so running it proves the load-from-disk path.
const USER_BINARIES: &[&str] = &[
    "hello", "bump", "list", "crash", "greedy", "lazy", "spawner", "grantee", "spin", "pingpong",
    "share", "rpc", "faultchild", "blk", "asyncblk", "fsdemo", "evt", "evtstream", "kbd",
];

// Embedded only by the `bench` build (`cargo xtask bench`), matching the
// feature-gated `include_bytes!` in main.rs. Copying it unconditionally would
// make every production kernel build require the bench crate's ELF to exist
// (build.rs panics if a source binary is missing), so gate it on the feature
// the same way -- Cargo sets CARGO_FEATURE_BENCH for this script when the
// kernel is built with `--features bench`.
const BENCH_BINARIES: &[&str] = &["bench"];

fn main() {
    // Read at RUNTIME (std::env::var), not at compile time (env!): a value
    // baked in when this script was compiled is exactly what went stale across
    // the rename. Cargo sets both of these freshly every time it runs the
    // script, so reading them here is always current.
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.parent().expect("kernel/ has a parent directory");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed=build.rs");

    let bench = std::env::var("CARGO_FEATURE_BENCH").is_ok();
    let binaries = USER_BINARIES
        .iter()
        .chain(if bench { BENCH_BINARIES } else { &[] });

    for name in binaries {
        let src = workspace_root.join(format!("target/x86_64-unknown-none/release/{name}-user"));
        let dst = out_dir.join(format!("{name}-user"));
        // Re-copy (and rebuild the kernel) whenever a user binary changes.
        println!("cargo:rerun-if-changed={}", src.display());
        std::fs::copy(&src, &dst).unwrap_or_else(|e| {
            panic!(
                "failed to copy user binary\n  from {}\n  to   {}\n  ({e})\n\
                 -- run `cargo xtask build` once so the *-user ELFs exist",
                src.display(),
                dst.display()
            )
        });
    }
}
