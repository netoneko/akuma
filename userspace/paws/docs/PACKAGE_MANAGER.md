# Paws Package Manager (`pkg`)

`paws` includes a basic package management utility accessible via the `pkg` command. This allows users to download and install software directly from a configured package server.

## Usage

```bash
pkg install <package_name> [package_name2 ...]
```

## Installation Strategy

The `pkg install` command follows a multi-step strategy to ensure packages can be installed either as standalone binaries or as complex archives containing multiple files (like headers and libraries).

### 1. Binary Installation (Fast Path)
`paws` first attempts to download the package as a single binary file.
*   **URL**: `http://<server>/bin/<package_name>`
*   **Destination**: `/bin/<package_name>`
*   If successful, the package is immediately usable as a command.

### 2. Archive Installation (Fallback)
If the binary download fails, `paws` attempts to find a compressed or uncompressed archive. This is used for packages like `tcc` or `libc` that require headers, libraries, and other support files.
*   **Supported Formats**: `.tar.gz` (compressed) and `.tar` (uncompressed).
*   **URLs**: 
    *   `http://<server>/archives/<package_name>.tar.gz`
    *   `http://<server>/archives/<package_name>.tar`
*   **Temporary Destination**: `/tmp/<package_name>.tar[.gz]`
*   **Extraction**: The archive is extracted to the root directory (`/`) using the internal `tar` command (`tar -xzvf` or `tar -xvf`).
*   **Cleanup**: The temporary archive file is deleted after successful extraction.

## Server Configuration

Currently, the package server is hardcoded to `10.0.2.2:8000`, which typically points to the host machine in a QEMU user-mode networking environment.

## Recent Changes (2026)

*   **Archive Fallback**: Added the ability to download and extract `.tar` and `.tar.gz` archives if a direct binary download is unavailable.
*   **Multi-Package Support**: Updated the command to accept multiple package names in a single invocation.
*   **Automatic Extraction**: Integrated with the userspace `tar` utility to automatically extract archives to the correct system paths (e.g., `/usr/lib`, `/usr/include`).
*   **Cleanup Logic**: Added automatic unlinking of temporary archive files after installation to save disk space.
