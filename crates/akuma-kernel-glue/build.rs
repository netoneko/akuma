// Mirrors the subset of the root crate's `build.rs` that the modules moved
// here (`main.rs`'s body, `smp_shared.rs`, `console.rs`) still check. Cargo
// sets `CARGO_FEATURE_*` per-package from THIS package's own resolved
// features, so this crate needs its own build script (see
// `akuma-kernel-core/build.rs` for the same pattern, one layer down).
fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_network)");
    println!("cargo::rustc-check-cfg=cfg(kernel_bkl_profile)");
    println!("cargo::rustc-check-cfg=cfg(kernel_tests)");
    println!("cargo::rustc-check-cfg=cfg(kernel_console_lock)");
    println!("cargo::rustc-check-cfg=cfg(kernel_audio)");

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    let smp_shared = std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok();
    if smp_shared {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_BKL_NETWORK");
    if std::env::var("CARGO_FEATURE_NO_BKL_NETWORK").is_ok() {
        println!("cargo:rustc-cfg=kernel_no_bkl_network");
    }

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BKL_PROFILE");
    if std::env::var("CARGO_FEATURE_BKL_PROFILE").is_ok() {
        println!("cargo:rustc-cfg=kernel_bkl_profile");
    }

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_TESTS");
    let kernel_tests = std::env::var("CARGO_FEATURE_NO_TESTS").is_err()
        && std::env::var("OPT_LEVEL").as_deref() != Ok("z");
    if kernel_tests {
        println!("cargo:rustc-cfg=kernel_tests");
    }

    let size_opt = std::env::var("OPT_LEVEL").as_deref() == Ok("z");

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    let extreme_profile = size_opt && std::env::var("CARGO_FEATURE_EXTREME").is_ok();
    if extreme_profile {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }

    println!("cargo:rerun-if-env-changed=CONSOLE_LOCK");
    let console_lock_default_on = !size_opt;
    let console_lock = match std::env::var("CONSOLE_LOCK").as_deref() {
        Ok("0") => false,
        Ok("1") => true,
        _ => console_lock_default_on,
    };
    if console_lock {
        println!("cargo:rustc-cfg=kernel_console_lock");
    }

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_PLATFORM_FIRECRACKER");
    let firecracker = std::env::var("CARGO_FEATURE_PLATFORM_FIRECRACKER").is_ok();
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SOUND");
    if std::env::var("CARGO_FEATURE_SOUND").is_ok() && !firecracker {
        println!("cargo:rustc-cfg=kernel_audio");
    }
}
