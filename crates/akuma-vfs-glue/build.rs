fn main() {
    // Forward the bin's `smp-shared` feature as `kernel_smp_shared`, the same cfg
    // name every other crate uses.
    //
    // **This file is not optional.** `proc.rs`'s `active_core_count` is
    // `#[cfg(kernel_smp_shared)]` with a `1` fallback for the other arm, and it
    // is what sizes the per-core CPU-time accounting `/proc` reports. Without the
    // cfg the crate compiles the fallback even under real SMP, and `/proc`
    // quietly divides by one core on a four-core machine — no build error, no
    // runtime error, just wrong numbers. `akuma-exec` shipped exactly this bug
    // for its `kernel_profile_extreme` gates (see its build.rs header).
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }

    // `extreme-size`, detected exactly as `akuma-exec` does it: the `size` and
    // `extreme-size` profiles are the only ones at opt-level "z", and the
    // `extreme` feature discriminates between them.
    //
    // This exists to keep the size floor honest. `src/config.rs` forces
    // `PROC_SYSCALL_LOG_ENABLED` and `PROC_SYSVIPC_ENABLED` to `false` on this
    // profile, and while they were `const` the `/proc` renderers behind them were
    // const-folded out of the image entirely. Handing them over as runtime config
    // at `set_config` time turned both into loads, retained both renderers, and
    // cost **4 KB** of `extreme-size` `.text` — measured, 711 K -> 715 K, before
    // the cfg'd getters in lib.rs put it back.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    let size_profile = std::env::var("OPT_LEVEL").as_deref() == Ok("z");
    if size_profile && std::env::var("CARGO_FEATURE_EXTREME").is_ok() {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }
}
