//! The suite's `kernel_*` cfgs, derived exactly as the root package's `build.rs` derives them,
//! from the same-named features the root forwards here. `OPT_LEVEL` is the profile's, which a
//! dependency's build script sees the same way the root's does.

fn main() {
    for cfg in [
        "kernel_tests",
        "kernel_profile_extreme",
        "kernel_smp_shared",
        "kernel_bkl_profile",
        "kernel_audio",
    ] {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }
    for var in [
        "CARGO_FEATURE_NO_TESTS",
        "CARGO_FEATURE_EXTREME",
        "CARGO_FEATURE_SMP_SHARED",
        "CARGO_FEATURE_BKL_PROFILE",
        "CARGO_FEATURE_SOUND",
        "CARGO_FEATURE_PLATFORM_FIRECRACKER",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let feature = |name: &str| std::env::var_os(format!("CARGO_FEATURE_{name}")).is_some();
    let size_opt = std::env::var("OPT_LEVEL").as_deref() == Ok("z");
    if !feature("NO_TESTS") && !size_opt {
        println!("cargo:rustc-cfg=kernel_tests");
    }
    if size_opt && feature("EXTREME") {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }
    if feature("SMP_SHARED") {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }
    if feature("BKL_PROFILE") {
        println!("cargo:rustc-cfg=kernel_bkl_profile");
    }
    if feature("SOUND") && !feature("PLATFORM_FIRECRACKER") {
        println!("cargo:rustc-cfg=kernel_audio");
    }
}
