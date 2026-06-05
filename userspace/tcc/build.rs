use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// musl HEADERS come from the Alpine apk, not an in-tree musl build. We only need
// the headers to cross-compile tcc here; the libc/crt that compiled programs link
// against is supplied on Akuma by `apk add musl-dev` (same package, same version).
// Pinned like apk-tools' downloads — bump in lockstep with the Akuma sysroot.
const MUSL_DEV_URL: &str =
    "https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/aarch64/musl-dev-1.2.5-r23.apk";

fn download_if_missing(url: &str, dest: &Path) {
    if dest.exists() {
        return;
    }
    let name = dest.file_name().unwrap().to_str().unwrap();
    println!("cargo:warning=Downloading {}...", name);
    let status = Command::new("curl")
        .arg("-L")
        .arg(url)
        .arg("-o")
        .arg(dest)
        .status()
        .expect("Failed to execute curl");
    if !status.success() {
        panic!("Failed to download {}", url);
    }
}

/// Extract a single path (e.g. `usr/include`) out of an Alpine `.apk` (a gzipped,
/// multi-segment tar) into `dest`.
fn extract_apk_path(apk: &Path, dest: &Path, path: &str) {
    let status = Command::new("tar")
        .arg("xzf")
        .arg(apk)
        .arg("-C")
        .arg(dest)
        .arg(path)
        .status()
        .expect("Failed to run tar on apk");
    if !status.success() {
        panic!("Failed to extract '{}' from {}", path, apk.display());
    }
}

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

    // ── musl headers from apk ───────────────────────────────────────────────
    let vendor_dir = manifest_dir.join("vendor");
    fs::create_dir_all(&vendor_dir).unwrap();
    let musl_apk = vendor_dir.join("musl-dev.apk");
    download_if_missing(MUSL_DEV_URL, &musl_apk);

    let musl_sysroot = out_dir.join("musl-sysroot");
    let _ = fs::remove_dir_all(&musl_sysroot);
    fs::create_dir_all(&musl_sysroot).unwrap();
    extract_apk_path(&musl_apk, &musl_sysroot, "usr/include");
    let musl_include = musl_sysroot.join("usr/include");
    if !musl_include.join("stdio.h").exists() {
        panic!(
            "musl headers not found at {} after extracting {}",
            musl_include.display(),
            musl_apk.display()
        );
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
        .include(&musl_include)
        .target(&target)
        .host(&host);

    // Build the tcc compiler for SIZE on size/extreme kernels. A smaller tcc
    // image means a smaller demand-paged working set, which is what sets the
    // low-memory compile floor — so forward cargo's "s"/"z" OPT_LEVEL straight
    // to the C compiler (-Os/-Oz) instead of remapping it to -O3.
    let opt_level_str = env::var("OPT_LEVEL").unwrap();
    match opt_level_str.as_str() {
        "s" | "z" => { build.opt_level_str(&opt_level_str); }
        other => { build.opt_level(other.parse().unwrap_or(0)); }
    }
    // Emit one section per function/datum so the linker can garbage-collect the
    // tcc codegen paths that are never reached (pairs with --gc-sections below).
    build
        .flag("-ffunction-sections")
        .flag("-fdata-sections")
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
    // Garbage-collect unreferenced sections at link time (pairs with the
    // -ffunction-sections/-fdata-sections above) to shrink the final binary.
    println!("cargo:rustc-link-arg=--gc-sections");

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
    let musl_inc = musl_include.to_str().unwrap();
    run_cc("tinycc/lib/libtcc1.c", "libtcc1_base.o", &["-I", "tinycc", "-I", "tinycc/include", "-I", musl_inc]);
    run_cc("tinycc/lib/lib-arm64.c", "lib-arm64.o", &["-D__arm64_clear_cache=__clear_cache", "-I", "tinycc", "-I", "tinycc/include", "-I", musl_inc]);

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

    // 3. Stage + pack libtcc1.tar — the ONLY sysroot artifact we ship.
    //
    // It carries tcc's compiler-helper archive (libtcc1.a) AND tcc's internal
    // headers (tccdefs.h, stddef.h, stdarg.h, …). Combined with `apk add
    // musl-dev` on Akuma — which provides crt1.o/crti.o/crtn.o, libc.a and the
    // POSIX headers — this is everything our tcc needs. We deliberately no
    // longer build or ship a full musl sysroot (the old libc.tar); musl is
    // sourced from apk on both sides (headers here, libc on Akuma).
    let libtcc1_staging = out_dir.join("libtcc1_staging");
    if libtcc1_staging.exists() {
        fs::remove_dir_all(&libtcc1_staging).unwrap();
    }
    let libtcc1_tcc_dir = libtcc1_staging.join("usr/lib/tcc");
    let libtcc1_inc_dir = libtcc1_tcc_dir.join("include");
    fs::create_dir_all(&libtcc1_inc_dir).unwrap();
    fs::copy(out_dir.join("libtcc1.a"), libtcc1_tcc_dir.join("libtcc1.a")).unwrap();
    copy_dir_recursive(Path::new("tinycc/include"), &libtcc1_inc_dir).unwrap();

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
        .expect("Failed to execute tar for libtcc1.tar");
    if !status.success() {
        panic!("tar command failed for libtcc1.tar");
    }

    // 4. Copy to dist directory
    let dist_dir = manifest_dir.join("dist");
    fs::create_dir_all(&dist_dir).unwrap();
    fs::copy(&libtcc1_archive_path, dist_dir.join("libtcc1.tar")).unwrap();
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
