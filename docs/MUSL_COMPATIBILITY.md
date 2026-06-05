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

Musl is **sourced from Alpine's apk**, not built in-tree — there is no
`userspace/musl` package anymore:
- **Host build**: `userspace/tcc/build.rs` downloads the pinned Alpine aarch64
  `musl-dev` apk and extracts only `usr/include`, which it uses to cross-compile
  tcc (`-nostdinc -I <musl headers>`). The apk is cached under
  `userspace/tcc/vendor/`.
- **On Akuma**: install the libc + startup files + headers with
  `apk add musl-dev` (same package/version the build pulls). Akuma no longer
  ships a musl sysroot of its own (`libc.tar` was retired).
- **What we ship**: only `libtcc1.tar` (tcc's runtime `libtcc1.a` + tcc's
  internal headers like `tccdefs.h`). Combined with `apk add musl-dev`, that is
  the complete toolchain for our tcc.

> Rationale: Alpine's musl is already ABI-compatible with Akuma's Linux-AArch64
> syscall layer (verified — a `tcc -static` binary linked against apk `libc.a`
> runs on Akuma), so maintaining a separate in-tree musl build added cost with
> no benefit.

## 3. TCC Integration

The Tiny C Compiler (TCC) is fully integrated with Musl:
- **Default Linkage**: TCC links all C programs against Musl by default.
- **Clean Source**: The TCC source code (`tinycc/`) remains unmodified. Architecture-specific stubs and defines are handled in `userspace/tcc/src/libc_stubs.c` and `build.rs`.
- **Full C Support**: With Musl, TCC can compile and run complex C applications using standard POSIX APIs.
