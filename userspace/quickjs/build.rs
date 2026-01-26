//! Build script for compiling QuickJS for bare-metal aarch64
//!
//! This compiles the QuickJS engine with flags suitable for
//! a no_std environment without OS support.

fn main() {
    println!("cargo:rerun-if-changed=quickjs/quickjs.c");
    println!("cargo:rerun-if-changed=quickjs/quickjs.h");
    println!("cargo:rerun-if-changed=quickjs/cutils.c");
    println!("cargo:rerun-if-changed=quickjs/libbf.c");
    println!("cargo:rerun-if-changed=quickjs/libregexp.c");
    println!("cargo:rerun-if-changed=quickjs/libunicode.c");
    println!("cargo:rerun-if-changed=quickjs/stubs.c");

    // Compile our stubs first
    cc::Build::new()
        .file("quickjs/stubs.c")
        .include("quickjs")
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .flag("-w")
        .compile("stubs");

    // Compile QuickJS with our custom headers
    cc::Build::new()
        .file("quickjs/quickjs.c")
        .file("quickjs/cutils.c")
        .file("quickjs/libbf.c")
        .file("quickjs/libregexp.c")
        .file("quickjs/libunicode.c")
        // Use our shim headers
        .include("quickjs")
        // No standard includes - use our shims
        .flag("-nostdinc")
        // Freestanding environment flags
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        // Disable warnings for QuickJS code
        .flag("-w")
        // QuickJS configuration
        .define("CONFIG_VERSION", "\"2024-01-13\"")
        // Enable BigInt support
        .define("CONFIG_BIGNUM", None)
        // Disable features that require OS support
        .define("EMSCRIPTEN", None)
        .compile("quickjs");
}
