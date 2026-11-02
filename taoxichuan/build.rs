//! Hand the kernel its linker script, which `akuma-tokio-kernel` exports (`rustc-link-arg-bins` is
//! scoped to this package's binary, so no other crate built for the target sees it).

fn main() {
    let script = std::env::var("DEP_AKUMA_TOKIO_KERNEL_LINKER_SCRIPT").expect("akuma-tokio-kernel exports a linker script");
    println!("cargo:rustc-link-arg-bins=-T{script}");
}
