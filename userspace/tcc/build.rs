use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=tinycc/tcc.c");
    println!("cargo:rerun-if-changed=tinycc/libtcc.c");
    println!("cargo:rerun-if-changed=src/libc_stubs.c");
    println!("cargo:rerun-if-changed=src/setjmp.S");
    println!("cargo:rerun-if-changed=src/config.h");
    println!("cargo:rerun-if-changed=tinycc/tccdefs.h"); // tccdefs.h

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap(); // e.g., aarch64-unknown-none
    
    // Object file names for TCC core
    let tcc_main_obj_name = "tcc_main.o";
    let libc_stubs_obj_name = "libc_stubs.o";
    let setjmp_obj_name = "setjmp.o";

    let mut common_build = cc::Build::new();
    common_build
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
    common_build.opt_level(opt_level_num)
        .out_dir(&out_dir);

    // Compile tinycc/tcc.c
    let mut tcc_compiler = common_build.clone();
    tcc_compiler
        .file("tinycc/tcc.c")
        .define("main", "tcc_main") // Rename main to tcc_main
        .compile(tcc_main_obj_name);

    // Compile src/libc_stubs.c
    let mut stubs_compiler = common_build.clone();
    stubs_compiler
        .file("src/libc_stubs.c")
        .compile(libc_stubs_obj_name);

    // Compile src/setjmp.S
    let mut setjmp_compiler = common_build.clone(); // Use a clone here for setjmp
    setjmp_compiler
        .file("src/setjmp.S")
        .compile(setjmp_obj_name);

    // Compile lib/crt1.S for embedding
    let mut crt1_compiler = cc::Build::new(); // Use a fresh build to avoid inherited flags
    crt1_compiler
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .flag("-w") // Suppress warnings
        .target(&target)
        .host(&env::var("HOST").unwrap())
        .file("lib/crt1.S")
        .compile("crt1.o"); // Compile to crt1.o
    
    // Compile lib/libc.c for embedding
    let mut libc_compiler = cc::Build::new(); // Use a fresh build to avoid inherited flags
    libc_compiler
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .flag("-w") // Suppress warnings
        .include("include") // libc.c needs system headers
        .define("time_t", "long long")
        .target(&target)
        .host(&env::var("HOST").unwrap())
        .file("lib/libc.c")
        .compile("libc.o"); // Compile to libc.o

    // Compile lib/crti.S for embedding
    let mut crti_compiler = cc::Build::new(); // Use a fresh build to avoid inherited flags
    crti_compiler
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .flag("-w") // Suppress warnings
        .target(&target)
        .host(&env::var("HOST").unwrap())
        .file("lib/crti.S")
        .compile("crti.o"); // Compile to crti.o

    // Compile lib/crtn.S for embedding
    let mut crtn_compiler = cc::Build::new(); // Use a fresh build to avoid inherited flags
    crtn_compiler
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .flag("-w") // Suppress warnings
        .target(&target)
        .host(&env::var("HOST").unwrap())
        .file("lib/crtn.S")
        .compile("crtn.o"); // Compile to crtn.o

    // Manually create a single archive libtcc_all_objs.a from all object files
    let lib_tcc_core_path = out_dir.join("libtcc_all_objs.a");
    
    let mut ar_command = cc::Build::new().target(&target).get_archiver();
    ar_command
        .arg("rcs") // r: insert or replace, c: create, s: write archive index
        .arg(&lib_tcc_core_path)
        .arg(out_dir.join(tcc_main_obj_name))
        .arg(out_dir.join(libc_stubs_obj_name))
        .arg(out_dir.join(setjmp_obj_name))
        .current_dir(&out_dir) // Run ar in out_dir so paths are relative
        .status()
        .expect("Failed to create libtcc_all_objs.a archive");

    // Instruct rustc to link against the manually created archive
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=tcc_all_objs");
}
