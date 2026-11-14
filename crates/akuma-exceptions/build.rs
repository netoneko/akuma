fn main() {
    // Mirror the bin crate's cfg names; the same feature-forwarding scheme
    // `akuma-exec` uses (see its build.rs). Every cfg this crate gates on must
    // be declared here, or rustc's `unexpected_cfgs` lint fires.
    println!("cargo::rustc-check-cfg=cfg(kernel_tests)");

    // The bin crate's `no-tests` feature compiles the boot self-test surface
    // out. Absence of the feature = `kernel_tests` ON, exactly mirroring the
    // bin crate's build.rs (the devbox passes `no-tests`; the default build does
    // not).
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_TESTS");
    if std::env::var("CARGO_FEATURE_NO_TESTS").is_err() {
        println!("cargo:rustc-cfg=kernel_tests");
    }
}
