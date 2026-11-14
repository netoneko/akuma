//! The suite's `kernel_*` cfgs, derived exactly as the root package's `build.rs` derives them,
//! from the same-named features the root forwards here.

fn main() {
    for cfg in [
        "kernel_tests",
        "kernel_bkl_profile",
        "kernel_audio",
    ] {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }
    for var in [
        "CARGO_FEATURE_NO_TESTS",
        "CARGO_FEATURE_BKL_PROFILE",
        "CARGO_FEATURE_SOUND",
        "CARGO_FEATURE_PLATFORM_FIRECRACKER",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let feature = |name: &str| std::env::var_os(format!("CARGO_FEATURE_{name}")).is_some();
    if !feature("NO_TESTS") {
        println!("cargo:rustc-cfg=kernel_tests");
    }
    if feature("BKL_PROFILE") {
        println!("cargo:rustc-cfg=kernel_bkl_profile");
    }
    if feature("SOUND") && !feature("PLATFORM_FIRECRACKER") {
        println!("cargo:rustc-cfg=kernel_audio");
    }
}
