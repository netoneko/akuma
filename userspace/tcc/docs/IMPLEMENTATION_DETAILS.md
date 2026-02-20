# TCC (Tiny C Compiler) Integration with Akuma OS

This document details the integration of the Tiny C Compiler (TCC) as a self-hosted compiler within the Akuma OS userspace, targeting standard compliance via Musl libc.

## Overall Goal

To provide a fully functional C development environment on Akuma OS. This is achieved by linking TCC against `musl` libc and providing a standard sysroot.

## Key Challenges and Solutions

### 1. Musl Libc Integration

**Problem:** Standard C applications require a production-grade C library. Akuma originally used a rudimentary stub libc which was insufficient for complex software.

**Solution:** Integrated `musl` libc as the primary C library.
- TCC's sysroot (`/usr/lib`, `/usr/include`) is populated with Musl artifacts.
- The kernel was updated to support the Linux/AArch64 syscall ABI and dynamic ELF relocations (`GLOB_DAT`, `JUMP_SLOT`) required by Musl.

### 2. Clean TCC Source Tree

**Problem:** Modifying the original TCC source (`tinycc/`) makes it difficult to track upstream changes and maintain the port.

**Solution:** Achieved a zero-modification port of TCC.
- **`__arm64_clear_cache`**: Instead of patching `lib-arm64.c`, the function is aliased via a compiler define (`-D__arm64_clear_cache=__clear_cache`) in `build.rs`, and the implementation is provided in `src/libc_stubs.c`.
- **Internal Headers**: TCC's private headers (like `stdarg.h`, `tccdefs.h`) are packaged into `/usr/lib/tcc/include` and handled via include paths in `build.rs`.

### 3. Unified Distribution (`libc.tar`)

**Problem:** Managing separate archives for the compiler, its headers, and the C library was complex.

**Solution:** Created a unified `libc.tar` in `userspace/tcc/build.rs`.
- This archive merges Musl headers/libraries with TCC's internal headers and the `libtcc1.a` runtime library.
- Extracted to `/` on boot, it provides a complete development environment.

### 4. Stability and Runtime Fixes

- **Memory Alignment**: `malloc` and `realloc` in `main.rs` enforce 8-byte alignment to satisfy Musl's expectations.
- **Relocation Support**: The kernel's `elf_loader` parses `SHT_RELA` sections to correctly initialize the GOT, preventing null pointer dereferences in Musl's initialization code.

## MISSING PARTS

*   **Floating Point**: Further verification of `long double` support in TCC binaries on the target.
*   **Dynamic Linking**: Currently, all binaries are statically linked. Future work could involve implementing an ELF interpreter for dynamic loading.
