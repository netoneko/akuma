// Forwards the `no-tests` opt-out as `cfg(kernel_tests)`, the same way the other crates do
// (`akuma-kernel-core/build.rs`): Cargo sets `CARGO_FEATURE_*` per package, so a crate that
// reads the cfg needs its own script. Here it gates the boot self-tests' validation bypass:
// an image built with `--features no-tests` has no bypass code at all.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_tests)");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_TESTS");
    if std::env::var("CARGO_FEATURE_NO_TESTS").is_err() {
        println!("cargo:rustc-cfg=kernel_tests");
    }
}
