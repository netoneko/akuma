use std::env;
use std::fs;
use std::path::{PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor_dir = manifest_dir.join("vendor");
    let dash_src_dir = vendor_dir.join("dash-0.5.12");
    let target_bin_dir = manifest_dir.join("../../bootstrap/bin");
    let gmake_path = "/opt/homebrew/opt/make/libexec/gnubin/make";

    // 1. Ensure vendor directory exists
    if !vendor_dir.exists() {
        fs::create_dir_all(&vendor_dir).unwrap();
    }

    // 2. Clean and Extract
    if dash_src_dir.exists() {
        println!("cargo:warning=Cleaning existing dash source directory...");
        fs::remove_dir_all(&dash_src_dir).expect("Failed to remove existing source dir");
    }

    let tarball_path = vendor_dir.join("dash-0.5.12.tar.gz");
    if !tarball_path.exists() {
        println!("cargo:warning=Downloading dash-0.5.12.tar.gz...");
        let status = Command::new("curl")
            .arg("-L")
            .arg("http://gondor.apana.org.au/~herbert/dash/files/dash-0.5.12.tar.gz")
            .arg("-o")
            .arg(&tarball_path)
            .status()
            .expect("Failed to execute curl");
        if !status.success() { panic!("Failed to download dash"); }
    }

    println!("cargo:warning=Extracting dash-0.5.12.tar.gz...");
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball_path)
        .arg("-C")
        .arg(&vendor_dir)
        .status()
        .expect("Failed to execute tar");
    if !status.success() { panic!("Failed to extract dash"); }

    // 3. Patch configure to NOT override LDFLAGS
    let configure_path = dash_src_dir.join("configure");
    let configure_content = fs::read_to_string(&configure_path).expect("Failed to read configure");
    let old_ldflags = "export LDFLAGS=\"-static -Wl,--fatal-warnings\"";
    let new_ldflags = "export LDFLAGS=\"$LDFLAGS -static\"";
    
    if configure_content.contains(old_ldflags) {
        let patched_content = configure_content.replace(old_ldflags, new_ldflags);
        fs::write(&configure_path, patched_content).expect("Failed to write patched configure");
        println!("cargo:warning=Patched configure script to respect LDFLAGS.");
    }

    // 4. Configure
    println!("cargo:warning=Configuring dash...");
    let ldflags = "-static -Wl,--entry=_start";
    let status = Command::new("./configure")
        .current_dir(&dash_src_dir)
        .env("CC", "aarch64-linux-musl-gcc")
        .env("LDFLAGS", ldflags)
        .arg("--host=aarch64-linux-musl")
        .arg("--enable-static")
        .arg("--disable-glob")
        .arg("--disable-test-workaround")
        .arg("--disable-lineno")
        .arg("--without-libedit")
        .status()
        .expect("Failed to execute configure");
    
    if !status.success() {
        panic!("Failed to configure dash.");
    }

    // 5. Patch generated Makefiles to ensure our LDFLAGS are used
    for makefile_rel_path in &["Makefile", "src/Makefile"] {
        let makefile_path = dash_src_dir.join(makefile_rel_path);
        if makefile_path.exists() {
            println!("cargo:warning=Patching {}...", makefile_rel_path);
            let content = fs::read_to_string(&makefile_path).expect("Failed to read Makefile");
            let patched = content.lines().map(|line| {
                if line.starts_with("LDFLAGS =") {
                    format!("LDFLAGS = {}", ldflags)
                } else {
                    line.to_string()
                }
            }).collect::<Vec<_>>().join("\n");
            fs::write(&makefile_path, patched).expect("Failed to write patched Makefile");
        }
    }

    // 6. Compile with GNU Make if available
    println!("cargo:warning=Compiling dash with GNU Make...");
    let make_cmd = if PathBuf::from(gmake_path).exists() { gmake_path } else { "make" };
    let status = Command::new(make_cmd)
        .current_dir(&dash_src_dir)
        .arg("V=1")
        .arg(format!("LDFLAGS={}", ldflags))
        .status()
        .expect("Failed to execute make");
    
    if !status.success() {
        panic!("Failed to compile dash.");
    }

    // 7. Copy the compiled binary
    let compiled_dash_path = dash_src_dir.join("src").join("dash");
    let final_dash_path = target_bin_dir.join("dash");

    if !target_bin_dir.exists() {
        fs::create_dir_all(&target_bin_dir).expect("Failed to create bootstrap/bin");
    }

    fs::copy(&compiled_dash_path, &final_dash_path)
        .expect(&format!("Failed to copy {} to {}", compiled_dash_path.display(), final_dash_path.display()));

    println!("cargo:warning=Successfully built and installed dash to {}", final_dash_path.display());
    
    println!("cargo:rerun-if-changed=build.rs");
}
