// Mirrors `akuma-exec` / `akuma-primitives`: emit `kernel_smp_shared` from the
// forwarded `smp-shared` feature and `kernel_profile_extreme` under the size
// profiles, so every `#[cfg(...)]` in this crate matches the rest of the tree
// instead of silently compiling the `not()` branch.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }

    let size_profile = std::env::var("OPT_LEVEL").as_deref() == Ok("z");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    if size_profile && std::env::var("CARGO_FEATURE_EXTREME").is_ok() {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }
}
