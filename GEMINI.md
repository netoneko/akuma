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
- **Testing:** Kernel tests are located in `src/*_tests.rs`; userspace tests are within their respective directories in `userspace/`.

## Development Tools and Servers

### Rust Language Server Protocol (LSP) - rust-analyzer

`rust-analyzer` is the primary Language Server Protocol (LSP) implementation for Rust, providing powerful features for code understanding, diagnostics, and development.

*   **Functionality**: `rust-analyzer` enables features like code completion, go-to-definition, type inference, refactoring assistance, and most importantly for Gemini CLI, detailed diagnostics (checks) and build integration.
*   **Configuration**: `rust-analyzer` typically works out-of-the-box in Rust projects with a `Cargo.toml` file. While project-specific configurations (e.g., `rust-analyzer.json` or `.vscode/settings.json`) can further customize its behavior, it generally relies on standard `cargo` commands.
*   **Gemini CLI Integration**: The Gemini CLI agent can leverage `rust-analyzer`'s capabilities by executing standard `cargo` commands.
    *   **Checks and Diagnostics**: Gemini CLI will primarily use `cargo check` to perform code analysis, identify errors, and retrieve diagnostics.
    *   **Builds**: For triggering full builds, Gemini CLI will use `cargo build`.
    *   **Interpretation**: Gemini CLI is equipped to interpret the output of `cargo check` and `cargo build` to provide feedback, report errors, and understand the project's build status.

To ensure seamless operation, it is recommended to have `rust-analyzer` installed and accessible in the development environment. The Gemini CLI will assume the presence of a functional Rust toolchain and `cargo` for these operations.
