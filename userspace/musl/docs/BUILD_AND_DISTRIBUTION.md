# Musl Build and Distribution Process

This document summarizes the automation and integration of `musl` libc into the Akuma OS build ecosystem.

## 1. Automated Build Package (`userspace/musl/`)
A dedicated Cargo package was created to manage the Musl lifecycle:
- **`build.rs`**: Implements a robust build pipeline:
    - **Out-of-Tree Build**: Builds Musl in a `build/` subdirectory within `OUT_DIR` to prevent source tree pollution.
    - **Cross-Compilation**: Correctly passes `aarch64-linux-musl` target flags and LLVM toolchain paths (`llvm-ar`, `llvm-ranlib`).
    - **Artifact Staging**: Installs Musl to a local path and prepares a `usr/` structure containing `lib/` and `include/`.
- **Archive Generation**: Produces `musl.tar` using specific compatibility flags:
    - `--format=ustar`: Ensures Akuma's `tar` utility can parse the headers.
    - `COPYFILE_DISABLE=1` & `--no-xattrs`: Prevents macOS metadata (`._` files) from corrupting the archive.

## 2. Workspace Integration
- **`userspace/Cargo.toml`**: Added `musl` as a workspace member.
- **`userspace/build.sh`**: Integrated the package into the global build script. It now automatically builds Musl and copies `musl.tar` to `bootstrap/archives/` for system-wide deployment.

## 3. TCC Integration
The rudimentary stub libc previously used by TCC has been completely replaced by Musl:
- **Sysroot Update**: `userspace/tcc/build.rs` now points to `userspace/musl/dist/` for standard C headers and `libc.a`.
- **Runtime Objects**: TCC now links using Musl's standard startup objects (`crt1.o`, `crti.o`, `crtn.o`).
- **Standard Compliance**: C programs compiled with `tcc` inside Akuma now target a full POSIX-compliant environment by default.

## 4. Distribution Flow
1. `cargo build -p musl` -> Compiles Musl and creates `musl.tar`.
2. `userspace/build.sh` -> Moves `musl.tar` to `bootstrap/archives/`.
3. `scripts/populate_disk.sh` -> Includes the archive in the disk image.
4. Akuma Boot -> `paws` (sh) extracts `musl.tar` to `/usr`, populating the system sysroot.
