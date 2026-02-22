# Building Userspace Software

This guide explains how to create, build, and package new userspace applications for Akuma OS. It is designed to be agent-friendly and follows the established patterns for integrating Rust with C libraries and the Akuma kernel.

## Core Foundation

All userspace software in Akuma relies on two primary components:

1.  **`libakuma`**: The core Rust library that provides safe wrappers for Akuma system calls.
2.  **`musl`**: A lightweight C library used for C-compatibility and as a foundation for more complex software (like `tcc` or `quickjs`).

## Project Structure

New packages should be added as members of the `userspace` workspace.

```text
userspace/
├── myapp/
│   ├── Cargo.toml
│   ├── build.rs         # CRITICAL: All build logic goes here
│   └── src/
│       └── main.rs
├── libakuma/
└── musl/
```

### Cargo.toml Template

```toml
[package]
name = "myapp"
version = "0.1.0"
edition = "2021"
build = "build.rs"

[dependencies]
libakuma = { path = "../libakuma" }
# Add other dependencies here
```

## Wrapping C Libraries

If your application depends on C libraries or source files:

1.  Place the C source in a subdirectory (e.g., `myapp/vendor/`).
2.  Use the `cc` crate in `build.rs` to compile the C code.
3.  **No Shell Scripts**: All compilation and packaging must be handled within `build.rs`.

### `build.rs` for C Integration

```rust
fn main() {
    println!("cargo:rerun-if-changed=vendor/library.c");
    
    cc::Build::new()
        .file("vendor/library.c")
        .include("vendor")
        .include("../musl/dist/include") // Use musl headers
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .flag("-w")
        .compile("library");
}
```

## Packaging for `pkg install`

The `pkg install` command in `paws` expects artifacts in specific locations and formats.

### 1. Standalone Binaries
For simple tools, the binary should be placed in `../bootstrap/bin/` or served from a `bin/` directory on the package server.

### 2. Archive Packages (.tar)
For software that requires supporting files (headers, libraries, config), use a `.tar` archive.

**CRITICAL: Tar Format Settings**
Akuma's `tar` implementation and `pkg install` require a very specific tar format. You must use these settings in your `build.rs`:

```rust
use std::process::Command;
// ... inside main()

let status = Command::new("tar")
    .env("COPYFILE_DISABLE", "1") // Disable macOS ._ files
    .arg("--no-xattrs")           // No extended attributes
    .arg("--format=ustar")        // Standard USTAR format
    .arg("-cf")
    .arg(&archive_path)
    .arg("-C")
    .arg(&staging_dir)
    .arg("usr")                   // Usually packages are rooted at /usr
    .status()
    .expect("Failed to create tar archive");
```

## Complete `build.rs` Template

This template demonstrates how to compile a C library, link it, and package the final result as a `.tar` archive for `pkg install`.

```rust
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let staging_dir = out_dir.join("staging");
    let dist_dir = manifest_dir.join("dist");

    // 1. Compile C dependencies
    cc::Build::new()
        .file("src/c_logic.c")
        .include("../musl/dist/include")
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .compile("clogic");

    // 2. Prepare staging area for packaging
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir).unwrap();
    }
    fs::create_dir_all(staging_dir.join("usr/bin")).unwrap();
    
    // Note: build.rs runs BEFORE the crate is compiled. 
    // To package the binary produced by the current crate, you have two options:
    // 1. Use a separate "package" crate that depends on your app and packages it.
    // 2. Have build.rs package assets/libraries, and use a workspace-level tool to collect the final binary.
    // For C-based tools (like TCC or QuickJS), build.rs can compile the C code and package it directly.

    // 3. Create the Tar Archive (The "Particular" Way)
    if !dist_dir.exists() {
        fs::create_dir_all(&dist_dir).unwrap();
    }
    
    let archive_path = dist_dir.join("myapp.tar");
    
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
        panic!("Tar creation failed");
    }
    
    println!("cargo:warning=Package created at {}", archive_path.display());
}
```

## Deployment for Testing

To make your package available for `pkg install`:

1.  Run the build process (this copies binaries and archives to `bootstrap/`):
    ```bash
    cd userspace
    ./build.sh
    ```
2.  Run a web server in the `bootstrap/` directory (where binaries and archives are staged):
    ```bash
    cd ../bootstrap
    python3 -m http.server 8000
    ```
3.  Inside Akuma (`paws` shell):
    ```bash
    pkg install myapp
    ```

## Agent Guidelines

When adding a new package:
1.  **Always** use `build.rs` for any logic beyond basic Cargo dependencies.
2.  **Strictly** adhere to the `tar` flags mentioned above.
3.  Verify that `libakuma` is used for all kernel interactions.
4.  If the package is a C library wrapper, ensure `musl` headers are included and `no_std` is maintained in the Rust wrapper.
