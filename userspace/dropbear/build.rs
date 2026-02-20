//! Build script for Dropbear SSH server
//!
//! Compiles the Dropbear C sources and packages the result.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let staging_dir = out_dir.join("staging");
    let dist_dir = manifest_dir.join("dist");

    println!("cargo:rerun-if-changed=dropbear");
    
    // In Phase 3, we will add the full list of Dropbear source files and configuration.
    // For now, we prepare the infrastructure.
    
    // Ensure staging area exists
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir).unwrap();
    }
    fs::create_dir_all(staging_dir.join("usr/bin")).unwrap();

    // Create the Tar Archive (The "Particular" Way)
    if !dist_dir.exists() {
        fs::create_dir_all(&dist_dir).unwrap();
    }
    
    let archive_path = dist_dir.join("dropbear.tar");
    
    // Note: This is a skeleton tar. Full packaging happens after compilation.
    let status = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("--no-xattrs")
        .arg("--format=ustar")
        .arg("-cf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&staging_dir)
        .arg("usr")
        .status()
        .expect("Failed to execute tar");

    if !status.success() {
        println!("cargo:warning=Tar creation failed (likely staging is empty)");
    }
    
    println!("cargo:warning=Dropbear infrastructure initialized.");
}
