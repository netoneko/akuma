use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let src_dir = manifest_dir.join("native-cli-ai");
    let target_bin_dir = manifest_dir.join("../../bootstrap/bin");

    if !src_dir.exists() {
        panic!(
            "native-cli-ai source not found at {}. Did you forget to initialize submodules?",
            src_dir.display()
        );
    }

    fs::create_dir_all(&target_bin_dir).expect("Failed to create bootstrap/bin");

    let num_jobs = std::thread::available_parallelism()
        .map(|n| n.get().to_string())
        .unwrap_or_else(|_| "4".to_string());

    // Override the upstream release profile with Akuma-tuned flags:
    //   - opt-level=3: speed over size (nca is I/O-bound but inference heavy)
    //   - lto=fat: full cross-crate inlining — the upstream uses "thin"
    //   - neon+fp16+dotprod: all SIMD extensions available on qemu-virt AArch64
    //   - static: link against musl statically, no dynamic loader on Akuma
    let rustflags = [
        "-C opt-level=3",
        "-C lto=fat",
        "-C codegen-units=1",
        "-C panic=abort",
        "-C overflow-checks=off",
        "-C target-feature=+neon,+fp16,+dotprod",
        "-C link-arg=-static",
    ]
    .join(" ");

    println!("cargo:warning=Building nca (native-cli-ai dev) for aarch64-unknown-linux-musl...");

    let status = Command::new("cargo")
        .current_dir(&src_dir)
        .args([
            "build",
            "--release",
            // Disable clipboard (arboard/X11/Wayland): nca runs over SSH on Akuma.
            "--no-default-features",
            "--target",
            "aarch64-unknown-linux-musl",
            "-p",
            "nca-cli",
            "-j",
            &num_jobs,
        ])
        // musl cross-compilation toolchain
        .env("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER", "aarch64-linux-musl-gcc")
        .env("CC_aarch64_unknown_linux_musl", "aarch64-linux-musl-gcc")
        .env("CXX_aarch64_unknown_linux_musl", "aarch64-linux-musl-g++")
        .env("AR_aarch64_unknown_linux_musl", "aarch64-linux-musl-ar")
        // CARGO_ENCODED_RUSTFLAGS is set by the outer cargo process and takes priority
        // over RUSTFLAGS. Unset it so our RUSTFLAGS are actually used, and so the
        // outer workspace's linker flags (-Tlinker.ld, max-page-size) don't bleed in.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env("RUSTFLAGS", &rustflags)
        .status()
        .expect("Failed to invoke cargo — is cargo installed?");

    if !status.success() {
        panic!("Failed to build nca");
    }

    let compiled = src_dir.join("target/aarch64-unknown-linux-musl/release/nca");
    if !compiled.exists() {
        panic!("nca binary not found at {}", compiled.display());
    }

    let dest = target_bin_dir.join("nca");
    fs::copy(&compiled, &dest).expect("Failed to copy nca binary");
    let _ = Command::new("aarch64-linux-musl-strip").arg(&dest).status();

    println!("cargo:warning=Installed nca to bootstrap/bin/nca");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=native-cli-ai/Cargo.toml");
    println!("cargo:rerun-if-changed=native-cli-ai/crates/cli/src/main.rs");
    println!("cargo:rerun-if-changed=native-cli-ai/crates/core/src/lib.rs");
}
