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

    // --- Start of new archiving logic ---
    let staging_dir = out_dir.join("sysroot_staging");
    let lib_dir = staging_dir.join("lib");
    let include_dir = staging_dir.join("include");

    fs::create_dir_all(&lib_dir).unwrap();
    fs::create_dir_all(&include_dir).unwrap();

    // Copy the compiled static library
    let lib_src_path = out_dir.join(format!("libtcc_all_objs.a"));
    let lib_dest_path = lib_dir.join(format!("libtcc_all_objs.a"));
    fs::copy(&lib_src_path, &lib_dest_path).unwrap();

    // Copy relevant headers
    copy_headers_recursive("tinycc", &include_dir).unwrap();
    copy_headers_recursive("src", &include_dir).unwrap();
    copy_headers_recursive("include", &include_dir).unwrap();

    // Create the archive
    let archive_name = "tcc_sysroot.tar.gz";
    let archive_path = out_dir.join(archive_name);

    // Use `tar` to create a gzipped archive
    Command::new("tar")
        .arg("-czvf")
        .arg(&temp_archive_path)
        .arg("-C") // Change directory before adding files
        .arg(&out_dir)
        .arg("sysroot_staging")
        .status()
        .expect("Failed to create tar.gz archive");

    // Copy the archive to a well-known location for build.sh
    let dist_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("dist");
    fs::create_dir_all(&dist_dir).unwrap();
    let final_archive_path = dist_dir.join(archive_name);
    fs::copy(&temp_archive_path, &final_archive_path).unwrap();

    println!("cargo:warning=Generated TCC sysroot archive: {}", final_archive_path.display());
    // --- End of new archiving logic ---
}

// Helper function to recursively copy .h files
fn copy_headers_recursive<P: AsRef<Path>>(src: P, dest: P) -> std::io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().map_or(false, |ext| ext == "h") {
            let file_name = path.file_name().unwrap();
            fs::copy(&path, dest.as_ref().join(file_name))?;
        } else if path.is_dir() {
            // Recursive copy not strictly needed for this case as includes are flat,
            // but good practice if structure was nested.
            // For now, only top-level headers in these dirs are copied to a single include dir.
        }
    }
    Ok(())
}
