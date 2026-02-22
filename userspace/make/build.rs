use std::env;
use std::fs;
use std::path::{PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor_dir = manifest_dir.join("vendor");
    let make_src_dir = vendor_dir.join("make-4.4");
    let target_bin_dir = manifest_dir.join("../../bootstrap/bin");
    let gmake_path = "/opt/homebrew/opt/make/libexec/gnubin/make";

    // Ensure vendor directory exists
    if !vendor_dir.exists() {
        fs::create_dir_all(&vendor_dir).unwrap();
    }

    // 1. Download make-4.4.tar.gz
    let tarball_path = vendor_dir.join("make-4.4.tar.gz");
    if !tarball_path.exists() {
        println!("cargo:warning=Downloading make-4.4.tar.gz using curl...");
        let status = Command::new("curl")
            .arg("-L")
            .arg("https://ftp.gnu.org/gnu/make/make-4.4.tar.gz")
            .arg("-o")
            .arg(&tarball_path)
            .status()
            .expect("Failed to execute curl");
        if !status.success() { panic!("Failed to download make"); }
    }

    // 2. Extract the tarball
    if !make_src_dir.exists() {
        println!("cargo:warning=Extracting make-4.4.tar.gz...");
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&tarball_path)
            .arg("-C")
            .arg(&vendor_dir)
            .status()
            .expect("Failed to execute tar");
        if !status.success() { panic!("Failed to extract make"); }
    }

    // 3. Configure
    println!("cargo:warning=Configuring make...");
    let ldflags = "-static -Wl,--entry=_start";
    let status = Command::new("./configure")
        .current_dir(&make_src_dir)
        .env("CC", "aarch64-linux-musl-gcc")
        .env("LDFLAGS", ldflags)
        .arg("--host=aarch64-linux-musl")
        .status()
        .expect("Failed to execute configure");

    if !status.success() {
        panic!("Failed to configure make.");
    }

    // 4. Compile with GNU Make if available
    println!("cargo:warning=Compiling make...");
    let make_cmd = if PathBuf::from(gmake_path).exists() { gmake_path } else { "make" };
    let status = Command::new(make_cmd)
        .current_dir(&make_src_dir)
        .arg("V=1")
        .arg(format!("LDFLAGS={}", ldflags))
        .status()
        .expect("Failed to execute make");

    if !status.success() {
        panic!("Failed to compile make.");
    }

    // 5. Copy the compiled binary to bootstrap/bin
    let compiled_make_path = make_src_dir.join("make");
    let final_make_path = target_bin_dir.join("make");

    if !target_bin_dir.exists() {
        fs::create_dir_all(&target_bin_dir).unwrap();
    }

    fs::copy(&compiled_make_path, &final_make_path)
        .expect(&format!("Failed to copy {} to {}", compiled_make_path.display(), final_make_path.display()));

    println!("cargo:warning=Successfully built and installed make to {}", final_make_path.display());
    println!("cargo:rerun-if-changed=build.rs");
}
