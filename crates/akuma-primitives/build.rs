// Emit `kernel_smp_shared` from the forwarded feature. This crate owns
// `PreemptGuard`, whose IRQ-masking half is conditional on `smp-shared`; without
// this build.rs the `#[cfg(kernel_smp_shared)]` here would silently compile the
// `not()` branch.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }
}
