fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_size)");
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");
    println!("cargo::rustc-check-cfg=cfg(ext2_fs_cache)");
    let size_profile = std::env::var("OPT_LEVEL").as_deref() == Ok("z");
    if size_profile {
        println!("cargo:rustc-cfg=kernel_profile_size");
    }
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    if size_profile && std::env::var("CARGO_FEATURE_EXTREME").is_ok() {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }
    // The large clock block cache (opt-in). Forwarded from the kernel's
    // `fs-cache` feature; never combined with the minimal `extreme` profile.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_FS_CACHE");
    if std::env::var("CARGO_FEATURE_FS_CACHE").is_ok() {
        println!("cargo:rustc-cfg=ext2_fs_cache");
    }
}
