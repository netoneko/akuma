fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_size)");
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");

    // OPT_LEVEL is "z" only for profile.size / profile.extreme-size (opt-level = "z").
    // PROFILE is always "release" for inherited profiles, so we can't use that.
    let size_profile = std::env::var("OPT_LEVEL").as_deref() == Ok("z");

    // `extreme-size` and `size` are indistinguishable via OPT_LEVEL (both "z"), so
    // the `extreme` Cargo feature (set only by build_extreme_size.sh) is the
    // discriminator. Cargo exposes it to build scripts as CARGO_FEATURE_EXTREME.
    let extreme_profile = size_profile && std::env::var("CARGO_FEATURE_EXTREME").is_ok();
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    // linker.ld now derives the boot-stack reservation (STACK_BOTTOM / STACK_TOP /
    // IMAGE_RESERVE) from the actual linked image size, so there is no longer a
    // per-profile IMAGE_SIZE here nor a --defsym=STACK_BOTTOM to inject. Still
    // rerun if the linker script changes so the derivation can't go stale behind a
    // cache hit.
    println!("cargo:rerun-if-changed=linker.ld");

    if size_profile {
        println!("cargo:rustc-cfg=kernel_profile_size");
    }
    if extreme_profile {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }

    // Boot-stack size, injected into linker.ld as the BOOT_STACK_SIZE symbol.
    // ALWAYS passed (linker.ld has no PROVIDE default — a PROVIDE would override
    // the defsym under LLD, the historical STACK_BOTTOM no-op bug).
    //   release/size: 1 MB — the boot test suite runs deep on thread 0.
    //   extreme:      32 KB — no test suite (no-tests); thread 0's measured stack
    //                 high-water is ~10 KB (docs/EXTREME_STACK_TRIMMING.md), and
    //                 its exception stack is a separate PMM allocation. Reclaims
    //                 ~992 KB to the user-page pool (≈17% of RAM at the 4.5 MB
    //                 floor). config::KERNEL_STACK_SIZE is NOT used for the boot
    //                 stack bounds (main.rs derives them from STACK_TOP/BOTTOM).
    let boot_stack_size: usize = if extreme_profile { 32 * 1024 } else { 1024 * 1024 };
    println!("cargo:rustc-link-arg=--defsym=BOOT_STACK_SIZE={boot_stack_size}");
}
