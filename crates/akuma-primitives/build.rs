// Mirrors the bin crate / akuma-exec / akuma-net / akuma-ext2: emit
// `kernel_profile_extreme` under the size profiles, and `kernel_smp_shared` /
// `kernel_no_bkl_*` from the forwarded features.
//
// This crate needs them because it owns `MAX_THREADS` (256 normally, 64 on the
// size profiles — the per-slot statics are BSS whether used or not) and
// `PreemptGuard`, whose IRQ-masking half is conditional on the BKL-drop
// features. Without this build.rs every `#[cfg(kernel_profile_extreme)]` here
// would silently compile the `not()` branch — the exact dormant-gate bug
// `akuma-exec`'s own build.rs comment records.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_network)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_vfs)");

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_BKL_NETWORK");
    if std::env::var("CARGO_FEATURE_NO_BKL_NETWORK").is_ok() {
        println!("cargo:rustc-cfg=kernel_no_bkl_network");
    }

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_BKL_VFS");
    if std::env::var("CARGO_FEATURE_NO_BKL_VFS").is_ok() {
        println!("cargo:rustc-cfg=kernel_no_bkl_vfs");
    }

    // `size` and `extreme-size` are indistinguishable via OPT_LEVEL (both "z");
    // the `extreme` feature is the discriminator. Mirrors the bin crate.
    let size_profile = std::env::var("OPT_LEVEL").as_deref() == Ok("z");
    if size_profile {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    if size_profile && std::env::var("CARGO_FEATURE_EXTREME").is_ok() {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }
}
