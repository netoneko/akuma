fn main() {
    let script = std::env::var("DEP_AKUMA_TOKIO_KERNEL_LINKER_SCRIPT").expect("akuma-tokio-kernel exports a linker script");
    println!("cargo:rustc-link-arg-bins=-T{script}");
}
