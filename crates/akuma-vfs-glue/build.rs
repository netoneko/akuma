fn main() {
    // Forward the bin's `smp-shared` feature as `kernel_bkl_smp`, the same cfg
    // name every other crate uses.
    //
    // **This file is not optional.** `proc.rs`'s `active_core_count` is
    // `#[cfg(kernel_bkl_smp)]` with a `1` fallback for the other arm, and it
    // is what sizes the per-core CPU-time accounting `/proc` reports. Without the
    // cfg the crate compiles the fallback even under real SMP, and `/proc`
    // quietly divides by one core on a four-core machine — no build error, no
    // runtime error, just wrong numbers.
    println!("cargo::rustc-check-cfg=cfg(kernel_bkl_smp)");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BKL_SMP");
    if std::env::var("CARGO_FEATURE_BKL_SMP").is_ok() {
        println!("cargo:rustc-cfg=kernel_bkl_smp");
    }
}
