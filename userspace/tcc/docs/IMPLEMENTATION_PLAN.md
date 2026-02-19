# TinyCC on Akuma Implementation Plan

## Objective
Port TinyCC (tcc) to Akuma userspace to allow compiling C programs directly on the target system.

## Strategy
1.  **Embed TinyCC Source**: Use `tinycc` as a git submodule.
2.  **Libc Shim**: Since Akuma has no standard libc, we must provide one for TCC to function. This has been split between `userspace/tcc/src/libc_stubs.c` (for `printf` family, `str*`, `mem*`, `qsort`, `realpath` stubs, `environ`) and `userspace/tcc/src/main.rs` (for memory allocation, file I/O, process control, `getcwd`, `mmap`, `munmap`).
    *   **Memory**: `malloc`, `free`, `realloc`, `calloc` (Implemented in Rust, wrapping `libakuma` global allocator).
    *   **String/Mem**: `memcpy`, `memset`, `strlen`, `strcpy`, `strcat`, `strcmp`, `strncmp`, `strchr`, `strrchr`, `strstr`, `strpbrk`, `memcmp`, `memchr` (Implemented in C `libc_stubs.c`).
    *   **File I/O**: `fopen`, `fclose`, `fread`, `fwrite`, `fputc`, `fgetc`, `fflush`, `fseek`, `ftell`, `rewind`, `ferror`, `feof`, `freopen` (Implemented in Rust, wrapping `libakuma` file descriptors).
    *   **Output**: `printf`, `fprintf`, `sprintf`, `snprintf`, `vsnprintf`, `vfprintf` (Implemented in C `libc_stubs.c` calling Rust's `fwrite`/`fputc` for output).
    *   **System**: `exit`, `unlink`, `remove`, `rename`, `mkdir`, `system` (stub), `getenv` (stub), `getcwd`, `stat`, `fstat`, `lstat` (implemented in Rust, wrapping `libakuma` syscalls).
    *   **Dynamic Loading**: `dlopen`, `dlsym`, `dlclose` (Stubbed in C `libc_stubs.c`).
    *   **Setjmp**: `setjmp`, `longjmp` (Implemented in AArch64 assembly).
    *   **Time**: `gettimeofday` (Implemented in Rust).
    *   **Math**: `ldexp`, `ldexpl` (Implemented in Rust, or `ldexpl` alias in `math.h`).
3.  **Build System**:
    *   Uses `cc` crate in `build.rs`.
    *   Compiles `tinycc/tcc.c`, `src/libc_stubs.c`, `src/setjmp.S`.
    *   Defines `TCC_TARGET_ARM64=1`, `TCC_IS_NATIVE=1`, `ONE_SOURCE=1`, `CONFIG_TCC_STATIC=1`, `CONFIG_TCC_SEMLOCK=0`, `main=tcc_main`.
    *   Includes `tinycc`, `src`, `include` directories.
    *   Removes explicit `TCC_VERSION` definition from `build.rs` as it's now in `config.h`.
4.  **Runtime Support (Headers/Libs)**:
    *   A custom set of minimal C headers (`stddef.h`, `stdarg.h`, `stdint.h`, `stdio.h`, `stdlib.h`, `string.h`, `unistd.h`, `sys/types.h`, `sys/stat.h`, `sys/time.h`, `sys/mman.h`, `fcntl.h`, `setjmp.h`, `math.h`, `errno.h`, `ctype.h`, `limits.h`, `inttypes.h`) are provided in `userspace/tcc/include`.
    *   These headers and minimal runtime libraries (`libc.c`, `crt0.S`) for programs compiled by TCC will be placed in `../bootstrap/usr/include` and `../bootstrap/usr/lib` on the target system.

## Future Refactoring Recommendation:
Consider creating a separate `userspace/libc` project/crate to centralize these minimal C headers and their Rust/C implementations. This would promote reusability and maintainability for other userspace C applications.

## Implementation Steps

### Phase 1: Setup
- [X] Add `tinycc` submodule.
- [X] Create directory structure (`userspace/tcc`, `userspace/tcc/docs`, `userspace/tcc/examples/hello_world`, `userspace/tcc/include`, `userspace/tcc/include/sys`, `userspace/tcc/lib`, `userspace/tcc/src`).

### Phase 2: Rust Shim (The "Libc") & Headers
- [X] Create/Update `userspace/tcc/src/main.rs` to:
    - [X] Implement `FILE` struct (`#[repr(C)]`).
    - [X] Export `malloc`/`free`/`realloc`/`calloc`.
    - [X] Implement `fopen`/`fclose`/`fread`/`fwrite`/`fputc`/`fgetc`/`ungetc`/`getc`/`putc`/`putchar`/`fputs`/`fflush`/`fseek`/`ftell`/`rewind`/`ferror`/`feof`/`freopen`.
    - [X] Implement `read`/`write`/`lseek`/`getcwd`/`mmap`/`munmap`/`mprotect`.
    - [X] Implement `stat`/`fstat`/`mkdir`/`lstat`.
    - [X] Implement `gettimeofday`.
    - [X] Implement `getenv` (stub).
    - [X] Export `close` from Rust.
    - [X] Initialize `stdin`/`stdout`/`stderr` static pointers.
    - [X] Define `_start` entry point to parse args and call `tcc_main`.
- [X] Create/Update `userspace/tcc/src/libc_stubs.c` to:
    - [X] Implement `mem*` and `str*` functions (from `sqlite_stubs.c`).
    - [X] Implement `printf`/`fprintf`/`sprintf`/`snprintf`/`vsnprintf`/`vfprintf` functions (using Rust's `fwrite`/`fputc`).
    - [X] Implement `system` (stub).
    - [X] Implement `dlopen`/`dlsym`/`dlclose`/`dlerror` (stubs).
    - [X] Implement `strpbrk`.
    - [X] Implement `realpath` (stub).
    - [X] Implement `qsort` (simple bubble sort).
    - [X] Declare `environ` global variable.
- [X] Create `userspace/tcc/src/setjmp.S` for AArch64 `setjmp`/`longjmp`.
- [X] Create/Update header files in `userspace/tcc/include/` and `userspace/tcc/include/sys/`:
    - [X] `assert.h`, `ctype.h`, `errno.h`, `limits.h`, `math.h`, `stdarg.h`, `stddef.h` (add `ssize_t`, `pid_t`, `time_t`), `stdint.h`, `stdio.h` (full declarations, `freopen`), `stdlib.h` (full declarations, `qsort`), `string.h` (add `strpbrk`), `time.h`, `unistd.h` (full declarations, include `sys/types.h`, `close`), `sys/types.h` (declare `ssize_t`, `pid_t`, `time_t`), `sys/stat.h` (full declarations, struct stat), `sys/time.h` (struct timeval), `sys/mman.h` (mmap constants/declarations), `fcntl.h` (O_* flags), `setjmp.h` (jmp_buf, setjmp/longjmp prototypes), `inttypes.h`.
- [X] Create `userspace/tcc/src/config.h`.

### Phase 3: Build Script
- [X] `userspace/tcc/build.rs`:
    - [X] Compiles `tinycc/tcc.c`, `src/libc_stubs.c`, `src/setjmp.S`.
    - [X] Configures TCC defines and include paths (`-I tinycc`, `-I src`, `-I include`).

### Phase 4: Integration
- [X] Add `tcc` to `members` in `userspace/Cargo.toml`.
- [X] Update `userspace/build.sh` to:
    - [X] Add `tcc` to `MEMBERS` and `BINARIES` lists.
    - [X] Copy `userspace/tcc/include` to `../bootstrap/usr/include`.
    - [X] Copy `userspace/tcc/lib` (contains `crt0.S`, `libc.c`) to `../bootstrap/usr/lib`.
    - [X] Copy `userspace/tcc/examples/hello_world/hello.c` to `../bootstrap/hello.c`.

### Phase 5: Headers for Target (Done as part of Phase 4)

## Testing
- [ ] Compile `hello.c` on target.
- [ ] Run the produced binary.
