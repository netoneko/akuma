# Package Management

This document describes the package management system in Akuma for userspace binaries.

## Overview

Akuma includes a simple package manager (`pkg`) that downloads and installs userspace binaries from an HTTP server. Packages are ELF binaries or tar archives served over HTTP and installed to `/bin/`.

All downloads use **streaming I/O** — data is written to disk in 4 KB chunks as it arrives from the network, so the kernel heap footprint stays constant regardless of file size. This allows installing multi-megabyte binaries (e.g. `llama-cli`) without OOM panics on the 32 MB kernel heap.

## Architecture

```
┌─────────────────────┐   HTTP GET (streaming)   ┌─────────────────────┐
│     Akuma Shell     │ ──────────────────────▶  │   Python HTTP       │
│     (pkg install)   │   4 KB chunks → disk     │   Server (port 8000)│
└─────────────────────┘                          └─────────────────────┘
         │                                                │
         ▼                                                ▼
┌─────────────────────┐                          ┌─────────────────────┐
│      /bin/<pkg>     │                          │   Served directory  │
│   (installed ELF)   │                          │   (bin/, archives/) │
└─────────────────────┘                          └─────────────────────┘
```

## Using the Package Manager

### Installing Packages

```
pkg install <package1> [package2] ...
```

The package manager tries two strategies in order:

1. **Binary**: downloads `http://10.0.2.2:8000/bin/<package>` and saves to `/bin/<package>`
2. **Archive**: downloads `http://10.0.2.2:8000/archives/<package>.tar.gz` (or `.tar`), extracts to `/`

Multiple packages can be specified in a single command. If one package fails to install, the process continues with the remaining packages.

### Examples

```
pkg install dash
pkg install tar
pkg install echo2 hello
```

## Userspace Package Structure

Userspace packages live in the `userspace/` directory as a Cargo workspace:

```
userspace/
├── Cargo.toml          # Workspace configuration
├── libakuma/           # Shared library for syscall wrappers
│   ├── Cargo.toml
│   └── src/lib.rs
├── echo2/              # Example package
│   ├── Cargo.toml
│   └── src/main.rs
├── stdcheck/           # Standard library compatibility checker
│   ├── Cargo.toml
│   └── src/main.rs
└── linker.ld           # Linker script for userspace binaries
```

## Building Packages

### Build All Packages

```bash
cd userspace
cargo build --release
```

### Build a Specific Package

```bash
cd userspace
cargo build --release -p stdcheck
```

Binaries are output to `userspace/target/aarch64-unknown-none/release/`.

## Serving Packages

To make packages available for `pkg install`, run a web server from a directory containing `bin/` and/or `archives/` subdirectories:

```bash
# Example layout:
# packages/
# ├── bin/
# │   ├── dash
# │   ├── tar
# │   └── llama-cli
# └── archives/
#     └── sbase.tar.gz

cd packages
python3 -m http.server 8000
```

The package manager uses `10.0.2.2` (QEMU's host gateway) to reach the host machine's port 8000.

## Creating a New Package

1. Create a new directory under `userspace/`:

```bash
cd userspace
mkdir -p mypackage/src
```

2. Create `Cargo.toml`:

```toml
[package]
name = "mypackage"
version = "0.1.0"
edition = "2021"

[dependencies]
libakuma = { path = "../libakuma" }
```

3. Create `src/main.rs`:

```rust
#![no_std]
#![no_main]

use libakuma::*;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Your code here
    exit(0);
}
```

4. Add to workspace in `userspace/Cargo.toml`:

```toml
[workspace]
members = [
    "libakuma",
    "echo2",
    "stdcheck",
    "mypackage",  # Add your package here
]
```

5. Build and install:

```bash
cargo build --release -p mypackage
# Start web server if not running
python3 -m http.server 8000 &
# In Akuma shell:
pkg install mypackage
```

## Available Packages

| Package | Description |
|---------|-------------|
| `stdcheck` | Standard library compatibility checker |
| `echo2` | Simple echo program |

## Troubleshooting

### "Error downloading" or connection refused

Ensure the Python HTTP server is running on port 8000 and is accessible from QEMU via `10.0.2.2`.

### "Empty response"

The package binary may not exist on the server. Verify the file is present in the served directory.

### Package not found after install

Check that the binary was written to `/bin/`:

```
ls /bin
```

### Historical: OOM on large downloads

Prior to the streaming download fix, `pkg install` buffered entire HTTP responses in a kernel-heap `Vec<u8>`. Downloads larger than ~10 MB would trigger OOM panics like `memory allocation of 13475840 bytes failed`. This was fixed by streaming data through a `FileWriter` that appends 4 KB chunks to disk.

