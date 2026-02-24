use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor_dir = manifest_dir.join("vendor");
    let ubase_src_dir = vendor_dir.join("ubase-master");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let staging_dir = out_dir.join("staging");
    let dist_dir = manifest_dir.join("dist");
    let gmake_path = "/opt/homebrew/opt/make/libexec/gnubin/make";
    let musl_dist = manifest_dir.join("../musl/dist");

    // 1. Ensure vendor directory exists
    if !vendor_dir.exists() {
        fs::create_dir_all(&vendor_dir).unwrap();
    }

    // 2. Download and Extract
    let tarball_path = vendor_dir.join("ubase-master.tar.gz");
    if !tarball_path.exists() {
        println!("cargo:warning=Downloading ubase-master.tar.gz...");
        let status = Command::new("curl")
            .arg("-L")
            .arg("https://git.suckless.org/ubase/snapshot/ubase-master.tar.gz")
            .arg("-o")
            .arg(&tarball_path)
            .status()
            .expect("Failed to execute curl");
        if !status.success() { panic!("Failed to download ubase"); }
    }

    if ubase_src_dir.exists() {
        fs::remove_dir_all(&ubase_src_dir).unwrap();
    }

    println!("cargo:warning=Extracting ubase-master.tar.gz...");
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball_path)
        .arg("-C")
        .arg(&vendor_dir)
        .status()
        .expect("Failed to execute tar");
    if !status.success() { panic!("Failed to extract ubase"); }

    // 3. Patch config.mk
    let config_mk_path = ubase_src_dir.join("config.mk");
    let config_content = fs::read_to_string(&config_mk_path).expect("Failed to read config.mk");
    
    // We want to override CC, AR, RANLIB, and add our Musl includes
    let mut new_config = String::new();
    for line in config_content.lines() {
        if line.starts_with("CC =") {
            new_config.push_str("CC = aarch64-linux-musl-gcc
");
        } else if line.starts_with("AR =") {
            new_config.push_str("AR = aarch64-linux-musl-ar
");
        } else if line.starts_with("RANLIB =") {
            new_config.push_str("RANLIB = aarch64-linux-musl-ranlib
");
        } else if line.starts_with("CPPFLAGS =") {
            new_config.push_str(&format!("CPPFLAGS = -D_DEFAULT_SOURCE -D_GNU_SOURCE -I{}
", musl_dist.join("include").display()));
        } else if line.starts_with("LDFLAGS =") {
            new_config.push_str("LDFLAGS = -static -Wl,--entry=_start
");
        } else {
            new_config.push_str(line);
            new_config.push('
');
        }
    }
    fs::write(&config_mk_path, new_config).expect("Failed to write patched config.mk");

    // 4. Build
    println!("cargo:warning=Compiling ubase...");
    let make_cmd = if Path::new(gmake_path).exists() { gmake_path } else { "make" };
    let status = Command::new(make_cmd)
        .current_dir(&ubase_src_dir)
        .status()
        .expect("Failed to execute make");
    
    if !status.success() {
        // Some tools might fail due to missing Linux-specific headers in our minimal setup
        // For now, we'll continue if some succeed, but ubase's Makefile usually stops on error.
        // We might need to patch the Makefile to ignore errors or only build specific tools.
        println!("cargo:warning=ubase build finished with errors, some tools may be missing.");
    }

    // 5. Stage binaries
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir).unwrap();
    }
    fs::create_dir_all(staging_dir.join("usr/bin")).unwrap();

    // List of tools from ubase source (we'll try to copy all executables)
    if let Ok(entries) = fs::read_dir(&ubase_src_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let metadata = fs::metadata(&path).unwrap();
                // Check if it's an executable (simplified check for Unix)
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.mode() & 0o111 != 0 && path.extension().is_none() {
                        let file_name = path.file_name().unwrap().to_str().unwrap();
                        // Avoid copying source files, Makefiles, etc. (though extension check helps)
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

    // 6. Create Tar Package
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
    
    println!("cargo:warning=Package created at {}", archive_path.display());
}
