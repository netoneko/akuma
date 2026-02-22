# Plan for Implementing Missing Syscalls for Dash

This document outlines the strategy for integrating the missing system calls required by the `dash` userspace application into the Akuma operating system. The identified missing syscalls are `getpid`, `getppid`, and `geteuid`.

## 1. Identify Missing Syscalls

Based on the `userspace/dash/README.md` and subsequent analysis, the following AArch64 Linux system calls are currently unsupported by the Akuma kernel:

*   **Syscall 172: `getpid`** - Returns the process ID of the calling process.
*   **Syscall 173: `getppid`** - Returns the process ID of the parent of the calling process.
*   **Syscall 175: `geteuid`** - Returns the effective user ID of the calling process.

## 2. Kernel Implementation (`src/syscall.rs`)

For each missing syscall, the following steps will be taken within the kernel:

### 2.1. Update `Syscall` Enum

Add new variants to the `Syscall` enum in `src/syscall.rs` to represent each of the new system calls. The variants should map to their respective AArch64 syscall numbers.

```rust
// In src/syscall.rs
pub enum Syscall {
    // ... existing syscalls ...
    GetPid = 172,
    GetPPid = 173,
    GetEUid = 175,
    // ...
}
```

### 2.2. Implement Syscall Handlers

For each new `Syscall` variant, a corresponding handler function will be implemented in `src/syscall.rs` (or a dedicated module if the syscall logic is complex).

#### `sys_getpid()`

This function will retrieve the Process ID (PID) of the currently executing process.

*   **Logic:** Access the current thread's process control block (PCB) and return its unique PID.
*   **Return:** `usize` representing the PID.

#### `sys_getppid()`

This function will retrieve the Parent Process ID (PPID) of the currently executing process.

*   **Logic:** Access the current thread's PCB, then retrieve the PID of its parent process.
*   **Return:** `usize` representing the PPID.

#### `sys_geteuid()`

This function will retrieve the Effective User ID (EUID) of the currently executing process.

*   **Logic:** For simplicity, initially, Akuma might not have a concept of distinct user IDs. This syscall can return a default EUID (e.g., 0 for root) until a more robust user management system is implemented.
*   **Return:** `usize` representing the EUID.

### 2.3. Integrate with `syscall_handler()`

Modify the `syscall_handler()` function in `src/syscall.rs` to dispatch to the newly implemented handler functions based on the `Syscall` enum variant.

```rust
// In src/syscall.rs
pub fn syscall_handler(call: Syscall, args: [usize; 6]) -> SyscallResult {
    match call {
        // ... existing match arms ...
        Syscall::GetPid => sys_getpid(),
        Syscall::GetPPid => sys_getppid(),
        Syscall::GetEUid => sys_geteuid(),
        // ...
        _ => { /* Handle unknown syscalls or return an error */ }
    }
}
```

## 3. Userspace Library Integration (`userspace/libakuma`)

The `libakuma` library provides the userspace interface to kernel syscalls.

### 3.1. Add Wrapper Functions

For each new syscall, a corresponding wrapper function will be added to `userspace/libakuma/src/lib.rs`. These functions will abstract the raw syscall interface into idiomatic Rust functions.

```rust
// In userspace/libakuma/src/lib.rs
#[inline(always)]
pub fn getpid() -> usize {
    crate::syscall!(GETPID) // Assuming GETPID maps to 172
}

#[inline(always)]
pub fn getppid() -> usize {
    crate::syscall!(GETPPID) // Assuming GETPPID maps to 173
}

#[inline(always)]
pub fn geteuid() -> usize {
    crate::syscall!(GETEUID) // Assuming GETEUID maps to 175
}
```
*(Note: The actual `syscall!` macro usage might vary based on its implementation in `libakuma`.)*

### 3.2. Update Syscall Definitions (if applicable)

If `libakuma` maintains its own internal mapping of syscall numbers to names, these mappings will need to be updated to include `GETPID`, `GETPPID`, and `GETEUID`.

## 4. Testing

After implementation, tests will be added to verify the correct functionality of the new syscalls.

*   **Kernel Tests:** Add unit or integration tests within `src/*_tests.rs` to directly call the kernel-side handler functions and assert their behavior.
*   **Userspace Tests:** Add tests within `userspace/dash` or `userspace/libakuma` to call the new `libakuma` wrapper functions and verify the returned values.

## 5. Validation

Run the `dash` application in the Akuma OS environment and verify that the "Unknown syscall" errors for 172, 173, and 175 no longer appear, and that `dash` functions as expected.
