use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let crush_src_dir = manifest_dir.join("crush");
    let target_bin_dir = manifest_dir.join("../../bootstrap/bin");

    println!("cargo:rerun-if-changed=crush");
    println!("cargo:rerun-if-changed=build.rs");

    if !target_bin_dir.exists() {
        fs::create_dir_all(&target_bin_dir).expect("Failed to create bootstrap/bin");
    }

    println!("cargo:warning=Building crush with Go...");

    let status = Command::new("go")
        .current_dir(&crush_src_dir)
        .env("CC", "aarch64-linux-musl-gcc")
        .env("CGO_ENABLED", "1")
        .env("GOOS", "linux")
        .env("GOARCH", "arm64")
        .arg("build")
        .arg("-ldflags")
        .arg("-s -w -linkmode external -extldflags '-static'")
        .arg("-o")
        .arg(target_bin_dir.join("crush"))
        .arg(".")
        .status()
        .expect("Failed to execute go build");

    if !status.success() {
        panic!("Failed to build crush with Go");
    }

    println!("cargo:warning=Successfully built and installed crush to {}", target_bin_dir.join("crush").display());
}
