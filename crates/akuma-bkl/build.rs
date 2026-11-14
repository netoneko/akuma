fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_bkl_smp)");

    // Real (shared-kernel) SMP, forwarded from the bin's `smp-shared` feature.
    // Emits the same cfg name every other crate uses so the Big Kernel Lock code
    // is uniform across the tree.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BKL_SMP");
    if std::env::var("CARGO_FEATURE_BKL_SMP").is_ok() {
        println!("cargo:rustc-cfg=kernel_bkl_smp");
    }
}
