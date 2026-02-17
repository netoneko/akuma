# TinyCC on Akuma Implementation Plan

## Objective
Port TinyCC (tcc) to Akuma userspace to allow compiling C programs directly on the target system.

## Strategy
1.  **Embed TinyCC Source**: Use `tinycc` as a git submodule.
2.  **Libc Shim**: Since Akuma has no standard libc, we must provide one for TCC to function.
    *   **Memory**: `malloc`, `free`, `realloc` (Wrap `libakuma` global allocator).
    *   **String/Mem**: `memcpy`, `memset`, `strlen`, etc. (Reuse/adapt `sqlite_stubs.c` or implement in Rust).
    *   **File I/O**: `fopen`, `fread`, `fwrite`, `fclose`, `fseek`, `ftell`, `fflush`. Implement a minimal `FILE*` wrapper around `libakuma` file descriptors.
    *   **Output**: `printf`, `fprintf`, `vfprintf` (Wrap `libakuma::print`/`write`).
    *   **System**: `exit`, `unlink`.
    *   **Dynamic Loading**: `dlopen`, `dlsym`, `dlclose` (Stub out for now, or minimal impl if needed for TCC's internal operation).
3.  **Build System**:
    *   Use `cc` crate in `build.rs`.
    *   Compile `tcc` sources with `-nostdinc -ffreestanding`.
    *   Define `TCC_TARGET_ARM64` and `TCC_OS_AKUMA` (or similar, or just treat as generic linux-like).
    *   Link against our Rust shim exports.
4.  **Runtime Support (Headers/Libs)**:
    *   TCC needs system headers to compile programs.
    *   We need to populate `/usr/include` (or a custom path provided to tcc via `-I`) with minimal headers:
        *   `stddef.h`, `stdarg.h`, `stdint.h`, `stdbool.h`
        *   `stdio.h`, `stdlib.h`, `string.h` (containing prototypes for our shim functions).
    *   We need `libtcc1.a` (TCC's runtime library) or equivalent support functions compiled and available.

## Implementation Steps

### Phase 1: Setup
- [ ] Add `tinycc` submodule.
- [ ] Create directory structure.

### Phase 2: Rust Shim (The "Libc")
- [ ] `src/lib.rs` / `src/main.rs`:
    - [ ] Export `malloc`/`free`/`realloc`.
    - [ ] Implement `FILE` struct and `fopen`/`fclose`/`fread`/`fwrite`/`fputc`/`fgetc`.
    - [ ] Implement `printf` family.
    - [ ] Export `mem*` and `str*` functions (maybe from a C file if easier for varargs or complex logic, or Rust).

### Phase 3: Build Script
- [ ] `build.rs`:
    - [ ] Compile `tinycc/libtcc.c` (or `tcc.c` if we want the driver).
    - [ ] Compile `shim.c` (for things easier done in C).
    - [ ] Configure TCC defines: `TCC_TARGET_ARM64`, `CONFIG_TCC_STATIC` (maybe).

### Phase 4: Integration
- [ ] `src/main.rs` calls `tcc_main` (renamed from `main` in `tcc.c`).
- [ ] Ensure arguments are passed correctly.

### Phase 5: Headers for Target
- [ ] Create `userspace/tcc/include`.
- [ ] Add basic headers.
- [ ] Ensure `make_disk.sh` or build process copies these to the disk image.

## Testing
- [ ] Compile `hello.c` on target.
- [ ] Run the produced binary.
