// Mirrors `akuma-threading` / `akuma-exec`: emit `kernel_smp_shared` from the
// forwarded `smp-shared` feature, so every `#[cfg(...)]` in this crate matches the rest of the tree
// instead of silently compiling the `not()` branch.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }
}
