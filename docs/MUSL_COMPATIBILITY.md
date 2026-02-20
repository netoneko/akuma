# Musl Libc Compatibility in Akuma OS

Akuma OS provides a standard-compliant environment for C applications by integrating `musl` libc as its primary C library.

## 1. Kernel Requirements

To support `musl`, the Akuma kernel implements the following features:

### Syscall ABI
- **Architecture**: AArch64 (ARM64).
- **Register Mapping**: Syscall number in `x8`, arguments in `x0-x5`.
- **Standards Compliance**: Syscall numbers match the Linux AArch64 standard (e.g., `writev` is 66, `exit_group` is 94).

### Thread-Local Storage (TLS)
- **Thread Pointer**: `TPIDR_EL0` is reserved for userspace TLS (managed by `musl`).
- **Kernel Tracking**: The kernel tracks the Thread ID (TID) in `TPIDRRO_EL0` (Read-Only in EL0, R/W in EL1).
- **Persistence**: The kernel ensures `TPIDR_EL0` is correctly saved and restored during context switches.

### ELF Loading and Relocations
The kernel `elf_loader` supports dynamic relocations necessary for position-independent-like binaries:
- `R_AARCH64_RELATIVE`: Simple address adjustment.
- `R_AARCH64_ABS64`: Absolute 64-bit address.
- `R_AARCH64_GLOB_DAT`: Global Offset Table (GOT) data symbols.
- `R_AARCH64_JUMP_SLOT`: PLT/GOT jump slots.

## 2. Build and Distribution

The Musl library is managed as a workspace package in `userspace/musl/`:
- **Automation**: `build.rs` handles the cross-compilation of Musl from source.
- **Packaging**: Musl artifacts are merged into a unified `libc.tar` archive alongside TCC internal headers.
- **Deployment**: `libc.tar` is extracted to `/usr` during system bootstrap, providing a complete sysroot at `/usr/lib` and `/usr/include`.

## 3. TCC Integration

The Tiny C Compiler (TCC) is fully integrated with Musl:
- **Default Linkage**: TCC links all C programs against Musl by default.
- **Clean Source**: The TCC source code (`tinycc/`) remains unmodified. Architecture-specific stubs and defines are handled in `userspace/tcc/src/libc_stubs.c` and `build.rs`.
- **Full C Support**: With Musl, TCC can compile and run complex C applications using standard POSIX APIs.
