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

## 6. Challenges and Implementation Lessons (Post-Implementation)

The transition to the unified ABI encountered several critical technical hurdles that required deep debugging:

### A. TTBR0 and ASID Masking
When implementing stricter user pointer validation (checking if pages are actually mapped), we initially used the raw value of the `TTBR0_EL1` register. 
- **The Issue**: On AArch64, the top 16 bits of `TTBR0_EL1` contain the Address Space Identifier (ASID). Treating this as a raw physical address caused the kernel to dereference garbage pointers, leading to EL1 synchronization exceptions (Sync from EL1, FAR with high bits set).
- **The Fix**: All page table walks now explicitly mask the ASID bits (`ttbr0 & 0x0000_FFFF_FFFF_F000`) before treating the value as a physical address.

### B. Mismatched SPAWN Syscall Signatures
A subtle bug occurred where `libakuma` was updated to pass pointer arrays (`char**`) to the custom `SPAWN` syscall, but the kernel handler was still expecting a flat null-separated buffer.
- **The Issue**: This caused `sys_spawn` to interpret pointers as character data, leading to "not null terminated" errors and failed process creation (`spawn returned None`).
- **The Fix**: Unified both `EXECVE` and `SPAWN` to use a shared `parse_argv_array` helper that correctly traverses pointer arrays in userspace memory.

### C. String Visibility and Memory Barriers
During kernel-side ABI tests (`test_linux_process_abi`), strings written to physical memory via `phys_to_virt` were not always visible to the MMU immediately after mapping.
- **The Issue**: `copy_from_user_str` would return zeros or stale data because the physical memory writes hadn't fully propagated to the point of coherency before the MMU-based access occurred in the syscall handler.
- **The Fix**: Inserted explicit memory barriers (`dsb ish`, `isb`) after writing test data to ensure full visibility before simulating syscalls.

### D. ELF Mapping Validation
The move to 16-byte stack alignment and standard Linux layout required rigorous validation of the `INITIAL_SP`.
- Any misalignment in the `StackBuilder` would cause immediate crashes in the userspace entry assembly before `main` could even be reached.
- Implementation of `is_current_user_range_mapped` in the kernel provides a last line of defense against invalid user pointers causing kernel panics, effectively hardening the kernel against accidental or malicious memory access.
