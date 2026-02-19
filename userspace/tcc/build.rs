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
    println!("cargo:rerun-if-changed=lib/crti.S");
    println!("cargo:rerun-if-changed=lib/crtn.S");
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
        .define("CONFIG_TCCDIR", "\"/usr/lib/tcc\"")
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
    
    // libc.a
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
        .compile("akuma_libc");

    // crt1.o (from crt0.S)
    cc::Build::new()
        .file("lib/crt0.S")
        .target(&target)
        .host(&env::var("HOST").unwrap())
        .out_dir(&out_dir)
        .compile("crt1");

    // crti.o
    cc::Build::new()
        .file("lib/crti.S")
        .target(&target)
        .host(&env::var("HOST").unwrap())
        .out_dir(&out_dir)
        .compile("crti");

    // crtn.o
    cc::Build::new()
        .file("lib/crtn.S")
        .target(&target)
        .host(&env::var("HOST").unwrap())
        .out_dir(&out_dir)
        .compile("crtn");

    // libtcc1.a (TCC's own runtime)
    cc::Build::new()
        .file("tinycc/lib/libtcc1.c")
        .file("tinycc/lib/lib-arm64.c")
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .include("tinycc")
        .target(&target)
        .host(&env::var("HOST").unwrap())
        .opt_level(3)
        .out_dir(&out_dir)
        .compile("tcc1");

    // 3. Stage the sysroot
    let staging_dir = out_dir.join("sysroot_staging");
    let lib_dir = staging_dir.join("usr/lib");
    let include_dir = staging_dir.join("usr/include");
    let tcc_lib_dir = lib_dir.join("tcc");
    let tcc_include_dir = tcc_lib_dir.join("include");

    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir).unwrap();
    }
    fs::create_dir_all(&lib_dir).unwrap();
    fs::create_dir_all(&include_dir).unwrap();
    fs::create_dir_all(&tcc_lib_dir).unwrap();
    fs::create_dir_all(&tcc_include_dir).unwrap();

    fs::copy(out_dir.join("libakuma_libc.a"), lib_dir.join("libc.a")).unwrap();
    fs::copy(out_dir.join("libtcc1.a"), tcc_lib_dir.join("libtcc1.a")).unwrap();
    
    let find_and_copy_o = |name: &str, dest: &Path| {
        for entry in fs::read_dir(&out_dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_file() && path.extension().map_or(false, |ext| ext == "o") {
                let file_name = path.file_name().unwrap().to_str().unwrap();
                if file_name.ends_with(&format!("{}.o", name)) {
                    fs::copy(&path, dest).unwrap();
                    return;
                }
            }
        }
        fs::copy(out_dir.join(format!("lib{}.a", name)), dest).unwrap();
    };

    find_and_copy_o("crt1", &tcc_lib_dir.join("crt1.o"));
    find_and_copy_o("crti", &tcc_lib_dir.join("crti.o"));
    find_and_copy_o("crtn", &tcc_lib_dir.join("crtn.o"));
    
    copy_dir_recursive(Path::new("include"), &include_dir).unwrap();
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

    // 5. Copy to dist directory
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dist_dir = manifest_dir.join("dist");
    fs::create_dir_all(&dist_dir).unwrap();
    fs::copy(&archive_path, dist_dir.join("libc.tar")).unwrap();

    println!("cargo:warning=libc archive created at {}", dist_dir.join("libc.tar").display());
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
