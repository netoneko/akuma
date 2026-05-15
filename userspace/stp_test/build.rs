use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let go_src = manifest_dir.join("go");
    let c_src = manifest_dir.join("c/main.c");
    let out_dir = manifest_dir.join("../../bootstrap/bin");

    println!("cargo:rerun-if-changed=go/main.go");
    println!("cargo:rerun-if-changed=go/stp_arm64.s");
    println!("cargo:rerun-if-changed=go/go.mod");
    println!("cargo:rerun-if-changed=c/main.c");
    println!("cargo:rerun-if-changed=build.rs");

    if !out_dir.exists() {
        fs::create_dir_all(&out_dir).expect("failed to create bootstrap/bin");
    }

    // Build Go binary
    println!("cargo:warning=Building stp_test Go binary...");
    let go_status = Command::new("go")
        .current_dir(&go_src)
        .env("GOOS", "linux")
        .env("GOARCH", "arm64")
        .env("CGO_ENABLED", "0")
        .args(["build", "-ldflags", "-s -w", "-o"])
        .arg(out_dir.join("stp_test_go"))
        .arg(".")
        .status()
        .expect("failed to run go build");
    if !go_status.success() {
        panic!("go build for stp_test failed");
    }
    println!("cargo:warning=stp_test Go binary built.");

    // Build C binary
    println!("cargo:warning=Building stp_test C binary...");
    let c_status = Command::new("aarch64-linux-musl-gcc")
        .args(["-static", "-O2", "-o"])
        .arg(out_dir.join("stp_test_c"))
        .arg(&c_src)
        .status()
        .expect("failed to run aarch64-linux-musl-gcc; is it in PATH?");
    if !c_status.success() {
        panic!("C build for stp_test failed");
    }
    println!("cargo:warning=stp_test C binary built.");
}
