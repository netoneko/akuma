use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=tinycc/tcc.c");
    println!("cargo:rerun-if-changed=tinycc/libtcc.c");
    println!("cargo:rerun-if-changed=src/libc_stubs.c");
    println!("cargo:rerun-if-changed=src/setjmp.S");
    println!("cargo:rerun-if-changed=src/config.h");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap(); // e.g., aarch64-unknown-none
    
    let mut build = cc::Build::new();
    build
        .define("TCC_TARGET_ARM64", "1")
        .define("ONE_SOURCE", "1")
        .define("CONFIG_TCC_STATIC", "1")
        .define("CONFIG_TCC_SEMLOCK", "0")
        .define("time_t", "long long") // Define time_t globally
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .flag("-w") // Suppress warnings
        .include("tinycc")
        .include("src")
        .include("include")
        .target(&target)
        .host(&env::var("HOST").unwrap());
    
    let opt_level_str = env::var("OPT_LEVEL").unwrap();
    let opt_level_num = match opt_level_str.as_str() {
        "s" | "z" => 3, // For optimized size
        _ => opt_level_str.parse().unwrap_or(0), // Parse to u32, default to 0 if parsing fails
    };
    build.opt_level(opt_level_num)
        .out_dir(&out_dir);

    // Add all source files to a single compilation step
    build
        .file("tinycc/tcc.c")
        .file("src/libc_stubs.c")
        .file("src/setjmp.S")
        .define("main", "tcc_main") // Rename main to tcc_main
        .compile("tcc_all_objs"); // Compile all into one static library libtcc_all_objs.a

    // Instruct rustc to link against this library
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=tcc_all_objs");
}
