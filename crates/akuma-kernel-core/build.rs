// Mirrors the three `cfg(kernel_*)` derivations from the root crate's `build.rs`
// that the modules moved here (`timer.rs`, `file_page_cache.rs`) still check.
// Cargo sets `CARGO_FEATURE_*` per-package from THIS package's own resolved
// features, so this crate needs its own copies of `extreme`/`no-tests`/
// `smp-shared` (see Cargo.toml) and its own build script to read them — the
// root's build.rs only ever spoke for the root package.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_tests)");
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_TESTS");
    let kernel_tests = std::env::var("CARGO_FEATURE_NO_TESTS").is_err()
        && std::env::var("OPT_LEVEL").as_deref() != Ok("z");
    if kernel_tests {
        println!("cargo:rustc-cfg=kernel_tests");
    }

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    let size_opt = std::env::var("OPT_LEVEL").as_deref() == Ok("z");
    let extreme_profile = size_opt && std::env::var("CARGO_FEATURE_EXTREME").is_ok();
    if extreme_profile {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }
}
