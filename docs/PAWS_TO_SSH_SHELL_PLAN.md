# Plan: Migrate "pkg install" Logic from `paws` to SSH Shell

**Objective:** Consolidate the package installation functionality from the `userspace/paws` application into the built-in `pkg` command in the kernel's SSH shell. This will provide a more robust and centralized package management system.

## 1. Analysis of Current Implementations

### `userspace/paws` "pkg install":
- **Functionality:**
    - Downloads a package from a hardcoded server (`10.0.2.2:8000`).
    - First, attempts to download a pre-compiled binary from `http://<server>/bin/<package>` and installs it to `/bin/<package>`.
    - If the binary download fails, it falls back to downloading a tarball (`.tar.gz` or `.tar`) from `http://<server>/archives/<package>.tar[.gz]` to `/tmp/`.
    - It then uses an external `tar` command to extract the archive into the root filesystem (`/`).
- **Dependencies:**
    - `libakuma` for syscalls (networking, file I/O).
    - An external `tar` executable must be available for archive extraction.

### Built-in `pkg` command (`src/shell/commands/net.rs`):
- **Functionality:**
    - Downloads a package from a hardcoded path: `http://10.0.2.2:8000/target/aarch64-unknown-none/release/<package>`.
    - Saves the downloaded file directly to `/bin/<package>`.
- **Limitations:**
    - Only handles single-file binary packages.
    - Does not support archive extraction.
    - Lacks the fallback mechanism of `paws`.

## 2. Proposed Migration Plan

The goal is to extend the existing `pkg` command in `src/shell/commands/net.rs` to incorporate the more advanced features from `paws`, with the key change that `tar` extraction will be handled by an external process.

### Step 1: Modify `http_get` to Return Full Response

The current `http_get` helper in `src/shell/commands/net.rs` only returns the response body. To properly handle download failures (like 404 Not Found), it needs to be modified to return the HTTP status code as well.

- **Action:**
    - Modify `http_get` to return `Result<(u16, Vec<u8>), &'static str>`, where `u16` is the status code and `Vec<u8>` is the body.
    - Update the logic inside `http_get` to parse the status code from the response headers.

### Step 2: Implement Binary and Archive Download Logic in `PkgCommand`

Re-implement the core installation logic from `paws` within the `install_package` function of the `PkgCommand` struct.

- **Action:**
    1.  **Try Binary Download:**
        - Construct the URL for the binary: `http://10.0.2.2:8000/bin/<package>`.
        - Call the modified `http_get`.
        - If the status code is `200` (OK), write the response body to `/bin/<package>` and the installation for that package is complete.
    2.  **Fallback to Archive Download:**
        - If the binary download fails (e.g., status code `404`), attempt to download a tarball.
        - Try `http://10.0.2.2:8000/archives/<package>.tar.gz`.
        - If that fails, try `http://10.0.2.2:8000/archives/<package>.tar`.
        - If a tarball is successfully downloaded, save it to a temporary location, like `/tmp/<package>.tar.gz` or `/tmp/<package>.tar`.

### Step 3: Integrate External `tar` Extraction into `PkgCommand`

Instead of an in-kernel extractor, we will rely on an external `/bin/tar` utility.

- **Action:**
    1.  **Check for `/bin/tar`:** Before attempting extraction, check if `/bin/tar` exists.
    2.  **Spawn `tar` process:**
        - If `/bin/tar` exists, spawn a process to extract the downloaded tarball.
        - Example command: `tar -xzvf /tmp/<package>.tar.gz -C /` (for gzipped tar) or `tar -xvf /tmp/<package>.tar -C /` (for plain tar).
        - Use `execute_external_streaming` or `execute_external` from `src/shell/mod.rs` to run `tar`.
        - Capture and display any output or errors from the `tar` command.
    3.  **Handle missing `tar`:**
        - If `/bin/tar` does not exist, print a message to `stdout` recommending the user install `tar` using `pkg install tar`.
        - Example message: `"tar command not found. Please install it using 'pkg install tar' to extract archive packages."`
    4.  **Clean up:** Delete the temporary tarball from `/tmp` after successful (or attempted) extraction.

## 3. Code Structure

- **`src/shell/commands/net.rs`:**
    - `PkgCommand::install_package`: Will contain the main logic for binary/archive download attempts and the external `tar` invocation.
    - `http_get`: Modified to return the HTTP status code.
- **`src/shell/mod.rs`:**
    - The `execute_external_streaming` or `execute_external` function will be used to spawn the `tar` process.

## 4. Summary of Changes

By following this revised plan, the built-in `pkg` command will gain the following features, matching and exceeding the functionality of the `paws` utility:
- Robust, multi-step installation process (binary with archive fallback).
- Support for installing multi-file packages via tarballs by utilizing an external `/bin/tar` utility.
- User guidance to install `tar` if it's missing.
- Centralized logic within the kernel, removing the need for a separate userspace utility for this core function.
