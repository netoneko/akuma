//! Hands the kernel its linker script, relayed by `akuma-jvm-kernel`
//! (`DEP_<links>_<key>` reaches only a direct dependent — this crate).
fn main() {
    let script = std::env::var("DEP_AKUMA_JVM_KERNEL_LINKER_SCRIPT")
        .expect("akuma-jvm-kernel exports a linker script");
    println!("cargo:rustc-link-arg-bins=-T{script}");
}
