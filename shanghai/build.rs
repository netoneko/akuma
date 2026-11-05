//! Hand the kernel its linker arguments (the same two cases as `taoxichuan/build.rs`).
//!
//! amd64: the linker script `akuma-tokio-kernel` exports. aarch64: the workspace's `.cargo/config.toml`
//! already links every `aarch64-unknown-none` binary with `-Tlinker.ld`; it has no default for
//! `BOOT_STACK_SIZE`, so it is passed here (1 MiB, the main kernel's `release` value).

fn main() {
    if let Ok(script) = std::env::var("DEP_AKUMA_TOKIO_KERNEL_LINKER_SCRIPT") {
        println!("cargo:rustc-link-arg-bins=-T{script}");
    } else {
        assert!(
            std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64"),
            "akuma-tokio-kernel exports a linker script on amd64"
        );
        println!("cargo:rustc-link-arg-bins=--defsym=BOOT_STACK_SIZE={}", 1024 * 1024);
    }
}
