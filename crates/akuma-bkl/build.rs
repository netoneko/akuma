fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");

    // Real (shared-kernel) SMP, forwarded from the bin's `smp-shared` feature.
    // Emits the same cfg name every other crate uses so the Big Kernel Lock code
    // is uniform across the tree.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }
}
