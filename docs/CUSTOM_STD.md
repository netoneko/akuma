# Custom Rust Standard Library for Akuma

This document describes the effort to build a custom Rust standard library that uses akuma's native syscalls, enabling programs that depend on `std` (like the Boa JavaScript engine) to run on akuma without requiring Linux syscall compatibility.

## Background

### The Problem

Akuma has its own syscall ABI that differs from Linux:

| Operation | Akuma Syscall # | Linux aarch64 # |
|-----------|-----------------|-----------------|
| EXIT      | 0               | 93              |
| READ      | 1               | 63              |
| WRITE     | 2               | 64              |
| BRK       | 3               | 214             |
| OPENAT    | 56              | 56 (same)       |
| CLOSE     | 57              | 57 (same)       |

Programs compiled with `std` (like Boa) typically link against a C library (glibc, musl) which makes Linux syscalls. These programs fail on akuma because the syscall numbers don't match.

### Solution Approaches

1. **Option A: Custom std** (this document) - Patch Rust's std to use akuma syscalls directly
2. **Option B: Fork dependencies** - Modify boa_engine to use a custom akuma-std crate
3. **Option C: Kernel compatibility** - Add Linux-compatible syscall numbers to the kernel

## Implementation Status

### What's Done

#### 1. Custom Target Specification

Created `targets/aarch64-unknown-akuma.json`:

```json
{
    "arch": "aarch64",
    "os": "akuma",
    "panic-strategy": "abort",
    "singlethread": true,
    "linker": "rust-lld",
    ...
}
```

#### 2. Patched Rust std Library

Location: `rust-std-akuma/library/`

This is a copy of Rust's standard library with akuma-specific modifications.

##### Platform Abstraction Layer (PAL)

Created `library/std/src/sys/pal/akuma/`:

- **`mod.rs`** - Main PAL module with error handling, init/cleanup
- **`syscall.rs`** - Direct syscall wrappers using akuma's syscall numbers
- **`os.rs`** - OS-level functions (getcwd, exit, getpid)
- **`time.rs`** - Time handling using the UPTIME syscall

##### Syscall Interface

The syscall module provides direct kernel access:

```rust
// library/std/src/sys/pal/akuma/syscall.rs
pub mod nr {
    pub const EXIT: u64 = 0;
    pub const READ: u64 = 1;
    pub const WRITE: u64 = 2;
    pub const BRK: u64 = 3;
    // ... etc
}

pub fn syscall6(num: u64, a0: u64, ...) -> u64 {
    unsafe {
        asm!(
            "svc #0",
            in("x8") num,
            inout("x0") a0 => ret,
            ...
        );
    }
    ret
}
```

##### Modified Modules

| Module | File | Change |
|--------|------|--------|
| PAL selection | `sys/pal/mod.rs` | Added `target_os = "akuma"` branch |
| Allocator | `sys/alloc/akuma.rs` | mmap-based allocator |
| Arguments | `sys/args/akuma.rs` | Read from process info page |
| I/O errors | `sys/io/error/akuma.rs` | Error code translation |
| stdio | `sys/stdio/akuma.rs` | Read/write to fd 0/1/2 |
| Random | `sys/random/akuma.rs` | LCG PRNG seeded from uptime |
| Thread local | `sys/thread_local/mod.rs` | Using no_threads mode |
| File descriptors | `os/fd/*.rs` | Multiple cfg exclusions |

#### 3. Akuma-std Userspace Crate

Location: `userspace/akuma-std/`

A simpler alternative that provides std-like APIs on top of libakuma. This compiles but can't directly replace the sysroot std.

### What's Remaining

The following modules still need akuma-specific implementations or proper cfg exclusions:

1. **File System (`sys/fs/`)** - Currently falls through to "unsupported"
   - Need: `File`, `OpenOptions`, `Metadata`, `ReadDir`
   - Required traits: `FromInner`, `IntoInner`, `AsInner`, `AsFd`

2. **Networking (`sys/net/`)** - Currently unsupported
   - Need: `TcpStream`, `TcpListener`, `UdpSocket`
   - Can probably skip for initial version

3. **os/fd module** - Many trait implementations reference libc
   - Partial fix in place, but File/TcpStream still broken

