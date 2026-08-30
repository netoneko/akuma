fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");

    // `size` and `extreme-size` are indistinguishable via OPT_LEVEL (both "z");
    // the `extreme` feature (forwarded from the bin's `extreme`) is the
    // discriminator. Mirrors `akuma-exec`'s build.rs — and the reason that one
    // exists at all: without it, every `#[cfg(kernel_profile_extreme)]` gate
    // silently compiled the `not()` branch even on the size profile, leaving the
    // demand-paged ELF loader DORMANT.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    let size_profile = std::env::var("OPT_LEVEL").as_deref() == Ok("z");
    if size_profile {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }
}
