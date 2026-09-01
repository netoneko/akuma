// Mirrors the `cfg(kernel_*)` derivations from the root crate's `build.rs` that
// the modules here still check. Cargo sets `CARGO_FEATURE_*` per-package from
// THIS package's own resolved features, so this crate needs its own build
// script (see `akuma-kernel-core/build.rs` for the same pattern, one layer down).
fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }
}
