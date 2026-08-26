fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");
    println!("cargo::rustc-check-cfg=cfg(ext2_fs_cache)");

    // The `extreme` FEATURE is the discriminator, not the profile — exactly what
    // the root Cargo.toml says of `[profile.extreme-size]` ("build.rs keys the
    // extreme behaviour off the `extreme` FEATURE, not the profile — they are
    // selected together by scripts/build_extreme_size.sh"), and `extreme =
    // [..., "akuma-ext2/extreme", ...]` forwards it here.
    //
    // This used to ALSO emit the cfg on `OPT_LEVEL == "z"` alone. That dated
    // from when a second size profile (`size`) existed and wanted the same
    // behaviour; it was removed 2026-08-10, leaving a test that fires for any
    // size-optimised consumer of this crate — including the `userspace/`
    // workspace, whose release profile is `opt-level = "z"`. That made
    // `ext2probe-host --release` unbuildable: the crate believed it was the
    // extreme kernel (no block cache) while `fs-cache` still compiled
    // `ClockBlockCache`, which then had nothing to construct it and failed
    // `-D dead-code`. Requiring the feature keeps every shipped kernel target
    // byte-identical (build_extreme_size.sh passes `extreme` and opt-level z
    // together) and lets host tools build at any optimisation level.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    if std::env::var("CARGO_FEATURE_EXTREME").is_ok() {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }

    // The large clock block cache (opt-in). Forwarded from the kernel's
    // `fs-cache` feature; never combined with the minimal `extreme` profile.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_FS_CACHE");
    if std::env::var("CARGO_FEATURE_FS_CACHE").is_ok() {
        println!("cargo:rustc-cfg=ext2_fs_cache");
    }
}
