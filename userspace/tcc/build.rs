fn main() {
    println!("cargo:rerun-if-changed=tinycc/tcc.c");
    println!("cargo:rerun-if-changed=tinycc/libtcc.c");
    println!("cargo:rerun-if-changed=src/libc_stubs.c");

    let mut build = cc::Build::new();

    build
        .file("tinycc/tcc.c")
        .file("src/libc_stubs.c")
        .file("src/setjmp.S")
        // TCC Configuration
        .define("TCC_TARGET_ARM64", "1")
        .define("TCC_IS_NATIVE", "1") // We are running on the target
        .define("ONE_SOURCE", "1")
        .define("CONFIG_TCC_STATIC", "1")
        .define("CONFIG_TCC_SEMLOCK", "0")
        // Rename main to tcc_main so we can call it from Rust
        .define("main", "tcc_main")
        // System config
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .flag("-w") // Suppress warnings
        .include("tinycc")
        .include("src")
        .include("include"); // For our custom headers

    build.compile("tcc");
}
