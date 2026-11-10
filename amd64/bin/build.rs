//! Hand the kernel binary its linker script.
//!
//! `akuma-kernel-amd64` owns `amd64/linker.ld` and exports its path as `links` metadata
//! (`DEP_AKUMA_KERNEL_AMD64_LINKER_SCRIPT`), because a dependency's `rustc-link-arg` never
//! reaches the binary that depends on it. `rustc-link-arg-bins` scopes it to this package's bin.

fn main() {
    let script = std::env::var("DEP_AKUMA_KERNEL_AMD64_LINKER_SCRIPT")
        .expect("akuma-kernel-amd64 exports its linker script");
    println!("cargo:rustc-link-arg-bins=-T{script}");
}
