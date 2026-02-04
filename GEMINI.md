# Akuma OS - Gemini Context

This document provides a high-level overview of the Akuma project for AI assistants and contributors.

## Project Structure

Akuma is a Rust-based operating system targeting the AArch64 architecture (virtio-based QEMU).

- **Kernel (`src/`):** The core kernel logic, including memory management (PMM, MMU, Allocator), scheduling (Threading, Executor), VirtIO drivers, and syscall handling.
- **Userspace (`userspace/`):** Independent applications and libraries that run in unprivileged mode.
    - **libakuma:** The core userspace library that provides the interface for applications to communicate with the kernel via system calls.
- **Documentation (`docs/`):** Detailed architectural notes, bug analyses, and design decisions.
- **Scripts (`scripts/`):** Tooling for building, running, and debugging the OS.

## Technical Environment

### Rust `no_std`
The kernel is a `no_std` environment. 
- Avoid using the standard library (`std`).
- Use `core` and `alloc` crates.
- Dynamic memory allocation is managed by the custom allocator in `src/allocator.rs`.
- Be mindful of OOM (Out of Memory) conditions and stack limits.

### Thread Safety & Concurrency
Akuma is designed for multitasking and multi-threading.
- **Locking:** Use architectural-aware primitives (like Spinlocks or Mutexes that disable interrupts where necessary).
- **Atomic Operations:** Prefer atomic types for simple state tracking.
- **Interrupts:** Ensure critical sections are protected from interrupt preemption.
- **Context Switching:** Managed in `src/threading.rs`.

## Communication: Kernel & Userspace

Userspace applications interact with the kernel exclusively through system calls defined in `src/syscall.rs`. 
- `libakuma` abstracts these syscalls into a more idiomatic Rust API for userspace apps.
- Memory is isolated via MMU; data transfer between kernel and userspace requires careful validation of pointers and lengths.

## Common Tasks
- **Building:** Use `scripts/run.sh` to compile and launch in QEMU.
- **Debugging:** Use `scripts/run_with_gdb.sh` and refer to `docs/AI_DEBUGGING.md`.
- **Testing:** Kernel tests are located in `src/*_tests.rs`; userspace tests are within their respective directories in `userspace/`.
