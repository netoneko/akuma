use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let vendor_dir = manifest_dir.join("vendor");
    let dash_src_dir = vendor_dir.join("dash-0.5.12");
    let target_bin_dir = manifest_dir.join("../../bootstrap/bin");

    // Ensure vendor directory exists
    if !vendor_dir.exists() {
        fs::create_dir_all(&vendor_dir).unwrap();
    }

    let tarball_path = vendor_dir.join("dash-0.5.12.tar.gz");
    if !tarball_path.exists() {
        println!("cargo:warning=Downloading dash-0.5.12.tar.gz using curl...");
        let status = Command::new("curl")
            .arg("-L") // Follow redirects
            .arg("http://gondor.apana.org.au/~herbert/dash/files/dash-0.5.12.tar.gz")
            .arg("-o") // Output to file
            .arg(&tarball_path)
            .status()
            .expect("Failed to execute curl. Is it installed?");

        if !status.success() {
            panic!("Failed to download dash-0.5.12.tar.gz");
        }
    } else {
        println!("cargo:warning=dash-0.5.12.tar.gz already exists, skipping download.");
    }

    // 2. Extract the tarball
    if !dash_src_dir.exists() {
        println!("cargo:warning=Extracting dash-0.5.12.tar.gz...");
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&tarball_path)
            .arg("-C")
            .arg(&vendor_dir)
            .status()
            .expect("Failed to execute tar. Is it installed?");

        if !status.success() {
            panic!("Failed to extract dash-0.5.12.tar.gz");
        }
    } else {
        println!("cargo:warning=dash-0.5.12 directory already exists, skipping extraction.");
    }

    // 3. Configure
    println!("cargo:warning=Configuring dash...");
    let status = Command::new("./configure")
        .current_dir(&dash_src_dir)
        .env("CC", "aarch64-linux-musl-gcc")
        .env("LDFLAGS", "-static") // Moved LDFLAGS here as per user instruction
        .arg("--host=aarch64-linux-musl")
        .arg("--enable-static")
        .arg("--disable-glob")
        .arg("--disable-test-workaround")
        .arg("--disable-lineno")
        .arg("--without-libedit")
        .status()
        .expect("Failed to execute configure. Check if aarch64-linux-musl-gcc is in PATH.");

    if !status.success() {
        panic!("Failed to configure dash.");
    }

    // 4. Compile
    println!("cargo:warning=Compiling dash...");
    let status = Command::new("make")
        .current_dir(&dash_src_dir)
        // LDFLAGS="-static" moved to configure step
        .status()
        .expect("Failed to execute dash. Is make installed?");

    if !status.success() {
        panic!("Failed to compile dash.");
    }

    // 5. Copy the compiled binary to bootstrap/bin
    let compiled_dash_path = dash_src_dir.join("src").join("dash");
    let final_dash_path = target_bin_dir.join("dash");

    // Ensure target directory exists
    if !target_bin_dir.exists() {
        println!("cargo:warning=Creating target directory: {}...", target_bin_dir.display());
        fs::create_dir_all(&target_bin_dir).expect(&format!("Failed to create directory: {}", target_bin_dir.display()));
    }

    // Verify source file exists before copying
    if !compiled_dash_path.exists() {
        panic!("Compiled dash binary NOT FOUND at expected location: {}", compiled_dash_path.display());
    } else {
        println!("cargo:warning=Compiled dash binary FOUND at: {}", compiled_dash_path.display());
    }

    println!("cargo:warning=Attempting to copy from {} to {}...", compiled_dash_path.display(), final_dash_path.display());
    match fs::copy(&compiled_dash_path, &final_dash_path) {
        Ok(_) => println!("cargo:warning=Successfully copied dash binary to {}", final_dash_path.display()),
        Err(e) => panic!("Failed to copy dash binary from {} to {}: {}", compiled_dash_path.display(), final_dash_path.display(), e),
    }

    println!("cargo:rerun-if-changed=build.rs"); // Rerun build if build script changes
}
