fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_size)");
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");
    let size_profile = std::env::var("OPT_LEVEL").as_deref() == Ok("z");
    if size_profile {
        println!("cargo:rustc-cfg=kernel_profile_size");
    }
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    if size_profile && std::env::var("CARGO_FEATURE_EXTREME").is_ok() {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }
}
