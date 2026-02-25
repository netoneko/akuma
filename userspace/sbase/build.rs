use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let sbase_src_dir = manifest_dir.join("sbase");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let staging_dir = out_dir.join("staging");
    let dist_dir = manifest_dir.join("dist");
    let bootstrap_archives = manifest_dir.join("../../bootstrap/archives");
    let gmake_path = "/opt/homebrew/opt/make/libexec/gnubin/make";
    let musl_dist = manifest_dir.join("../musl/dist");

    if !sbase_src_dir.exists() {
        panic!("sbase source not found at {}. Did you forget to initialize submodules?", sbase_src_dir.display());
    }

    // 1. Patch config.mk
    let config_mk_path = sbase_src_dir.join("config.mk");
    let config_content = fs::read_to_string(&config_mk_path).expect("Failed to read config.mk");
    
    let mut new_config = String::new();
    let mut seen_cc = false;
    let mut seen_ar = false;
    let mut seen_ranlib = false;
    let mut seen_ldflags = false;
    let mut seen_cppflags = false;
    for line in config_content.lines() {
        let trimmed = line.trim_start_matches('#').trim();
        if line.starts_with("CC =") || trimmed.starts_with("CC =") {
            if !seen_cc {
                new_config.push_str("CC = aarch64-linux-musl-gcc\n");
                seen_cc = true;
            }
        } else if line.starts_with("AR =") || trimmed.starts_with("AR =") {
            if !seen_ar {
                new_config.push_str("AR = aarch64-linux-musl-ar\n");
                seen_ar = true;
            }
        } else if line.starts_with("RANLIB =") || trimmed.starts_with("RANLIB =") {
            if !seen_ranlib {
                new_config.push_str("RANLIB = aarch64-linux-musl-ranlib\n");
                seen_ranlib = true;
            }
        } else if line.starts_with("ARFLAGS =") || trimmed.starts_with("ARFLAGS =") {
            // skip duplicates, will be added with AR
        } else if line.starts_with("# tools") || line.starts_with("#tools") {
            new_config.push_str(line);
            new_config.push('\n');
            if !seen_ar {
                new_config.push_str("AR = aarch64-linux-musl-ar\n");
                new_config.push_str("ARFLAGS = rcs\n");
                new_config.push_str("RANLIB = aarch64-linux-musl-ranlib\n");
                seen_ar = true;
                seen_ranlib = true;
            }
        } else if line.starts_with("CPPFLAGS =") || trimmed.starts_with("CPPFLAGS =") || trimmed.starts_with("CFLAGS =") {
            if !seen_cppflags {
                new_config.push_str(&format!("CPPFLAGS = -D_DEFAULT_SOURCE -D_GNU_SOURCE -I{}\n", musl_dist.join("include").display()));
                seen_cppflags = true;
            }
        } else if line.starts_with("LDFLAGS =") || trimmed.starts_with("LDFLAGS =") {
            if !seen_ldflags {
                new_config.push_str("LDFLAGS = -static\n");
                seen_ldflags = true;
            }
        } else {
            new_config.push_str(line);
            new_config.push('\n');
        }
    }
    if !seen_ldflags {
        new_config.push_str("LDFLAGS = -static\n");
    }
    fs::write(&config_mk_path, new_config).expect("Failed to write patched config.mk");

    // 2. Patch Makefile to remove problematic tools and host build steps
    let makefile_path = sbase_src_dir.join("Makefile");
    let makefile_content = fs::read_to_string(&makefile_path).expect("Failed to read Makefile");
    let patched_makefile = makefile_content
        .replace("\tbc\\\n", "")
        .replace("\tdc\\\n", "")
        .replace("all: scripts/make", "all:")
        .replace("\t+@$(SMAKE) $(BIN)", "\t$(MAKE) $(BIN)");
    fs::write(&makefile_path, patched_makefile).expect("Failed to write patched Makefile");

    // 3. Build
    println!("cargo:warning=Compiling sbase...");
    let make_cmd = if Path::new(gmake_path).exists() { gmake_path } else { "make" };
    
    // Clean thoroughly
    let _ = Command::new("find")
        .current_dir(&sbase_src_dir)
        .arg(".")
        .arg("-name")
        .arg("*.o")
        .arg("-delete")
        .status();
    
    let _ = Command::new(make_cmd)
        .current_dir(&sbase_src_dir)
        .arg("clean")
        .status();

    let status = Command::new(make_cmd)
        .current_dir(&sbase_src_dir)
        .arg("-k") // Keep going on errors
        .env("CC", "aarch64-linux-musl-gcc")
        .env("AR", "aarch64-linux-musl-ar")
        .env("RANLIB", "aarch64-linux-musl-ranlib")
        .env("LDFLAGS", "-static")
        .status()
        .expect("Failed to execute make");
    
    if !status.success() {
        println!("cargo:warning=sbase build finished with errors, some tools may be missing.");
    }

    // 3. Stage binaries
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir).unwrap();
    }
    fs::create_dir_all(staging_dir.join("usr/bin")).unwrap();

    if let Ok(entries) = fs::read_dir(&sbase_src_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let metadata = fs::metadata(&path).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.mode() & 0o111 != 0 && path.extension().is_none() {
                        let file_name = path.file_name().unwrap().to_str().unwrap();
                        if !file_name.contains('.') && file_name != "Makefile" && file_name != "config" && file_name != "util" {
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
    let archive_path = dist_dir.join("sbase.tar");
    
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
    fs::copy(&archive_path, bootstrap_archives.join("sbase.tar")).expect("Failed to copy archive to bootstrap");

    println!("cargo:warning=Package created at {}", archive_path.display());
    println!("cargo:warning=Package copied to {}", bootstrap_archives.join("sbase.tar").display());
}
