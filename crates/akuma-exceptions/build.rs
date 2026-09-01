fn main() {
    // Mirror the bin crate's cfg names; the same feature-forwarding scheme
    // `akuma-exec` uses (see its build.rs). Every cfg this crate gates on must
    // be declared here, or rustc's `unexpected_cfgs` lint fires.
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_irq)");
    println!("cargo::rustc-check-cfg=cfg(kernel_tests)");

    // Real (shared-kernel) SMP, forwarded from the bin's `smp-shared` feature.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }

    // BKL-free timer-IRQ dispatch (Phase 7a), forwarded from the bin's
    // `no-bkl-irq` feature. Emitted independently of smp-shared — same scheme
    // as the bin crate — so the guard can compile-check in either combination.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_BKL_IRQ");
    if std::env::var("CARGO_FEATURE_NO_BKL_IRQ").is_ok() {
        println!("cargo:rustc-cfg=kernel_no_bkl_irq");
    }

    // The bin crate's `no-tests` feature compiles the boot self-test surface
    // out. Absence of the feature = `kernel_tests` ON, exactly mirroring the
    // bin crate's build.rs (extreme-size/devbox pass `no-tests`; the default
    // build does not).
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_TESTS");
    if std::env::var("CARGO_FEATURE_NO_TESTS").is_err() {
        println!("cargo:rustc-cfg=kernel_tests");
    }
}
