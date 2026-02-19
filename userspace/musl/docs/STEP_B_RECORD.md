# Step B: Musl Porting Experiment - Technical Record

This document records the actual steps taken to compile `musl` and build the first test application.

## 1. Compilation of Musl Libc

The following environment and configuration were used to build `musl` for the Akuma AArch64 target:

### Toolchain
- **Compiler**: `clang` (with `-target aarch64-linux-musl`)
- **Archiver**: `/opt/homebrew/opt/llvm/bin/llvm-ar`
- **Ranlib**: `/opt/homebrew/opt/llvm/bin/llvm-ranlib`

### Configuration Command
```bash
cd userspace/musl/musl
export CC="clang"
export CFLAGS="-target aarch64-linux-musl -Os"
export AR="/opt/homebrew/opt/llvm/bin/llvm-ar"
export RANLIB="/opt/homebrew/opt/llvm/bin/llvm-ranlib"

./configure 
    --prefix=$(pwd)/../dist 
    --disable-shared 
    --disable-debug 
    --enable-optimize=s 
    --target=aarch64-linux-musl
```

### Build and Install
```bash
make -j4
make install
```
The resulting artifacts (headers and `libc.a`) are located in `userspace/musl/dist/`.

## 2. Building the Test Application

A "Hello World" application was created to verify the kernel's new Linux-compatible features.

### Source (`hello_musl.c`)
```c
#include <stdio.h>

int main() {
    printf("Hello from Musl on Akuma OS!
");
    return 0;
}
```

### Compilation Step
```bash
clang -target aarch64-linux-musl -Os -c 
    -Iuserspace/musl/dist/include 
    userspace/musl/hello_musl.c 
    -o userspace/musl/hello_musl.o
```

### Linking Step
Because standard host linkers on macOS do not support AArch64 ELF well, `rust-lld` (provided by the Rust toolchain) was used:
```bash
/Users/netoneko/.rustup/toolchains/nightly-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/bin/rust-lld -flavor gnu 
    -static 
    userspace/musl/dist/lib/crt1.o 
    userspace/musl/dist/lib/crti.o 
    userspace/musl/hello_musl.o 
    -Luserspace/musl/dist/lib -lc 
    userspace/musl/dist/lib/crtn.o 
    -o userspace/musl/hello_musl.bin
```

## 3. Verification Details
- **Binary Format**: ELF 64-bit LSB executable, ARM aarch64, version 1 (SYSV), statically linked.
- **Deployment**: Copied to `bootstrap/bin/` and synced to `disk.img` using `scripts/populate_disk.sh`.
- **Target Path**: `/bin/hello_musl.bin` inside Akuma OS.
