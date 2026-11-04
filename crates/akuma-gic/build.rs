fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");
    println!("cargo::rustc-check-cfg=cfg(gic_per_core)");
    // The per-core entry points: under `smp-shared`, or alone (`secondary`).
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SECONDARY");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() || std::env::var("CARGO_FEATURE_SECONDARY").is_ok() {
        println!("cargo:rustc-cfg=gic_per_core");
    }

    // Real (shared-kernel) SMP, forwarded from the bin's `smp-shared` feature.
    // Same spelling every other crate uses, so the SMP-only GIC entry points
    // compile in exactly when the rest of the SMP tree does.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }
}
