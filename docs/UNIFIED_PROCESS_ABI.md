# Plan: Unified Linux-Compatible Process ABI

This document outlines the plan to unify Akuma's process spawning and argument passing with the standard Linux AArch64 ABI.

## 1. Goal
Eliminate the custom `ProcessInfo` argument passing mechanism and use the standard Linux stack layout for `argc`, `argv`, `envp`, and `auxv`. This ensures that both native Akuma applications and standard Linux binaries (linked with musl) use the same interface.

## 2. Kernel Changes

### A. Refactor `ProcessInfo`
- Remove `argv_data` and `cwd_data` from `ProcessInfo`.
- Keep only immutable identity fields: `pid`, `ppid`, `box_id`.
- Reduce the struct size or keep it 1KB but mostly reserved.

### B. Shared `StackBuilder`
- Extract the stack setup logic from `src/elf_loader.rs` into a reusable module or helper.
- Support building stacks from both `Vec<String>` (internal) and `char**` pointers (from syscalls).
- Ensure 16-byte alignment for the AArch64 stack pointer.

### C. Syscall Alignment
- Update `sys_spawn` to accept `char** argv` and `char** envp` instead of a flat buffer.
- Ensure `sys_execve` uses the same logic.
- Support `envp` (environment variables) properly.

## 3. Userspace Changes (`libakuma`)

### A. Entry Point Refactoring
- Update the assembly entry point (`_start`) to preserve the initial `sp`.
- Pass the initial `sp` to a new `libakuma_init(stack_ptr: usize)` function.
- `libakuma_init` will parse `argc`, `argv`, and `envp` directly from the stack.

### B. Syscall Wrappers
- Update `spawn` to build a `char**` array before calling the syscall.

## 4. Execution Steps

1.  **Step 1: Kernel `StackBuilder`**: Move and generalize stack setup logic.
2.  **Step 2: `libakuma` Entry**: Update assembly and add stack parsing logic.
3.  **Step 3: Syscall Update**: Change `spawn` and `execve` signatures and implementations.
4.  **Step 4: Cleanup**: Remove old `argv_data` logic and update tests.

## 5. Benefits
- **Compatibility**: Standard Linux binaries will work without hacks.
- **Flexibility**: No more 744-byte limit on arguments.
- **Robustness**: Uses the battle-tested Linux process entry model.
