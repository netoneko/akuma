# Refactoring `pkg install` into a Standalone `pkg` Utility

This document outlines the plan to refactor the package installation logic currently duplicated in the `paws` shell and `sshd` daemon into a single, standalone userspace utility located at `/bin/pkg`.

## 1. Project Goal

The `pkg install` command combines network downloads and archive extraction. This logic is complex and currently exists in two places, making it difficult to maintain and extend.

The goal is to:
1.  Create a new Rust binary crate at `userspace/pkg`.
2.  Move the entire `pkg install` logic (downloading and extraction) into this new crate.
3.  Remove the original logic from `paws` and `sshd`.
4.  Modify `paws` and `sshd` to simply execute the `/bin/pkg` binary.

This will centralize the logic, reduce code duplication, and create a standard, composable Unix-style utility.

## 2. Phase 1: `pkg` Crate Scaffolding

First, we will set up the directory structure and manifest for the new `pkg` crate.

1.  **Create Directories**:
    ```sh
    mkdir -p userspace/pkg/src
    mkdir -p userspace/pkg/docs
    ```

2.  **Create `userspace/pkg/Cargo.toml`**:
    The crate will be a binary and will depend on `libakuma` for system calls.

    ```toml
    [package]
    name = "pkg"
    version = "0.1.0"
    edition = "2021"

    [dependencies]
    libakuma = { path = "../libakuma" }
    ```

3.  **Create `userspace/pkg/src/main.rs`**:
    Start with a simple placeholder to ensure the crate builds correctly.

    ```rust
    #![no_std]
    #![no_main]

    extern crate alloc;
    use libakuma::{println, args};

    #[no_mangle]
    pub extern "C" fn main() {
        println!("pkg utility v0.1.0");
        let args_vec: Vec<String> = args().collect();
        // TODO: Implement main logic
    }
    ```

## 3. Phase 2: Implement Core Logic in `pkg`

The core of the work is to move the existing functionality from `paws` into the new `pkg` binary.

1.  **Copy Core Logic**:
    -   Locate the `cmd_pkg` function in `userspace/paws/src/main.rs`. Copy its entire body into the `main` function of `userspace/pkg/src/main.rs`.
    -   The `pkg` utility will get its arguments from `libakuma::args()` instead of a function parameter.

2.  **Copy Dependencies**:
    -   The `cmd_pkg` function depends on several helper functions within `paws/src/main.rs`. These must also be copied into `pkg/src/main.rs`:
        -   `download_file`
        -   `parse_url`
        -   `find_headers_end`
        -   `execute_external_with_status` (and its dependency `find_bin`)
        -   Any other helper traits or structs (e.g., `ParsedUrl`, `ToLowercaseExt`).

3.  **Adapt the Code**:
    -   The code in `pkg` will be a standalone binary, so all functions can be defined at the module level.
    -   Ensure all necessary `use` statements from the top of `paws/src/main.rs` are present (e.g., `use alloc::format;`, `use libakuma::net::TcpStream;`).
    -   Replace `println!` and `print!` calls with the versions from the `libakuma` crate if they are not already.

## 4. Phase 3: Refactor `paws` and `sshd`

With the logic centralized in `/bin/pkg`, we can replace the implementations in `paws` and `sshd` with a simple external command execution.

1.  **Refactor `paws`**:
    -   In `userspace/paws/src/main.rs`, completely replace the body of the `cmd_pkg` function.
    -   The new body will construct an argument vector and call `execute_external_with_status`.

    **New `cmd_pkg` in `paws/src/main.rs`**:
    ```rust
    fn cmd_pkg(args: &[String]) {
        if args.len() < 2 {
            println("Usage: pkg install <package>");
            return;
        }
        // The first argument to the process is the command itself, which is "pkg"
        execute_external_with_status(args);
    }
    ```
    -   Delete the `download_file` function and all its helper functions/structs from `paws/src/main.rs`, as they are no longer needed there.

2.  **Refactor `sshd`**:
    -   Locate the `pkg install` handling logic in `sshd` (likely in `userspace/sshd/src/shell/`). It's a copy of the `paws` implementation.
    -   Apply the same refactoring: replace the entire download-and-extract implementation with a simple function that executes `/bin/pkg` using the `sshd` equivalent of `execute_external_with_status`.
    -   Remove the duplicated `download_file` helper functions from the `sshd` codebase.

## 5. Phase 4: Build Integration & Verification

Finally, integrate the new crate into the OS image build process and create a test plan.

1.  **Update Build Scripts**:
    -   Modify `userspace/build.sh` (or the equivalent workspace build script) to include the new `pkg` crate.
    -   Add a command to build the `pkg` crate and copy the resulting binary to the disk image's `/bin` directory.
    ```sh
    # In userspace/build.sh
    # ...
    cargo build --release -p pkg
    cp target/aarch64-unknown-akuma/release/pkg ../disk/bin/
    # ...
    ```

2.  **Verification Plan**:
    1.  Rebuild the entire userspace to ensure `paws`, `sshd`, and `pkg` all compile successfully.
    2.  Run Akuma and boot into the `paws` shell.
    3.  Execute `pkg install <some_package>` where `<some_package>` is a known, working package.
    4.  **Observe the output**: The output should now come from the `pkg` utility (e.g., "pkg utility v0.1.0"), not from `paws`.
    5.  Verify that the package is downloaded and extracted correctly.
    6.  Connect to the system via `sshd` and run the same `pkg install` command.
    7.  Verify that it also works correctly and that the output indicates the standalone `pkg` utility was executed.
    8.  Check the `paws` and `sshd` binary sizes; they should be slightly smaller now that the duplicated logic has been removed.
