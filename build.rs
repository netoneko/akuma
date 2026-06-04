fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_size)");

    // OPT_LEVEL is "z" only for profile.size (opt-level = "z").
    // PROFILE is always "release" for inherited profiles, so we can't use that.
    let size_profile = std::env::var("OPT_LEVEL").as_deref() == Ok("z");

    if size_profile {
        println!("cargo:rustc-cfg=kernel_profile_size");
    }

    // Pass STACK_BOTTOM to the linker script so the assertion stays tight per profile.
    // STACK_BOTTOM = KERNEL_PHYS_LOAD + IMAGE_SIZE (the boot stack starts right after
    // the reserved image region). The linker script uses PROVIDE(STACK_BOTTOM=...) as
    // a fallback that this --defsym overrides.
    let kernel_phys_load: usize = 0x4020_0000;
    // size profile is hand-tightened to 944 KB (page-aligned) to claw back the
    // ~80 KB that a fixed 1 MB reserve wasted above the ~914 KB kernel; release
    // keeps a roomy 3 MB. Must match IMAGE_SIZE in src/boot.rs.
    let image_size: usize = if size_profile { 0xEC000 } else { 0x30_0000 };
    let stack_bottom = kernel_phys_load + image_size;
    println!("cargo:rustc-link-arg=--defsym=STACK_BOTTOM={stack_bottom:#x}");
}
