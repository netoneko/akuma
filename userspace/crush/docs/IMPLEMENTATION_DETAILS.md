# Crush Static Build Implementation Details

This document outlines the process and requirements for building a statically linked AArch64 binary for `crush` targeting Akuma OS.

## Overview

`crush` is a Go-based application. To run on Akuma, it must be compiled for the `linux/arm64` ABI and statically linked to avoid dependencies on dynamic libraries (like `ld-linux-aarch64.so.1`) which may not be present or compatible in the target environment.

## Build Environment Requirements

- **Go Toolchain:** `go1.26.1` or newer.
- **Cross-Compiler:** `aarch64-linux-musl-gcc` (available via Homebrew on macOS: `brew install musl-cross`).
- **Target Architecture:** `arm64` (AArch64).
- **Target OS:** `linux`.

## Build Command

The following command was used to produce the static binary:

```bash
cd userspace/crush/crush
CC=aarch64-linux-musl-gcc \
CGO_ENABLED=1 \
GOOS=linux \
GOARCH=arm64 \
go build -ldflags="-s -w -linkmode external -extldflags '-static'" \
-o ../../../bootstrap/bin/crush .
```

### Flag Explanations

- **`CC=aarch64-linux-musl-gcc`**: Specifies the C cross-compiler to use for CGO and external linking.
- **`CGO_ENABLED=1`**: Enables CGO, which is necessary for certain dependencies (like `sqlite` or `modernc.org/sqlite` when not using pure Go equivalents) and for external linking.
- **`GOOS=linux` & `GOARCH=arm64`**: Targets the Linux kernel ABI on 64-bit ARM.
- **`-ldflags="-s -w"`**: Strips the symbol table and DWARF debugging information to reduce binary size.
- **`-linkmode external`**: Forces the use of an external linker (the `CC` specified) instead of Go's internal linker.
- **`-extldflags '-static'`**: Passes the `-static` flag to the external linker, ensuring all libraries (including `musl`) are linked statically.

## SQLite Compatibility Fixes for Akuma

Running `modernc.org/sqlite` on Akuma OS requires several adjustments due to current kernel and VFS limitations.

### 1. Disabling WAL Mode
**Issue:** Write-Ahead Logging (WAL) requires shared memory (`mmap` with `MAP_SHARED`) and complex POSIX file locking (`F_SETLK` with `F_WRLCK`), which are not yet fully implemented or supported in the Akuma VFS.
**Fix:** Set `PRAGMA journal_mode = DELETE` in `internal/db/connect.go`. This uses a traditional rollback journal which is more compatible with simple file systems.

### 2. Bypassing File Locking
**Issue:** SQLite's default locking protocol relies on `fcntl` commands (`F_SETLK`, etc.) that may return `ENOSYS` or `EINVAL` on Akuma, causing `SQLITE_PROTOCOL (15)` errors.
**Fix:** Append `nolock=1` to the SQLite connection string (DSN) in `internal/db/connect_modernc.go`. This instructs the driver to bypass host-level file locking. Note that this is only safe if only one process accesses the database at a time.

### 3. Memory Management (Stack Size)
**Issue:** The `modernc.org/sqlite` driver (being a C-to-Go transpilation) can be stack-heavy. Standard Akuma user stacks (e.g., 1MB) may lead to stack overflows or OOM-like crashes (`exit code 137`).
**Fix:** The kernel `USER_STACK_SIZE_OVERRIDE` was increased to 8MB in `src/config.rs` to provide sufficient headroom for the SQLite engine and Go runtime.

### 4. Cache Size Tuning
**Fix:** Reduced `PRAGMA cache_size = -2000` (~2MB) to keep the memory footprint manageable within the userspace environment.

## Verification
...

After building, verify the binary type using the `file` command:

```bash
file bootstrap/bin/crush
```

Expected output:
`bootstrap/bin/crush: ELF 64-bit LSB executable, ARM aarch64, version 1 (SYSV), statically linked, stripped`

## Implementation Notes

- Initially, `CGO_ENABLED=0` was attempted to produce a pure Go static binary. However, Go's internal linker for the `linux/arm64` target on macOS still produced a dynamically linked binary with an interpreter requirement (`/lib/ld-linux-aarch64.so.1`).
- Switching to `CGO_ENABLED=1` with an explicit `musl` cross-compiler and `external` link mode successfully produced a truly standalone, statically linked ELF binary compatible with Akuma.
