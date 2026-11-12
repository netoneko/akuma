fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_bkl_smp)");
    println!("cargo::rustc-check-cfg=cfg(kernel_tests)");

    // Real (shared-kernel) SMP, forwarded from the bin's `smp-shared` feature
    // (smp-shared = ["akuma-exec/smp-shared"]). Emits the same cfg name the bin
    // crate uses so the Big Kernel Lock code is uniform across crates.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BKL_SMP");
    if std::env::var("CARGO_FEATURE_BKL_SMP").is_ok() {
        println!("cargo:rustc-cfg=kernel_bkl_smp");
    }

    // The bin crate's `no-tests` feature compiles the boot self-test surface out.
    // Absence of the feature = `kernel_tests` ON, mirroring the bin's build.rs and
    // `akuma-exceptions`. `pmm`'s forwarders need it: they are `cfg(kernel_tests)`
    // and their callers are the boot suite up in the binary.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_TESTS");
    if std::env::var("CARGO_FEATURE_NO_TESTS").is_err() {
        println!("cargo:rustc-cfg=kernel_tests");
    }
}
