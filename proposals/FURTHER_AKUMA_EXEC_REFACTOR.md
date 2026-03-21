# Refactor Plan: Reorganizing `crates/akuma-exec/src/process/mod.rs`

## Goal
The `mod.rs` file in `akuma-exec/src/process` has grown to over 2,400 lines, making it difficult to maintain. The goal is to decompose this file into smaller, specialized modules while maintaining the existing API and functionality.

## Proposed Module Structure

### 1. `stats.rs` (Syscall Statistics)
**Responsibilities:** Tracking and reporting syscall performance and frequency.
**Move from `mod.rs`:**
- `ProcessSyscallStats` struct and implementation.
- `enable_process_syscall_stats` / `process_syscall_stats_enabled`.
- `dump_running_process_stats`.
- `syscall_name` helper.

### 2. `fd.rs` (File Descriptor Management)
**Responsibilities:** Managing the per-process and shared file descriptor tables.
**Move from `mod.rs`:**
- `SharedFdTable` struct and implementation.
- `Process` methods for FD manipulation: `alloc_fd`, `get_fd`, `remove_fd`, `set_fd`, `swap_fd`, `update_fd`, `set_cloexec`, `clear_cloexec`, `is_cloexec`, `set_nonblock`, `clear_nonblock`, `is_nonblock`, `close_cloexec_fds`, `fd_table`.

### 3. `spawn.rs` (Process Spawning)
**Responsibilities:** High-level APIs for creating new processes with associated threads and I/O channels.
**Move from `mod.rs`:**
- `spawn_process`.
- `spawn_process_with_channel`.
- `spawn_process_with_channel_cwd`.
- `spawn_process_with_channel_ext`.

### 4. `exec.rs` (Execution Helpers)
**Responsibilities:** Synchronous and asynchronous helpers for executing binaries and streaming output.
**Move from `mod.rs`:**
- `exec_with_io` / `exec_with_io_cwd`.
- `exec`.
- `exec_async` / `exec_async_cwd`.
- `exec_streaming` / `exec_streaming_cwd`.
- `reattach_process` / `reattach_process_ext`.

### 5. `image.rs` (Process Image Management)
**Responsibilities:** Logic for replacing the address space image (core of `execve`).
**Move from `mod.rs`:**
- `Process::replace_image`.
- `Process::replace_image_from_path`.
- `compute_heap_lazy_size`.

### 6. `mod.rs` (Core Lifecycle & Module Entry)
**Responsibilities:** Kernel-level process initialization, the core `Process` struct, and user-mode entry.
**Keep in `mod.rs`:**
- `Process` struct definition.
- `Process::from_elf` / `Process::from_elf_path` (initial creation).
- `Process::run` and `Process::prepare_for_execution`.
- `Process` I/O methods: `set_stdin`, `read_stdin`, `write_stdout`, `take_stdout`, `reset_io`.
- Memory management methods: `get_brk`, `set_brk`.
- `enter_user_mode` (arch-specific assembly).
- `init` and `on_thread_cleanup`.
- Module re-exports and sub-module declarations.

## Implementation Steps

1. **Phase 1: Statistics Extraction**
   - Create `stats.rs`.
   - Move stats logic.
   - Update `mod.rs` to re-export.
   - Verify with `cargo check`.

2. **Phase 2: FD Table Extraction**
   - Create `fd.rs`.
   - Move `SharedFdTable` and associated `Process` methods.
   - Ensure `with_irqs_disabled` and necessary imports are handled.

3. **Phase 3: Image Replacement Extraction**
   - Create `image.rs`.
   - Move `replace_image` methods. This will require careful handling of `Process` method visibility.

4. **Phase 4: Spawning & Execution Helpers**
   - Create `spawn.rs` and `exec.rs`.
   - Move high-level wrapper functions.

5. **Phase 5: Final Cleanup**
   - Review `mod.rs` for any remaining "dead wood".
   - Ensure all public APIs remain compatible.

## Benefits
- **Improved Maintainability:** Files will be 200-500 lines instead of 2,400.
- **Better Compile Times:** Smaller units of compilation (incremental).
- **Logical Isolation:** Changes to syscall stats won't require touching core process lifecycle code.
