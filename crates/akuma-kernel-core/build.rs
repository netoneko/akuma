// Mirrors the `cfg(kernel_*)` derivations from the root crate's `build.rs`
// that the modules moved here (`timer.rs`, `file_page_cache.rs`, `console.rs`)
// still check.
// Cargo sets `CARGO_FEATURE_*` per-package from THIS package's own resolved
// features, so this crate needs its own copies of `no-tests`/
// `smp-shared` (see Cargo.toml) and its own build script to read them — the
// root's build.rs only ever spoke for the root package.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_tests)");
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");
    println!("cargo::rustc-check-cfg=cfg(kernel_console_lock)");

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_TESTS");
    let kernel_tests = std::env::var("CARGO_FEATURE_NO_TESTS").is_err();
    if kernel_tests {
        println!("cargo:rustc-cfg=kernel_tests");
    }

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }

    // `console.rs`'s opt-in cross-core lock. Same derivation as the root and
    // `akuma-kernel-glue` build scripts: on unless `CONSOLE_LOCK=0`.
    println!("cargo:rerun-if-env-changed=CONSOLE_LOCK");
    let console_lock = std::env::var("CONSOLE_LOCK").as_deref() != Ok("0");
    if console_lock {
        println!("cargo:rustc-cfg=kernel_console_lock");
    }
}