4. **Process (`sys/process/`)** - Needs spawn/wait support

5. **Pipes (`sys/pipe/`)** - Currently unsupported

## Building

### Prerequisites

```bash
# Install Rust source
rustup component add rust-src

# Ensure nightly toolchain
rustup install nightly
```

### Build Command

```bash
cd userspace/boa

# Set library source path
export __CARGO_TESTS_ONLY_SRC_ROOT=/path/to/akuma/rust-std-akuma/library

# Build with custom std
cargo build \
    --target ../targets/aarch64-unknown-akuma.json \
    -Zbuild-std=std,core,alloc,panic_abort
```

### Current Build Status

The build progresses through std compilation but fails on modules that use the "unsupported" fs/net implementations, which don't implement required traits like `AsFd`, `FromInner`, etc.

## Architecture

```
rust-std-akuma/library/
├── std/src/
│   ├── sys/
│   │   ├── pal/
│   │   │   ├── akuma/          # Akuma PAL (NEW)
│   │   │   │   ├── mod.rs
│   │   │   │   ├── syscall.rs
│   │   │   │   ├── os.rs
│   │   │   │   └── time.rs
│   │   │   └── mod.rs          # Modified to include akuma
│   │   ├── alloc/
│   │   │   └── akuma.rs        # mmap allocator (NEW)
│   │   ├── args/
│   │   │   └── akuma.rs        # Process info page args (NEW)
│   │   ├── io/error/
│   │   │   └── akuma.rs        # Error handling (NEW)
│   │   ├── stdio/
│   │   │   └── akuma.rs        # stdin/stdout/stderr (NEW)
│   │   ├── random/
│   │   │   └── akuma.rs        # PRNG (NEW)
│   │   └── thread_local/
│   │       └── mod.rs          # Modified for no_threads
│   └── os/
│       ├── fd/
│       │   ├── raw.rs          # Modified cfg conditions
│       │   ├── owned.rs        # Modified cfg conditions
│       │   └── mod.rs          # Modified cfg conditions
│       └── mod.rs              # Added akuma to fd module
└── ...
```

## Syscall Reference

Akuma syscall numbers used by std:

```rust
pub mod nr {
    pub const EXIT: u64 = 0;
    pub const READ: u64 = 1;
    pub const WRITE: u64 = 2;
    pub const BRK: u64 = 3;
    pub const MKDIRAT: u64 = 34;
    pub const OPENAT: u64 = 56;
    pub const CLOSE: u64 = 57;
    pub const GETDENTS64: u64 = 61;
    pub const LSEEK: u64 = 62;
    pub const FSTAT: u64 = 80;
    pub const NANOSLEEP: u64 = 101;
    pub const MUNMAP: u64 = 215;
    pub const UPTIME: u64 = 216;
    pub const MMAP: u64 = 222;
}
```

## Next Steps

To complete the custom std:

1. **Implement akuma fs module** (`sys/fs/akuma.rs`)
   - Use existing syscalls: OPENAT, CLOSE, READ, WRITE, LSEEK, FSTAT, GETDENTS64
   - Implement required traits for `File` struct

2. **Skip networking** for initial version
   - Add cfg exclusions to avoid compiling net-related code

3. **Add more cfg exclusions** to os/fd/owned.rs
   - Exclude File, TcpStream, TcpListener, UdpSocket implementations

4. **Test with a simple std program** before tackling boa

## Alternative: akuma-std Crate

The `userspace/akuma-std/` crate provides another approach:

- Named `std` in Cargo.toml
- Provides std-like APIs using libakuma syscalls
- Compiles successfully for `aarch64-unknown-none`

However, this cannot replace the sysroot std because Rust looks for `std` in the sysroot, not in dependencies. External crates doing `use std::...` won't find it.

## References

- [Rust std source structure](https://github.com/rust-lang/rust/tree/master/library/std)
- [Redox OS](https://www.redox-os.org/) - Similar approach with custom std
- [Rust embedded book](https://docs.rust-embedded.org/) - no_std development
- [Build std documentation](https://doc.rust-lang.org/nightly/cargo/reference/unstable.html#build-std)
