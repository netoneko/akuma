# Implementation Issues: Unified Process ABI

This document tracks the technical challenges and bugs encountered during the migration from the custom `ProcessInfo` passing mechanism to the standard Linux AArch64 Stack ABI.

## 1. TTBR0 ASID Corruption (Kernel Panic)
**Symptom**: Kernel crashed with `Sync from EL1: EC=0x25` (Data Abort) and a `FAR` (Fault Address Register) containing high-bit noise (e.g., `0x2000043ffb000`) when validating user pointers.

**Cause**: The kernel's pointer validation logic (`is_current_user_range_mapped`) used the raw value of the `TTBR0_EL1` register. On AArch64, the top 16 bits of this register contain the Address Space Identifier (ASID). Treating the raw register value as a physical address caused the page table walker to dereference garbage memory.

**Resolution**: Implemented explicit masking of ASID bits before page table traversal:
```rust
let l0_addr = ttbr0 & 0x0000_FFFF_FFFF_F000; // Mask bits [63:48] and [11:0]
```

## 2. SPAWN/EXECVE Signature Mismatch
**Symptom**: Userspace reported `spawn returned None`, and the kernel logged `copy_from_user_str: not null terminated within 512 bytes`.

**Cause**: `libakuma` was updated to pass standard C-style pointer arrays (`char** argv`) to align with Linux requirements. However, the existing `sys_spawn` handler in the kernel still expected a single flat buffer containing null-separated strings. The kernel was trying to read the binary pointers as if they were ASCII characters.

**Resolution**: Created a unified `parse_argv_array` helper in `src/syscall.rs` that traverses the pointer array in userspace, dereferencing each `char*` individually.

## 3. String Visibility in ABI Tests
**Symptom**: Kernel-side unit tests (`test_linux_process_abi`) reported that virtual addresses mapped to test frames read as zeros, despite strings being copied there just before.

**Cause**: Lack of memory barriers. The strings were written to physical memory via a kernel virtual address (identity map), but the MMU-based access in the syscall handler occurred before the writes were coherent/visible to the translation table walker.

**Resolution**: Inserted data synchronization barriers and instruction barriers after writing test data:
```rust
core::arch::asm!("dsb ish", "isb");
```

## 4. Missing Null Termination in libakuma Wrappers
**Symptom**: `/bin/hello_musl.bin` could be run from the shell but `elftest` reported it was missing. `chdir` and `open` calls from Rust-based userspace apps were intermittently failing with `ENOENT`.

**Cause**: Rust's `&str` type is not null-terminated. Many `libakuma` syscall wrappers were passing `s.as_ptr()` directly to the kernel. While the kernel's `copy_from_user_str` looks for `\0`, it would either read past the end of the string into random memory or fail its length check.

**Resolution**: Updated all path-related wrappers in `libakuma` (`open`, `chdir`, `mkdir`, `unlink`, `rename`, `access`) to create a temporary null-terminated string:
```rust
let path_c = alloc::format!("{}\0", path);
syscall(syscall::OPENAT, AT_FDCWD, path_c.as_ptr() as u64, ...);
```

## 5. VFORK/EXECVE Bridge PID Leak
**Symptom**: `elftest` using the "Linux Spawn" method would time out waiting for the child process.

**Cause**: In a real Linux `vfork`, the parent and child share memory until `execve`. Akuma uses a "bridge" mode where `vfork` returns a dummy PID (`0x7FFFFFFF`), and the subsequent `execve` actually spawns the process. However, `sys_execve` was returning `0` (standard success), leaving the parent with no way to know the *real* PID of the newly created child.

**Resolution**: Modified `sys_execve` to return the newly spawned PID if called from a bridge thread. Updated `elftest` to detect the bridge result and update its tracking PID accordingly.

## 6. Stack Alignment (AArch64 Requirement)
**Symptom**: Immediate crash in `_start` assembly before reaching `libakuma_init` or `main`.

**Cause**: The AArch64 architecture requires the Stack Pointer (`sp`) to be 16-byte aligned at all times when used for memory access. The `StackBuilder` was calculating the total size correctly but not ensuring the final `sp` value was rounded down to a 16-byte boundary.

**Resolution**: Added explicit alignment logic to `UserStack::setup_linux_stack`:
```rust
sp &= !0xF; // Force 16-byte alignment
```
