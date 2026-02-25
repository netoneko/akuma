# Refactoring `pkg install` Download Logic to `libakuma-tls`

## Goal

The primary goal of this refactoring was to centralize the network download and HTTP/HTTPS parsing logic for package installation. Previously, this logic was duplicated in the `paws` shell and `sshd` daemon, and then consolidated into the new `pkg` utility, but still as an internal implementation.

This document describes the final step of moving this network-related functionality into the shared `libakuma-tls` library, making it reusable and providing a foundation for secure (HTTPS) downloads for all userspace applications.

## Changes Implemented

### 1. `libakuma-tls` Updates

The `userspace/libakuma-tls` crate was enhanced with a new public function:

-   **`libakuma_tls::download_file(url: &str, dest_path: &str) -> Result<(), Error>`**: This function now handles the entire process of fetching a file from a given URL (supporting both HTTP and HTTPS) and streaming its content directly to a specified local file path.
    -   It reuses existing `libakuma-tls` components for URL parsing, DNS resolution, TCP/TLS connection setup, and HTTP request construction.
    -   Crucially, it implements a streaming mechanism to write the response body directly to a file descriptor, avoiding large memory allocations for big downloads.
    -   HTTP header parsing is performed to check the status code before streaming the body.

The `libakuma-tls/src/lib.rs` file was updated to export this new function.

### 2. `pkg` Utility Refactoring

The `userspace/pkg` crate was modified to leverage the new `libakuma-tls::download_file` function:

-   **Dependency Added**: `libakuma-tls` was added as a dependency in `userspace/pkg/Cargo.toml`.
-   **Code Removal**: All internal implementations of `download_file`, `parse_url`, `find_headers_end`, `ToLowercaseExt`, and `ParsedUrl` were removed from `userspace/pkg/src/main.rs`.
-   **Function Call Update**: The `cmd_pkg` function in `userspace/pkg/src/main.rs` was updated to call `libakuma_tls::download_file` for all download operations, simplifying its logic and immediately gaining HTTPS capabilities.
-   **Cleanup**: Unused `use` statements were removed from `userspace/pkg/src/main.rs`.

## Impact

-   **Reduced Duplication**: The core download logic is now in a single, well-defined library.
-   **Improved Maintainability**: Changes or bug fixes to the download process only need to be applied in one place (`libakuma-tls`).
-   **Memory Efficiency**: Streaming downloads prevent large files from exhausting memory resources in userspace applications.
-   **HTTPS Support**: The `pkg` utility (and any future userspace app) now automatically supports secure HTTPS downloads without additional code.
-   **Clear Separation of Concerns**: `pkg` focuses on package management (what to install, where to extract), while `libakuma-tls` handles the network communication details (how to download).
