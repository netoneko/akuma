# Package Management

This document describes the package management system in Akuma for userspace binaries.

## Overview

Akuma includes a simple package manager (`pkg`) that downloads and installs userspace binaries from an HTTP server. Packages are ELF binaries built for the `aarch64-unknown-none` target and installed to `/bin/`.

## Architecture

```
┌─────────────────────┐     HTTP GET      ┌─────────────────────┐
│     Akuma Shell     │ ───────────────▶  │   Python HTTP       │
│     (pkg install)   │                   │   Server (port 8000)│
└─────────────────────┘                   └─────────────────────┘
         │                                          │
         │                                          │
         ▼                                          ▼
┌─────────────────────┐                   ┌─────────────────────┐
│      /bin/<pkg>     │                   │ userspace/target/   │
│   (installed ELF)   │                   │ aarch64-.../release │
└─────────────────────┘                   └─────────────────────┘
```

## Using the Package Manager

### Installing a Package

```
pkg install <package-name>
```

This downloads the binary from `http://10.0.2.2:8000/target/aarch64-unknown-none/release/<package>` and saves it to `/bin/<package>`.

### Examples

```
pkg install stdcheck
pkg install echo2
```

### Running Without Arguments

Running `pkg` without arguments shows usage information:

```
Usage: pkg install <package>

Examples:
  pkg install stdcheck
  pkg install echo2
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

To make packages available for `pkg install`, run a web server from the userspace directory:

```bash
cd userspace
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

Ensure the Python HTTP server is running on port 8000 from the `userspace/` directory.

### "Empty response"

The package binary may not exist. Verify the package is built:

```bash
ls userspace/target/aarch64-unknown-none/release/
```

### Package not found after install

Check that the binary was written to `/bin/`:

```
ls /bin
```

