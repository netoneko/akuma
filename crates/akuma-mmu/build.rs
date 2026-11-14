fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_bkl_smp)");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BKL_SMP");
    if std::env::var("CARGO_FEATURE_BKL_SMP").is_ok() {
        println!("cargo:rustc-cfg=kernel_bkl_smp");
    }
}
