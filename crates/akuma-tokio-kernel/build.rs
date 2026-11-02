//! Pass the machine half's linker script on to the kernel binary.
//!
//! `DEP_<links>_<key>` reaches only a direct dependent, and the kernel depends on this crate, not on
//! the machine half: so the path is re-exported here, and the kernel's `build.rs` reads it as
//! `DEP_AKUMA_TOKIO_KERNEL_LINKER_SCRIPT`.

fn main() {
    if let Ok(path) = std::env::var("DEP_AKUMA_TOKIO_KERNEL_AMD64_LINKER_SCRIPT") {
        println!("cargo:linker_script={path}");
    }
}
