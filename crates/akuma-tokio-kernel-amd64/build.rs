//! Export the linker script's path to the crate that links the kernel binary.
//!
//! A dependency's `rustc-link-arg` never reaches a binary that depends on it, so the path travels as
//! `links` metadata instead: `akuma-tokio-kernel`'s build script reads it as
//! `DEP_AKUMA_TOKIO_KERNEL_AMD64_LINKER_SCRIPT` and passes it on, and the kernel's own `build.rs` hands
//! it to the linker (`rustc-link-arg-bins`).

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:linker_script={dir}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");
    // The boot assembly lives in `akuma-boot-amd64`; its output feeds the kernel's link.
    println!("cargo:rerun-if-changed=../akuma-boot-amd64/src/boot.s");
}
