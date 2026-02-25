# `pkg` - Akuma OS Package Manager

The `pkg` utility is the primary package manager for Akuma OS userspace applications. It handles the download and installation of binary packages and archives from a configured HTTP/HTTPS server.

## Usage

The main command is `install`:

```
pkg install <package_name> [package_name2 ...]
```

### Examples

*   `pkg install myapp`: Attempts to install a package named `myapp`.
*   `pkg install busybox`: Installs the `busybox` package.

## Installation Process

When `pkg install <package_name>` is executed, the following steps occur:

1.  **Binary Download Attempt**: `pkg` first attempts to download a pre-compiled binary for `<package_name>` directly from the configured package server (e.g., `http://10.0.2.2:8000/bin/<package_name>`). If successful, the binary is placed in `/bin/` and the installation is complete for that package.
2.  **Archive Download Fallback**: If the binary download fails, `pkg` attempts to download a `.tar.gz` archive (e.g., `http://10.0.2.2:8000/archives/<package_name>.tar.gz`) or a `.tar` archive (e.g., `http://10.0.2.2:8000/archives/<package_name>.tar`).
3.  **Extraction**: If an archive is successfully downloaded to `/tmp/`, `pkg` then uses the `/bin/tar` utility to extract its contents to the root filesystem (`/`).
4.  **Cleanup**: The downloaded archive file in `/tmp/` is removed.

## Features

*   **HTTP/HTTPS Support**: `pkg` utilizes the `libakuma-tls` library for network operations, providing robust download capabilities over both insecure HTTP and secure HTTPS connections.
*   **Memory Efficient**: Downloads are streamed directly to disk, avoiding large memory allocations even for very large package files.
*   **External `tar`**: Relies on the `/bin/tar` utility for archive extraction, separating concerns.

## Development

The `pkg` utility is written in Rust and depends on `libakuma` for core OS interactions and `libakuma-tls` for network operations.

## Build Process

To build `pkg`, navigate to the `userspace/` directory and run:

```bash
cargo build --release -p pkg
```

The resulting binary will be located at `userspace/target/aarch64-unknown-none/release/pkg`.
It should then be copied to the `bootstrap/bin/` directory on the disk image to be available in the Akuma OS environment.
