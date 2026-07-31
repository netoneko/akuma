fn main() {
    // Mirror the bin crate + akuma-net: emit `kernel_profile_size` when built
    // under the `size` Cargo profile (which is the only profile with
    // opt-level = "z"). Without this, every `#[cfg(kernel_profile_size)]` gate
    // in this crate silently compiled the `not()` branch even on the size
    // profile — so the demand-paged ELF loader, the page-by-page interpreter
    // loader, and `HEAP_SLURP_MAX = 0` were all DORMANT, and the size kernel
    // still slurped whole binaries (e.g. the 723 KB tcc) into the kernel heap.
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_size)");
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_process)");

    // BKL-free fork page-copy (Phase 3), forwarded from the bin's `no-bkl-process`
    // feature. Unlike `no-bkl-network`/`no-bkl-vfs` — whose guards live in the bin
    // crate — `ProcessBklGuard` is constructed inside `fork_process`, so THIS crate
    // needs the cfg. See `process/bkl_guard.rs`.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_BKL_PROCESS");
    if std::env::var("CARGO_FEATURE_NO_BKL_PROCESS").is_ok() {
        println!("cargo:rustc-cfg=kernel_no_bkl_process");
    }

    // Real (shared-kernel) SMP, forwarded from the bin's `smp-shared` feature
    // (smp-shared = ["akuma-exec/smp-shared"]). Emits the same cfg name the bin
    // crate uses so the Big Kernel Lock code is uniform across crates.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }
    let size_profile = std::env::var("OPT_LEVEL").as_deref() == Ok("z");
    if size_profile {
        println!("cargo:rustc-cfg=kernel_profile_size");
    }
    // `size` and `extreme-size` are indistinguishable via OPT_LEVEL (both "z");
    // the `extreme` feature (forwarded from the bin's extreme = ["akuma-exec/extreme"])
    // is the discriminator. Mirrors the bin crate's build.rs.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    if size_profile && std::env::var("CARGO_FEATURE_EXTREME").is_ok() {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }
}
