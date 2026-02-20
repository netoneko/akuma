# Step B: Musl Porting Experiment

Once the kernel is prepared, we will compile `musl` and attempt to run a standalone "Hello World" linked against it.

## 1. Cross-Compilation Environment
We need a toolchain capable of building `musl` for `aarch64-linux-musl`.

**Action:**
- Use `aarch64-linux-musl-gcc` or `clang` with appropriate target flags.
- Configure `musl` with:
  ```bash
  ./configure 
      CROSS_COMPILE=aarch64-linux-musl- 
      --disable-shared 
      --disable-debug 
      --enable-optimize=s 
      --prefix=$(pwd)/dist
  ```
- Run `make` and `make install`.

## 2. Minimal Syscall Shim (if needed)
If we encounter syscalls that Akuma doesn't support yet (e.g., `set_tid_address`, `rt_sigprocmask`), we may need to add stubs to the kernel or patch `musl`'s `src/internal/syscall.h` to redirect them to a no-op handler.

## 3. Linking the First Test Application
We will manually link a simple C file using the newly built `musl` artifacts.

**Command Example:**
```bash
aarch64-linux-musl-gcc -static -nostdlib 
    hello.c 
    userspace/musl/dist/lib/crt1.o 
    userspace/musl/dist/lib/crti.o 
    userspace/musl/dist/lib/crtn.o 
    -Luserspace/musl/dist/lib -lc 
    -o hello_musl.bin
```

## 4. Execution and Debugging
- Load `hello_musl.bin` onto the Akuma disk image.
- Run it and observe the kernel logs.
- Use `GDB` or kernel tracing to identify where it hangs or crashes.
- Common failure points:
    - Early initialization in `__init_libc`.
    - `malloc` initialization failing due to `brk`/`mmap` issues.
    - Printing failing due to `ioctl` or `writev` expectations.
