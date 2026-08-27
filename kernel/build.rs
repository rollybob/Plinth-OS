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
    "share", "rpc", "faultchild", "blk", "asyncblk", "blkwrite", "fsdemo", "evt", "evtstream",
    "unified", "kbd", "mouse", "rwfs", "stealer", "stealwork", "gfx", "gfxtext",
    "gfxsplit", "gfxbound", "gfxrevoke", "fbreclaim", "fbreclaimchild", "fbrelease",
    "fbreleasechild", "shell", "shellapp",
    "caprelease", "quietworker", "spawnwaitcap",
    "blkreclaim", "blkreclaimchild",
    "blkrelend", "blkrelendmid",
    "blkipclend", "blkrecvchild",
    "bind",
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

    // Debug-only single-demo filter. A boot runs ~23 demos in sequence, which
    // makes bisecting a hang or a fault miserable; PLINTH_DEMO=<substring>
    // builds a kernel that skips every demo whose label does not match, so one
    // can be reproduced in isolation. Forwarded as a rustc-env so
    // `option_env!` in scheduler.rs sees it, and rerun-if-env-changed so
    // changing (or clearing) it actually rebuilds instead of serving a cached
    // kernel with the previous filter compiled in -- the failure mode that
    // would make this tool lie.
    println!("cargo:rerun-if-env-changed=PLINTH_DEMO");
    if let Ok(filter) = std::env::var("PLINTH_DEMO") {
        println!("cargo:rustc-env=PLINTH_DEMO={filter}");
    }

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
