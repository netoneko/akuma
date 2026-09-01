fn main() {
    // **This file is load-bearing and its absence is silent.**
    //
    // Sixteen consts here are `#[cfg(kernel_profile_extreme)]` / `#[cfg(not(..))]`
    // pairs — `MAX_ARG_STRLEN` is 128 KiB or 4 KiB, `PROC_SYSCALL_LOG_ENABLED` is
    // true or false, and so on. Cfgs do not travel with code: a crate carved out
    // of `src/` inherits every `kernel_*` cfg its source reads and receives none
    // of them. Without this forwarding, every one of those pairs compiles the
    // *non*-extreme arm even in an `extreme-size` build, with no build error and
    // no runtime error — just the wrong numbers, everywhere, in the profile whose
    // entire purpose is being small.
    //
    // Detected exactly as `akuma-exec` does it: `size` and `extreme-size` are the
    // only profiles at opt-level "z", and the `extreme` feature discriminates.
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    let size_profile = std::env::var("OPT_LEVEL").as_deref() == Ok("z");
    if size_profile && std::env::var("CARGO_FEATURE_EXTREME").is_ok() {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }
}
