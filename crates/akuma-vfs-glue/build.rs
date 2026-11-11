fn main() {
    // Forward the bin's `smp-shared` feature as `kernel_smp_shared`, the same cfg
    // name every other crate uses.
    //
    // **This file is not optional.** `proc.rs`'s `active_core_count` is
    // `#[cfg(kernel_smp_shared)]` with a `1` fallback for the other arm, and it
    // is what sizes the per-core CPU-time accounting `/proc` reports. Without the
    // cfg the crate compiles the fallback even under real SMP, and `/proc`
    // quietly divides by one core on a four-core machine — no build error, no
    // runtime error, just wrong numbers.
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }
}
