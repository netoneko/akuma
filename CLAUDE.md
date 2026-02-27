# Akuma OS - Claude Code Context

Akuma is a bare-metal Rust operating system targeting AArch64 (QEMU virt machine). It includes in-kernel SSH, networking, containers, a JS interpreter, a C compiler, a git clone, and an AI coding assistant (meow).

## Project Layout

- `src/` — Kernel (~56k lines of Rust, `no_std`)
- `userspace/` — Userspace apps and libraries (ELF binaries, musl libc)
  - `libakuma/` — Rust syscall wrapper library
  - `meow/` — AI coding assistant
  - `quickjs/` — JavaScript interpreter
  - `tcc/` — Tiny C Compiler
  - `herd/` — Container system
  - `sbase/` — Unix utilities
  - `dash/` — POSIX shell
  - `paws/` — Main interactive shell
- `docs/` — Architecture notes and design docs (103 files)
- `scripts/` — Build and debug scripts
- `config/` — Config files
- `linker.ld` — Kernel linker script

## Build & Run

```bash
cargo build --release          # Build kernel
cargo run --release            # Build and run in QEMU
scripts/run.sh                 # Convenience wrapper
scripts/create_disk.sh         # (Re)create ext2 disk image
scripts/populate_disk.sh       # Populate disk with userspace binaries
userspace/build.sh             # Build all userspace binaries
```

Use `cargo check` for fast diagnostics without a full build.

Target: `aarch64-unknown-none` (set in `rust-toolchain.toml`, nightly Rust required).

## Kernel Architecture

**Execution model:** Fixed 32-thread pool with preemptive 10ms round-robin scheduling. Hybrid model: threads + embassy async executor.

**Key kernel modules:**
- `src/main.rs` — Entry point and kernel init
- `src/threading.rs` — Thread pool, scheduler, context switch
- `src/process.rs` — Process/PCB management, ELF execution
- `src/syscall.rs` — Linux AArch64 ABI syscall interface (~40 syscalls)
- `src/exceptions.rs` — Exception vectors, IRQ handling
- `src/allocator.rs` — Heap (talc), OOM handling
- `src/pmm.rs` — Physical memory manager
- `src/mmu.rs` — MMU, userspace address space isolation
- `src/elf_loader.rs` — ELF parser and loader (static, static-PIE, dynamic)
- `src/smoltcp_net.rs` — TCP/IP stack (smoltcp)
- `src/socket.rs` — Socket syscall layer
- `src/vfs/` — VFS: ext2 (`vfs/ext2.rs`), memfs, procfs
- `src/ssh/` — In-kernel SSH-2 server (port 2222)
- `src/shell/` — Interactive shell and built-in commands
- `src/config.rs` — Tunable kernel parameters

**Memory layout:**
- `0x4000_0000` — Kernel code/data
- Kernel heap: ~120 MB (talc allocator)
- Per-process: user stack 256 KB with guard page

## no_std Rules

The kernel is `no_std`. Always:
- Use `core` and `alloc`, never `std`
- Be mindful of stack depth — default thread stack is 32 KB, async threads 512 KB
- Watch for OOM; the allocator can fail
- Avoid recursion in kernel code

## Concurrency

- Use spinlocks / interrupt-disabling mutexes for shared state
- Prefer atomics for simple flags
- Context switching is in `src/threading.rs`
- SSH and HTTP services each run in dedicated threads

## Userspace ↔ Kernel

- Syscalls only; defined in `src/syscall.rs`
- `libakuma` wraps syscalls idiomatically for Rust userspace code
- Kernel validates all userspace pointers before dereferencing

## Subsystems Quick Reference

| Subsystem | Location | Notes |
|-----------|----------|-------|
| SSH server | `src/ssh/` | SSH-2, Ed25519, AES-128-CTR, port 2222 |
| Networking | `src/smoltcp_net.rs` | smoltcp TCP/IP, VirtIO-net |
| Containers | `userspace/herd/` | Process isolation (herd/box) |
| JS engine | `userspace/quickjs/` | QuickJS |
| C compiler | `userspace/tcc/` | TCC + musl (static linking) |
| Dynamic linker | `src/elf_loader.rs` | PT_INTERP + ld-musl-aarch64.so.1 at 0x3000_0000 |
| Git | `userspace/scratch/` | Git clone; needs 256+ KB stack (zlib) |
| AI assistant | `userspace/meow/` | meow coding assistant |
| Shell | `src/shell/` + `userspace/paws/` + `userspace/dash/` | In-kernel shell + POSIX dash |
| VFS | `src/vfs/` | ext2, memfs, procfs |

## Testing

In-kernel tests live in `src/*_tests.rs`. Run with standard cargo test mechanisms. Userspace tests are in their respective `userspace/` subdirectories.

## Current Branch

`improve-dash-compatibility` — work on making dash shell work correctly on Akuma.
