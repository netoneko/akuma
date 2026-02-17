# Tiny C Compiler (TCC) for Akuma OS

This directory contains the Tiny C Compiler (TCC) ported to run as a userspace application on the Akuma operating system. TCC allows for compiling simple C programs directly on the Akuma target system.

## Components

- `tinycc/`: Git submodule containing the upstream TinyCC source code.
- `src/main.rs`: The Rust entry point for the `tcc` binary. It handles argument parsing, initialization of standard I/O streams, and exposes a minimal libc interface (memory allocation, file I/O, process control functions) as `extern "C"` functions for the C-based TCC core.
- `src/libc_stubs.c`: Provides additional C standard library stubs (string/memory manipulation, `printf` family, `qsort`, `realpath` stub, `environ`) that are linked with the TCC core.
- `src/setjmp.S`: AArch64 assembly implementation of `setjmp` and `longjmp` for TCC's internal error handling.
- `src/config.h`: Configuration header for TinyCC, defining target-specific settings.
- `include/`: Contains minimal C standard library headers (`stdio.h`, `stdlib.h`, `string.h`, `unistd.h`, `sys/types.h`, `sys/stat.h`, `sys/time.h`, `sys/mman.h`, `fcntl.h`, `setjmp.h`, `math.h`, `errno.h`, `ctype.h`, `limits.h`, `inttypes.h`) adapted for the Akuma `no_std` environment. These headers are essential for TCC's compilation process.
- `lib/`: Contains `crt0.S` (minimal C runtime startup for compiled programs) and `libc.c` (minimal C library for compiled programs, providing basic syscall wrappers for `printf`, `exit`, etc.). These files are intended to be compiled and linked by `tcc` on the target system.
- `examples/hello_world/hello.c`: A sample C "Hello World" program to demonstrate TCC's compilation capabilities.

## Build Process

The `tcc` binary is built using `cargo build --release -p tcc` from the `userspace/` directory. The `build.rs` script compiles the C/Assembly sources (`tinycc/tcc.c`, `src/libc_stubs.c`, `src/setjmp.S`) and links them into the Rust binary.

During the overall Akuma userspace build (`userspace/build.sh`), the `tcc` executable, along with its custom headers (`include/`) and minimal C runtime libraries (`lib/`), are copied to the `../bootstrap/bin`, `../bootstrap/usr/include`, and `../bootstrap/usr/lib` directories, respectively. These directories are then used to create the final disk image for the Akuma OS.

## Usage on Akuma

Once Akuma is running, you can use the `tcc` binary to compile C programs:

```bash
# Example: Compile hello_world.c
tcc -o hello /hello.c /usr/lib/crt0.S /usr/lib/libc.c

# Run the compiled program
/hello
```

**Note**: The included `libc.c` and `crt0.S` in `lib/` provide a very minimal C runtime for programs compiled by `tcc`. They wrap `libakuma` syscalls directly. Complex C programs requiring a full POSIX-compliant libc may not compile or run correctly without further libc development.
