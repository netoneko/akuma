# Implementation Plan for GNU Make Userspace Package

This document outlines the steps to integrate GNU Make (version 4.4) as a userspace application within the Akuma OS project. The build process will be entirely managed by a `build.rs` script, ensuring consistency with Akuma's userspace packaging guidelines.

## Objectives

1.  Integrate GNU Make 4.4 into the Akuma userspace.
2.  Automate the build process using a `build.rs` script.
3.  Ensure the compiled binary is statically linked and placed in `bootstrap/bin`.
4.  Adhere to Akuma's `no_std` and `musl`-based userspace conventions.

## Detailed Steps

### 1. Project Structure Setup

*   Create the following directory structure:
    *   `userspace/make/`
    *   `userspace/make/docs/`
    *   `userspace/make/src/`
    *   `userspace/make/vendor/`
*   Place this `IMPLEMENTATION_PLAN.md` file in `userspace/make/docs/`.

### 2. `userspace/make/Cargo.toml` Creation

*   Define a new Cargo package named "make" within the `userspace` workspace.
*   Specify `build = "build.rs"` to ensure our custom build logic is executed.
*   Include `libakuma` as a dependency (even if not directly used, it's a standard userspace dependency).

### 3. `userspace/make/src/main.rs` Creation

*   Create a minimal `main.rs` file. This is required by Cargo, but the actual `make` binary will come from the C source compilation.

### 4. `userspace/make/build.rs` Implementation

This script will orchestrate the entire build process for GNU Make.

*   **Download Source**:
    *   Use `wget` (via `std::process::Command`) to download `https://ftp.gnu.org/gnu/make/make-4.4.tar.gz` into `userspace/make/vendor/`.
*   **Extract Archive**:
    *   Use `tar` (via `std::process::Command`) to extract `make-4.4.tar.gz` within `userspace/make/vendor/`. This will create a `make-4.4` directory.
*   **Configure**:
    *   Run `./configure --host=aarch64-linux-musl CC=aarch64-linux-musl-clang` (via `std::process::Command`). This configures Make for cross-compilation to AArch64 using `musl` and `clang`.
*   **Compile**:
    *   Run `make LDFLAGS="-static"` (via `std::process::Command`) from within the `make-4.4` directory. This compiles Make and ensures static linking.
*   **Install/Copy Binary**:
    *   Copy the resulting `make` binary from `userspace/make/vendor/make-4.4/make` to `bootstrap/bin/make`. This path needs to be resolved carefully within `build.rs` context.

### 5. `userspace/make/README.md` Creation

*   Create a simple `README.md` file in `userspace/make/` explaining that this package contains GNU Make.

### 6. Update `userspace/Cargo.toml`

*   Add `make` to the `members` array in the `[workspace]` section of `userspace/Cargo.toml`.

## Verification

1.  Run `cargo build -p make` from the `userspace/` directory.
2.  Verify that `bootstrap/bin/make` exists and is a valid AArch64 executable.

## Dependencies

*   `wget` and `tar` commands must be available in the build environment.
*   `aarch64-linux-musl-clang` cross-compiler toolchain must be available and configured.

## Potential Challenges

*   Ensuring correct relative paths for commands executed from `build.rs`.
*   Handling potential errors during download, extraction, configuration, and compilation steps within `build.rs`.
*   Verifying that the `aarch64-linux-musl-clang` compiler is correctly picked up by the `configure` script.

---
End of Plan
