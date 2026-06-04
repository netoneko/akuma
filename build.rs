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
    // The linker ASSERT (STACK_BOTTOM guardrail) lives in linker.ld; ensure edits
    // to it force a relink so the guardrail can't go stale behind a cache hit.
    println!("cargo:rerun-if-changed=linker.ld");

    if size_profile {
        println!("cargo:rustc-cfg=kernel_profile_size");
    }
    if extreme_profile {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }

    // Pass STACK_BOTTOM to the linker script so the assertion stays tight per profile.
    // STACK_BOTTOM = KERNEL_PHYS_LOAD + IMAGE_SIZE (the boot stack starts right after
    // the reserved image region). The linker script uses PROVIDE(STACK_BOTTOM=...) as
    // a fallback that this --defsym overrides.
    //
    // IMPORTANT: these IMAGE_SIZE values MUST stay in lockstep with IMAGE_SIZE in
    // src/boot.rs (it feeds both the ARM64 Image header and BOOT_STACK_TOP). The
    // linker ASSERT (_kernel_phys_end < STACK_BOTTOM) is the guardrail that fails
    // the build if the kernel outgrows the reserved region — bump the value here
    // AND in boot.rs together if that fires.
    let kernel_phys_load: usize = 0x4020_0000;
    // - extreme-size: the sc-* families are gated out, so the image is smaller than
    //   `size`; hand-tightened to 880 KB. This must cover `_kernel_phys_end`
    //   (LOAD .bin ≈ 807 KB PLUS ~46 KB of NOLOAD .bss boot page tables → end at
    //   ~853 KB), not just the .bin, leaving ~27 KB margin. The freed reservation
    //   (vs size's 944 KB) goes to the user-page pool, lowering the RAM floor. If
    //   the kernel grows past this the linker ASSERT fails — measure and bump.
    // - size: hand-tightened to 944 KB (page-aligned) over the ~914 KB kernel.
    // - release: a roomy 3 MB.
    let image_size: usize = if extreme_profile {
        0xDC000 // 880 KB
    } else if size_profile {
        0xEC000 // 944 KB
    } else {
        0x30_0000 // 3 MB
    };
    let stack_bottom = kernel_phys_load + image_size;
    println!("cargo:rustc-link-arg=--defsym=STACK_BOTTOM={stack_bottom:#x}");
}
