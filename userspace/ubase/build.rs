use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let ubase_src_dir = manifest_dir.join("ubase");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let staging_dir = out_dir.join("staging");
    let dist_dir = manifest_dir.join("dist");
    let bootstrap_archives = manifest_dir.join("../../bootstrap/archives");
    let gmake_path = "/opt/homebrew/opt/make/libexec/gnubin/make";
    let musl_dist = manifest_dir.join("../musl/dist");

    if !ubase_src_dir.exists() {
        panic!("ubase source not found at {}. Did you forget to initialize submodules?", ubase_src_dir.display());
    }

    // 1. Patch config.mk
    let config_mk_path = ubase_src_dir.join("config.mk");
    let config_content = fs::read_to_string(&config_mk_path).expect("Failed to read config.mk");
    
    let mut new_config = String::new();
    for line in config_content.lines() {
        if line.starts_with("CC =") {
            new_config.push_str("CC = aarch64-linux-musl-gcc\n");
        } else if line.starts_with("AR =") {
            new_config.push_str("AR = aarch64-linux-musl-ar\n");
        } else if line.starts_with("RANLIB =") {
            new_config.push_str("RANLIB = aarch64-linux-musl-ranlib\n");
        } else if line.starts_with("CPPFLAGS =") {
            new_config.push_str(&format!("CPPFLAGS = -D_DEFAULT_SOURCE -D_GNU_SOURCE -I{}\n", musl_dist.join("include").display()));
        } else if line.starts_with("LDFLAGS =") {
            new_config.push_str("LDFLAGS = -static -Wl,--entry=_start\n");
        } else {
            new_config.push_str(line);
            new_config.push('\n');
        }
    }
    fs::write(&config_mk_path, new_config).expect("Failed to write patched config.mk");

    // 2. Build
    println!("cargo:warning=Compiling ubase...");
    let make_cmd = if Path::new(gmake_path).exists() { gmake_path } else { "make" };
    let status = Command::new(make_cmd)
        .current_dir(&ubase_src_dir)
        .status()
        .expect("Failed to execute make");
    
    if !status.success() {
        println!("cargo:warning=ubase build finished with errors, some tools may be missing.");
    }

    // 3. Stage binaries
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir).unwrap();
    }
    fs::create_dir_all(staging_dir.join("usr/bin")).unwrap();

    if let Ok(entries) = fs::read_dir(&ubase_src_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let metadata = fs::metadata(&path).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.mode() & 0o111 != 0 && path.extension().is_none() {
                        let file_name = path.file_name().unwrap().to_str().unwrap();
                        if !file_name.contains('.') && file_name != "Makefile" && file_name != "config" {
                            let dest = staging_dir.join("usr/bin").join(file_name);
                            if let Err(e) = fs::copy(&path, &dest) {
                                println!("cargo:warning=Failed to copy {}: {}", file_name, e);
                            }
                        }
                    }
                }
            }
        }
    }

    // 4. Create Tar Package
    if !dist_dir.exists() {
        fs::create_dir_all(&dist_dir).unwrap();
    }
    let archive_path = dist_dir.join("ubase.tar");
    
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
        panic!("Tar creation failed");
    }
    
    // 5. Copy to bootstrap/archives
    if !bootstrap_archives.exists() {
        fs::create_dir_all(&bootstrap_archives).unwrap();
    }
    fs::copy(&archive_path, bootstrap_archives.join("ubase.tar")).expect("Failed to copy archive to bootstrap");

    println!("cargo:warning=Package created at {}", archive_path.display());
    println!("cargo:warning=Package copied to {}", bootstrap_archives.join("ubase.tar").display());
}
