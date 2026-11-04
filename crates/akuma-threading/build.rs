// Mirrors `akuma-exec` / `akuma-primitives`: emit `kernel_smp_shared` from the
// forwarded `smp-shared` feature and `kernel_profile_extreme` under the size
// profiles, so every `#[cfg(...)]` in this crate matches the rest of the tree
// instead of silently compiling the `not()` branch.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");
    println!("cargo::rustc-check-cfg=cfg(hooked_sched)");
    println!("cargo::rustc-check-cfg=cfg(sgi_sched)");

    // Which scheduler path this build compiles. `hooked_sched`: the cooperative switch
    // whose machine effects arrive through `X86ArchHooks` - always on x86_64, and on
    // aarch64 under the `hooked` feature (a tokio kernel, B21 A4). `sgi_sched`: the
    // AArch64 SGI scheduler, every other aarch64 build. A host `cargo test` on an aarch64
    // host gets `sgi_sched`, exactly the `target_arch = "aarch64"` it had before.
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_HOOKED");
    let hooked = arch == "x86_64" || std::env::var("CARGO_FEATURE_HOOKED").is_ok();
    if hooked {
        println!("cargo:rustc-cfg=hooked_sched");
    } else if arch == "aarch64" {
        println!("cargo:rustc-cfg=sgi_sched");
    }

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }

    let size_profile = std::env::var("OPT_LEVEL").as_deref() == Ok("z");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    if size_profile && std::env::var("CARGO_FEATURE_EXTREME").is_ok() {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }
}
