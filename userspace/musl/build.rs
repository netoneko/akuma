use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let musl_src = manifest_dir.join("musl");
    let install_dir = out_dir.join("install");
    let staging_dir = out_dir.join("staging");
    let dist_dir = manifest_dir.join("dist");

    // Rerun if musl source changes (simplified)
    println!("cargo:rerun-if-changed=musl/src");
    println!("cargo:rerun-if-changed=musl/include");
    println!("cargo:rerun-if-changed=musl/arch");

    // Ensure toolchain paths
    let llvm_ar = "/opt/homebrew/opt/llvm/bin/llvm-ar";
    let llvm_ranlib = "/opt/homebrew/opt/llvm/bin/llvm-ranlib";

    println!("cargo:warning=Building Musl libc from source...");

    let build_dir = out_dir.join("build");
    fs::create_dir_all(&build_dir).unwrap();

    // 1. Configure
    // Always re-configure if the build directory is fresh or if we want to be sure
    // musl configure is very fast.
    let status = Command::new(musl_src.join("configure"))
        .current_dir(&build_dir)
        .env("CC", "clang")
        .env("CFLAGS", "-target aarch64-linux-musl -Os")
        .env("AR", llvm_ar)
        .env("RANLIB", llvm_ranlib)
        .arg(format!("--prefix={}", install_dir.display()))
        .arg("--disable-shared")
        .arg("--disable-debug")
        .arg("--enable-optimize=s")
        .arg("--target=aarch64-linux-musl")
        .status()
        .expect("Failed to run musl configure");

    if !status.success() {
        panic!("Musl configure failed");
    }

    // Ensure install directory exists
    fs::create_dir_all(&install_dir).unwrap();

    // 2. Build and Install
    let status = Command::new("make")
        .current_dir(&build_dir)
        .env("CC", "clang")
        .env("CFLAGS", "-target aarch64-linux-musl -Os")
        .env("AR", llvm_ar)
        .env("RANLIB", llvm_ranlib)
        .arg("-j4")
        .arg("install")
        .status()
        .expect("Failed to run musl make");

    if !status.success() {
        panic!("Musl build/install failed");
    }

    // 3. Stage for Tar
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir).unwrap();
    }
    fs::create_dir_all(staging_dir.join("usr")).unwrap();

    // Copy lib and include to usr/
    let status = Command::new("cp")
        .arg("-r")
        .arg(install_dir.join("lib"))
        .arg(staging_dir.join("usr/"))
        .status()
        .expect("Failed to copy libs");
    if !status.success() { panic!("Copy libs failed"); }

    let status = Command::new("cp")
        .arg("-r")
        .arg(install_dir.join("include"))
        .arg(staging_dir.join("usr/"))
        .status()
        .expect("Failed to copy headers");
    if !status.success() { panic!("Copy headers failed"); }

    // 4. Create Tar
    if !dist_dir.exists() {
        fs::create_dir_all(&dist_dir).unwrap();
    }
    let tar_path = dist_dir.join("musl.tar");

    let status = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("--no-xattrs")
        .arg("--format=ustar")
        .arg("-cf")
        .arg(&tar_path)
        .arg("-C")
        .arg(&staging_dir)
        .arg("usr")
        .status()
        .expect("Failed to create musl.tar");

    if !status.success() {
        panic!("Tar creation failed");
    }

    println!("cargo:warning=Musl package created at {}", tar_path.display());
}
