# TCC Distribution Plan for Akuma OS

This document outlines the plan for distributing and installing TinyCC (TCC) and its associated sysroot (headers and runtime libraries) on Akuma OS.

## 1. Build System Enhancements (`userspace/tcc/build.rs`)

The `build.rs` script will be responsible for preparing the TCC sysroot archive during the build process on the host.

### Tasks:
*   **Compile Runtime Objects:**
    *   Compile `userspace/tcc/lib/crt0.S` into `crt0.o` for the AArch64 target.
    *   Compile `userspace/tcc/lib/libc.c` into `libc.o` (or `libc.a`) for the AArch64 target.
*   **Stage Sysroot Directory:**
    *   Create a staging directory (e.g., `target/.../libc_staging`).
    *   `staging/include/`: Copy all headers from `userspace/tcc/include/`.
    *   `staging/lib/`: Place the compiled `crt0.o` and `libc.o`.
*   **Create Archive:**
    *   Pack the staging directory into a `libc.tar.gz` archive.
    *   *Note:* If `tar` is too complex to extract in `no_std`, consider a simple custom "blob" format (manifest + concatenated files) optionally compressed with zlib.
*   **Export Archive:**
    *   Copy the resulting archive to the `dist` directory or the Cargo `OUT_DIR` so `build.sh` can find it.

## 2. Global Build Script (`userspace/build.sh`)

Update the main build script to ensure the TCC archive is available for the package server.

### Tasks:
*   Ensure the `tcc` build step successfully generates `libc.tar.gz`.
*   Copy `libc.tar.gz` to the root of the userspace release directory (where `pkg install` expects to find files).

## 3. Package Manager Enhancements (`userspace/paws/src/main.rs`)

Update the `paws` shell's built-in `pkg install` command to handle both binaries and sysroot archives.

### Logic:
1.  **Attempt Binary Download:**
    *   Try to download `http://10.0.2.2:8000/bin/<package>`.
    *   If successful, save to `/bin/<package>` and finish.
2.  **Fallback to Archive Download:**
    *   If the binary is not found (or if the package is known to be a sysroot like `tcc-sysroot`), try to download `http://10.0.2.2:8000/archives/<package>.tar.gz`.
3.  **Extraction Process:**
    *   Download the archive to a temporary location (e.g., `/tmp/download.tar.gz`).
    *   Decompress and extract the archive to a temporary directory (e.g., `/tmp/extract/`).
    *   **Move Files:** Atomic-like move of extracted files to their final destinations:
        *   `include/*` -> `/usr/include/`
        *   `lib/*` -> `/usr/lib/`
    *   **Cleanup:** Remove temporary files.

## 4. Technical Requirements

*   **Decompression:** `paws` will need a `no_std` compatible zlib/deflate decompressor (e.g., `miniz_oxide`, as used in `scratch`).
*   **Tar Handling:** A minimal `tar` parser will be needed in `paws` to iterate through the archive entries and write them to the filesystem.
*   **Filesystem Operations:** `libakuma` must support the necessary directory creation (`mkdir_p`) and file writing operations.

## 5. Verification Steps

1.  Run `build.sh` on the host and verify `libc.tar.gz` is created.
2.  Start the Akuma kernel and the Python package server on the host.
3.  In `paws`, run `pkg install tcc`.
4.  Verify that `/bin/cc` (the TCC binary) is installed.
5.  Verify that `/usr/include/stdio.h` and `/usr/lib/libc.o` exist.
6.  Try compiling a "Hello World" program on Akuma: `cc hello.c -o hello`.
