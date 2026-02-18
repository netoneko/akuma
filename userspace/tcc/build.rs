use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=tinycc/tcc.c");
    println!("cargo:rerun-if-changed=tinycc/libtcc.c");
    println!("cargo:rerun-if-changed=src/libc_stubs.c");
    println!("cargo:rerun-if-changed=src/setjmp.S");
    println!("cargo:rerun-if-changed=src/config.h");
    println!("cargo:rerun-if-changed=lib/crt0.S");
    println!("cargo:rerun-if-changed=lib/libc.c");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap(); // e.g., aarch64-unknown-none
    
    // 1. Build TCC compiler itself
    let mut build = cc::Build::new();
    build
        .define("TCC_TARGET_ARM64", "1")
        .define("ONE_SOURCE", "1")
        .define("CONFIG_TCC_STATIC", "1")
        .define("CONFIG_TCC_SEMLOCK", "0")
        .define("time_t", "long long")
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .flag("-w")
        .include("tinycc")
        .include("src")
        .include("include")
        .target(&target)
        .host(&env::var("HOST").unwrap());
    
    let opt_level_str = env::var("OPT_LEVEL").unwrap();
    let opt_level_num = match opt_level_str.as_str() {
        "s" | "z" => 3,
        _ => opt_level_str.parse().unwrap_or(0),
    };
    build.opt_level(opt_level_num)
        .out_dir(&out_dir);

    build
        .file("tinycc/tcc.c")
        .file("src/libc_stubs.c")
        .file("src/setjmp.S")
        .define("main", "tcc_main")
        .compile("tcc_all_objs");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=tcc_all_objs");

    // 2. Build runtime objects for the sysroot
    // We compile crt0.o and libc.o separately
    
    // libc.o
    cc::Build::new()
        .file("lib/libc.c")
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .include("include")
        .target(&target)
        .host(&env::var("HOST").unwrap())
        .opt_level(3)
        .out_dir(&out_dir)
        .compile("akuma_libc"); // This creates libakuma_libc.a

    // For crt0.o, we might need a direct command if we want exactly crt0.o
    // But we can also use cc crate to build it.
    cc::Build::new()
        .file("lib/crt0.S")
        .target(&target)
        .host(&env::var("HOST").unwrap())
        .out_dir(&out_dir)
        .compile("crt0"); // This creates libcrt0.a

    // 3. Stage the sysroot
    let staging_dir = out_dir.join("sysroot_staging");
    let lib_dest_dir = staging_dir.join("lib");
    let include_dest_dir = staging_dir.join("include");

    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir).unwrap();
    }
    fs::create_dir_all(&lib_dest_dir).unwrap();
    fs::create_dir_all(&include_dest_dir).unwrap();

    // Copy runtime libraries
    // We rename them to standard names if needed, or just copy the .a files
    fs::copy(out_dir.join("libakuma_libc.a"), lib_dest_dir.join("libc.a")).unwrap();
    fs::copy(out_dir.join("libcrt0.a"), lib_dest_dir.join("crt0.a")).unwrap();
    
    // Also copy as .o if TCC prefers that (it often does for crt0)
    // The cc crate keeps the .o files in the out_dir
    // Finding them might be tricky, let's just use the .a for now or try to find them.
    // Actually, let's just copy the .a as .o if we really want to be sure.
    // TCC can link .a files.
    
    // Copy headers from userspace/tcc/include
    copy_dir_recursive(Path::new("include"), &include_dest_dir).unwrap();

    // 4. Create the archive
    let archive_name = "libc.tar.gz";
    let archive_path = out_dir.join(archive_name);

    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&staging_dir)
        .arg(".")
        .status()
        .expect("Failed to execute tar");

    if !status.success() {
        panic!("tar command failed");
    }

    // 5. Copy to dist directory
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dist_dir = manifest_dir.join("dist");
    fs::create_dir_all(&dist_dir).unwrap();
    fs::copy(&archive_path, dist_dir.join(archive_name)).unwrap();

    println!("cargo:warning=libc archive created at {}", dist_dir.join(archive_name).display());
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !dst.exists() {
        fs::create_dir_all(dst)?;
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}
