fn main() {
    // Mirror the bin crate + akuma-net: emit `kernel_profile_size` when built
    // under the `size` Cargo profile (which is the only profile with
    // opt-level = "z"). Without this, every `#[cfg(kernel_profile_size)]` gate
    // in this crate silently compiled the `not()` branch even on the size
    // profile — so the demand-paged ELF loader, the page-by-page interpreter
    // loader, and `HEAP_SLURP_MAX = 0` were all DORMANT, and the size kernel
    // still slurped whole binaries (e.g. the 723 KB tcc) into the kernel heap.
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_size)");
    if std::env::var("OPT_LEVEL").as_deref() == Ok("z") {
        println!("cargo:rustc-cfg=kernel_profile_size");
    }
}
