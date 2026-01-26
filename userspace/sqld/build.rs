//! Build script for compiling SQLite for bare-metal aarch64
//!
//! This compiles the SQLite amalgamation with flags suitable for
//! a no_std environment without OS support.

fn main() {
    println!("cargo:rerun-if-changed=sqlite3/sqlite3.c");
    println!("cargo:rerun-if-changed=sqlite3/sqlite3.h");
    println!("cargo:rerun-if-changed=sqlite3/sqlite_stubs.c");

    // Compile our stubs first
    cc::Build::new()
        .file("sqlite3/sqlite_stubs.c")
        .include("sqlite3")
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .flag("-w")
        .compile("sqlite_stubs");

    // Compile SQLite with our custom headers
    cc::Build::new()
        .file("sqlite3/sqlite3.c")
        // Use our shim headers
        .include("sqlite3")
        // No standard includes - use our shims
        .flag("-nostdinc")
        // Use custom OS layer (we provide VFS)
        .define("SQLITE_OS_OTHER", "1")
        // Single-threaded (no mutex needed)
        .define("SQLITE_THREADSAFE", "0")
        // Omit features that require OS support we don't have
        .define("SQLITE_OMIT_LOAD_EXTENSION", None)
        .define("SQLITE_OMIT_LOCALTIME", None)
        .define("SQLITE_OMIT_WAL", None)
        .define("SQLITE_OMIT_SHARED_CACHE", None)
        .define("SQLITE_OMIT_AUTOINIT", None)
        // Disable features that need more OS support
        .define("SQLITE_OMIT_PROGRESS_CALLBACK", None)
        .define("SQLITE_OMIT_DEPRECATED", None)
        .define("SQLITE_OMIT_TRACE", None)
        .define("SQLITE_OMIT_UTF16", None)
        .define("SQLITE_OMIT_COMPLETE", None)
        .define("SQLITE_OMIT_DECLTYPE", None)
        .define("SQLITE_OMIT_DATETIME_FUNCS", None)
        // Use memory-only temp storage
        .define("SQLITE_TEMP_STORE", "3")
        // Freestanding environment flags
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        // Disable warnings for SQLite code
        .flag("-w")
        .compile("sqlite3");
}
