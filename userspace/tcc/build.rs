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

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap();
    let host = env::var("HOST").unwrap();

    let musl_dist = manifest_dir.join("../musl/dist");
    if !musl_dist.exists() {
        panic!("Musl distribution not found at {}. Build musl package first.", musl_dist.display());
    }
    
    // 1. Build TCC compiler itself
    let mut build = cc::Build::new();
    build
        .define("TCC_TARGET_ARM64", "1")
        .define("ONE_SOURCE", "1")
        .define("CONFIG_TCC_STATIC", "1")
        .define("CONFIG_TCC_SEMLOCK", "0")
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .flag("-w")
        .include("tinycc")
        .include("tinycc/include")
        .include("src")
        .include(&musl_dist.join("include"))
        .target(&target)
        .host(&host);
    
    let opt_level_str = env::var("OPT_LEVEL").unwrap();
    let opt_level_num = match opt_level_str.as_str() {
        "s" | "z" => 3,
        _ => opt_level_str.parse().unwrap_or(0),
    };
    build.opt_level(opt_level_num)
        .out_dir(&out_dir);

    // Apply -Dmain=tcc_main only to compiler sources
    build
        .file("tinycc/tcc.c")
        .file("src/libc_stubs.c")
        .file("src/setjmp.S")
        .define("main", "tcc_main")
        .compile("tcc_all_objs");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=tcc_all_objs");

    // 2. Build runtime objects for the sysroot
    let mut sysroot_build = cc::Build::new();
    sysroot_build.target(&target).host(&host);
    let compiler = sysroot_build.get_compiler();
    
    let run_cc = |src: &str, obj: &str, extra_args: &[&str]| {
        let mut cmd = compiler.to_command();
        cmd.arg("-target").arg("aarch64-none-elf");
        cmd.arg("-ffreestanding").arg("-fno-builtin").arg("-nostdinc").arg("-O3");
        cmd.args(extra_args);
        cmd.arg("-c").arg(src).arg("-o").arg(out_dir.join(obj));
        let status = cmd.status().expect("Failed to run compiler");
        if !status.success() {
            panic!("Compiler failed for src: {}", src);
        }
    };

    // 2. Build TCC runtime objects
    run_cc("tinycc/lib/libtcc1.c", "libtcc1_base.o", &["-I", "tinycc", "-I", "tinycc/include", "-I", musl_dist.join("include").to_str().unwrap()]);
    run_cc("tinycc/lib/lib-arm64.c", "lib-arm64.o", &["-D__arm64_clear_cache=__clear_cache", "-I", "tinycc", "-I", "tinycc/include", "-I", musl_dist.join("include").to_str().unwrap()]);

    // Create archives manually
    let find_tool = |name: &str| {
        if Command::new(name).arg("--version").status().is_ok() {
            return Some(name.to_string());
        }
        let homebrew_path = format!("/opt/homebrew/opt/llvm/bin/{}", name);
        if Command::new(&homebrew_path).arg("--version").status().is_ok() {
            return Some(homebrew_path);
        }
        None
    };

    let ar_bin = find_tool("llvm-ar").unwrap_or_else(|| "ar".to_string());
    let ranlib_bin = find_tool("llvm-ranlib").unwrap_or_else(|| "ranlib".to_string());
    
    let ar_bin_clone = ar_bin.clone();
    let ranlib_bin_clone = ranlib_bin.clone();
    let run_ar = move |archive: &Path, objs: &[&Path]| {
        let mut cmd = Command::new(&ar_bin_clone);
        if ar_bin_clone.contains("llvm-ar") {
            cmd.arg("--format=gnu");
        }
        cmd.arg("rcs").arg(archive);
        for obj in objs {
            cmd.arg(obj);
        }
        let status = cmd.status().expect("Failed to run ar");
        if !status.success() {
            panic!("ar failed for archive: {:?}", archive);
        }

        let mut cmd = Command::new(&ranlib_bin_clone);
        cmd.arg(archive);
        let status = cmd.status().expect("Failed to run ranlib");
        if !status.success() {
            panic!("ranlib failed for archive: {:?}", archive);
        }
    };

    run_ar(&out_dir.join("libtcc1.a"), &[&out_dir.join("libtcc1_base.o"), &out_dir.join("lib-arm64.o")]);

    // 3. Stage the sysroot
    let staging_dir = out_dir.join("sysroot_staging");
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir).unwrap();
    }

    let lib_dir = staging_dir.join("usr/lib");
    let include_dir = staging_dir.join("usr/include");
    let tcc_dir = staging_dir.join("usr/lib/tcc");
    let tcc_include_dir = tcc_dir.join("include");

    fs::create_dir_all(&lib_dir).unwrap();
    fs::create_dir_all(&include_dir).unwrap();
    fs::create_dir_all(&tcc_dir).unwrap();
    fs::create_dir_all(&tcc_include_dir).unwrap();

    // Copy Musl artifacts
    // Copy all files from musl/dist/lib to usr/lib
    copy_dir_recursive(&musl_dist.join("lib"), &lib_dir).unwrap();
    
    // TCC specific runtime (after musl to ensure we might overwrite if needed, 
    // but usually libtcc1.a is unique to TCC)
    fs::copy(out_dir.join("libtcc1.a"), tcc_dir.join("libtcc1.a")).unwrap();

    // Headers from Musl (Standard POSIX) - Copy contents of include
    copy_dir_recursive(&musl_dist.join("include"), &include_dir).unwrap();
    // TCC specific internal headers
    copy_dir_recursive(Path::new("tinycc/include"), &tcc_include_dir).unwrap();

    // 4. Create the archive
    let archive_name = "libc.tar";
    let archive_path = out_dir.join(archive_name);

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
        panic!("tar command failed");
    }

    // 5. Create libtcc1.tar (standalone TCC runtime, installable without full sysroot)
    let libtcc1_staging = out_dir.join("libtcc1_staging");
    if libtcc1_staging.exists() {
        fs::remove_dir_all(&libtcc1_staging).unwrap();
    }
    let libtcc1_tcc_dir = libtcc1_staging.join("usr/lib/tcc");
    fs::create_dir_all(&libtcc1_tcc_dir).unwrap();
    fs::copy(out_dir.join("libtcc1.a"), libtcc1_tcc_dir.join("libtcc1.a")).unwrap();

    let libtcc1_archive_path = out_dir.join("libtcc1.tar");
    let status = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("--no-xattrs")
        .arg("--format=ustar")
        .arg("-cf")
        .arg(&libtcc1_archive_path)
        .arg("-C")
        .arg(&libtcc1_staging)
        .arg("usr")
        .status()
        .expect("Failed to execute tar");

    if !status.success() {
        panic!("tar command failed for libtcc1.tar");
    }

    // 6. Copy to dist directory
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dist_dir = manifest_dir.join("dist");
    fs::create_dir_all(&dist_dir).unwrap();
    fs::copy(&archive_path, dist_dir.join("libc.tar")).unwrap();
    fs::copy(&libtcc1_archive_path, dist_dir.join("libtcc1.tar")).unwrap();

    println!("cargo:warning=libc archive created at {}", dist_dir.join("libc.tar").display());
    println!("cargo:warning=libtcc1 archive created at {}", dist_dir.join("libtcc1.tar").display());
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
