# Unified Linux-Compatible Process ABI

This document outlines the implementation of Akuma's unified process spawning and argument passing mechanism, aligned with the standard Linux AArch64 ABI.

## 1. Goal
Eliminate the custom `ProcessInfo` argument passing mechanism and use the standard Linux stack layout for `argc`, `argv`, `envp`, and `auxv`. This ensures that both native Akuma applications and standard Linux binaries (linked with musl) use the same interface.

## 2. Kernel Changes

### A. Refactored `ProcessInfo`
- Removed `argv_data` and `cwd_data` from `ProcessInfo`.
- Only immutable identity fields remain: `pid`, `ppid`, `box_id`.
- Asserted at compile-time to be under 1KB.

### B. Shared `StackBuilder`
- Implemented `UserStack` helper in `src/elf_loader.rs`.
- Supports building compliant frames from both internal string vectors and userspace pointer arrays (`char**`).
- Ensures strict 16-byte alignment for the AArch64 stack pointer.

### C. Syscall Unification
- Updated `sys_spawn` and `sys_execve` to accept `char** argv` and `char** envp`.
- Implemented `parse_argv_array` to safely traverse userspace pointer arrays.
- Added environment variable (`envp`) support across all process creation paths.

## 3. Userspace Changes (`libakuma`)

### A. Entry Point
- Updated the assembly entry point (`_start`) to preserve the initial `sp`.
- `libakuma_init` now parses `argc`, `argv`, and `envp` directly from the stack.
- Native apps define `extern "C" fn main()` instead of `_start`.

### B. Syscall Wrappers
- `libakuma::spawn` now builds standard null-terminated pointer arrays in userspace memory before invoking the kernel.

## 4. Execution Steps

1.  **[DONE] Step 1: Kernel `StackBuilder`**: Move and generalize stack setup logic.
2.  **[DONE] Step 2: `libakuma` Entry**: Update assembly and add stack parsing logic.
3.  **[DONE] Step 3: Syscall Update**: Change `spawn` and `execve` signatures and implementations.
4.  **[DONE] Step 4: Cleanup**: Remove old `argv_data` logic and update tests.

## 5. Challenges and Implementation Lessons

### A. TTBR0 and ASID Masking
When implementing user pointer validation (checking if pages are actually mapped), we initially used the raw value of the `TTBR0_EL1` register. 
- **The Issue**: On AArch64, the top 16 bits of `TTBR0_EL1` contain the Address Space Identifier (ASID). Treating this as a raw physical address caused the kernel to dereference garbage pointers, leading to EL1 synchronization exceptions.
- **The Fix**: All page table walks now explicitly mask the ASID bits (`ttbr0 & 0x0000_FFFF_FFFF_F000`) before treating the value as a physical address.

### B. Mismatched SPAWN Syscall Signatures
A bug occurred where `libakuma` was updated to pass pointer arrays (`char**`) to the custom `SPAWN` syscall, but the kernel handler was still expecting a flat null-separated buffer.
- **The Issue**: This caused `sys_spawn` to interpret pointers as character data, leading to "not null terminated" errors and failed process creation.
- **The Fix**: Unified both `EXECVE` and `SPAWN` to use a shared `parse_argv_array` helper.

### C. String Visibility and Memory Barriers
During kernel-side ABI tests, strings written to physical memory were not always visible to the MMU immediately after mapping.
- **The Issue**: `copy_from_user_str` would return zeros because the physical memory writes hadn't fully propagated to the point of coherency.
- **The Fix**: Inserted explicit memory barriers (`dsb ish`, `isb`) after writing test data.

## 6. Final Implementation Status

The Unified Process ABI is now fully implemented and verified:

### A. Interoperability
- **Custom SPAWN**: Native Akuma applications use `libakuma::spawn`, which builds a standard Linux-compatible stack frame for the child.
- **Linux vfork/execve**: The kernel supports the standard `vfork` pattern where a process calls `CLONE` (bridge mode) followed by `EXECVE`.
- **Inter-system waiting**: Native processes can wait for Linux/musl processes and vice versa using the unified PID space and `sys_wait4`.

### B. Bridge Mode
When a process calls `sys_execve` from a "bridge" thread (simulated `vfork`), the kernel spawns the new process normally, registers the child's output channel with the parent, and returns the **newly spawned PID** to the parent.

### C. Verification
- `userspace/elftest` verifies both regular `spawn` and Linux-style `vfork+execve` for both native and musl binaries.
- Standard Linux binaries (like GNU Make) now use the same stack-based argument passing as native Akuma utilities.
- All tests pass, confirming the robustness of the unified ABI.
