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

### 3. Distribution Archives

**Problem:** Managing separate archives for the compiler, its headers, and the C library was complex. Additionally, Alpine Linux's TCC package for aarch64 does not ship `libtcc1.a`, making TCC unusable without manual intervention.

**Solution:** The build produces two tar archives in `userspace/tcc/build.rs`:
- **`libc.tar`** — Full sysroot: merges Musl headers/libraries with TCC's internal headers and the `libtcc1.a` runtime library. Extracted to `/` on boot for a complete development environment.
- **`libtcc1.tar`** — Standalone archive containing only `usr/lib/tcc/libtcc1.a`. Can be extracted on top of any system that has TCC installed but is missing the runtime library (e.g. Alpine aarch64 containers).

See `docs/LIBTCC1.md` for details on the `libtcc1.a` problem and usage.

### 4. Stability and Runtime Fixes

- **Memory Alignment**: `malloc` and `realloc` in `main.rs` enforce 8-byte alignment to satisfy Musl's expectations.
- **Relocation Support**: The kernel's `elf_loader` parses `SHT_RELA` sections to correctly initialize the GOT, preventing null pointer dereferences in Musl's initialization code.

### 5. Static vs Dynamic Linking

TCC defaults to dynamic linking (producing binaries that reference `libc.so` and require `/lib/ld-musl-aarch64.so.1` at runtime). Akuma does not have a userspace dynamic linker, so dynamically linked binaries crash — GOT/PLT entries for libc functions resolve to address 0, causing an instruction abort at `ELR=0x0`.

**Workaround:** Always pass `-static` when compiling on Akuma:
```
tcc -static hello.c -o hello
```

This links against `libc.a` and produces a fully standalone binary.

## Known Issues

*   **Alpine TCC on Akuma:** Alpine's dynamically-linked TCC binary (`/usr/bin/tcc`, v0.9.28rc) crashes with a data abort during compilation when run on Akuma. This is a kernel compatibility issue with the musl dynamic linker, not a TCC bug.
*   **Dynamic linking:** Binaries compiled without `-static` crash on Akuma (no dynamic linker). Future work could involve implementing a userspace ELF interpreter.
*   **Floating Point**: Further verification of `long double` support in TCC binaries on the target.
