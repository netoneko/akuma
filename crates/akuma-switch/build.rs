// Mirrors `akuma-exec` / `akuma-primitives`: emit `kernel_bkl_smp` from the
// forwarded `smp-shared` feature, so every `#[cfg(...)]` in this crate matches the rest of the tree
// instead of silently compiling the `not()` branch.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_bkl_smp)");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BKL_SMP");
    if std::env::var("CARGO_FEATURE_BKL_SMP").is_ok() {
        println!("cargo:rustc-cfg=kernel_bkl_smp");
    }
}
