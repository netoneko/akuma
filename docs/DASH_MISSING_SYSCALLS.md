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

## 6. Job Control and Process Groups

Advanced shells like `dash` require functional Job Control to manage foreground and background processes. This necessitated the implementation of several standard Linux AArch64 syscalls and a significant architectural change to how terminal state is managed.

### 6.1. New Job Control Syscalls

The following syscalls were implemented to satisfy `dash`'s initialization and process management logic:

*   **Syscall 155: `getpgid`** - Returns the process group ID. For system threads (like the built-in SSH shell), it falls back to the thread ID.
*   **Syscall 154: `setpgid`** - Sets the process group ID of a target process.
*   **Syscall 157: `setsid`** - Creates a new session and sets the calling process as the session leader and process group leader.
*   **Syscall 129: `kill`** - Standard Linux `kill(pid, sig)` for sending signals to processes.

### 6.2. Shared Terminal State Architecture

Previously, terminal state (`TerminalState`) was isolated within each `Process` struct. This prevented the shell and its children from agreeing on which process group was in the foreground.

*   **The Fix:** Refactored `Process` to use `Arc<Spinlock<terminal::TerminalState>>`. 
*   **Global Registry:** Implemented a `TERMINAL_STATES` registry in `src/process.rs` mapping thread IDs to these shared states.
*   **Inheritance:** Modified `spawn_process_with_channel_ext` to automatically inherit the terminal state from the calling thread.
*   **IOCTL Synchronization:** Updated `TIOCGPGRP` (Get) and `TIOCSPGRP` (Set) to read from and write to the shared `foreground_pgid` field in the `TerminalState`.

### 6.3. Interactive SSH Shell Integration

To support `dash` over SSH, the built-in shell now performs the following:

1.  **Shared State Initialization:** `run_shell_session` creates and registers a shared `TerminalState` for the entire session.
2.  **Foreground Delegation:** Upon spawning an external shell (configured via `session.config.shell`), the session explicitly sets the new process's PID as the `foreground_pgid`.
3.  **IO Bridging:** Implemented `bridge_process` in the kernel to forward data between the encrypted SSH stream and the process's standard I/O streams.

These changes allow `dash` to correctly identify its foreground status, enabling echoing and full interactivity without infinite initialization loops.

## 7. Process Output and TTY Interactivity

Several architectural bottlenecks were resolved to ensure that character input reached the shell correctly and that command output (like `ls`) was bridged back to the SSH session without loss.

### 7.1. Proper `execve` and `fork` Semantics

The previous "spawn-as-exec" model caused grandchildren processes to lose their connection to the SSH bridge.

*   **In-Place `execve`**: Implemented `Process::replace_image` to replace a process's memory in-place. This preserves the process ID, file descriptors, and critically, the shared `ProcessChannel` reference used by the SSH bridge.
*   **Deep-Copy `fork`**: Implemented `fork_process` to create a true copy of the parent process (including stack and heap) and its metadata. The child inherits the same `ProcessChannel`, ensuring its output automatically flows to the bridge reading from the parent's channel.

### 7.2. TTY Line Discipline

To support interactive shell behavior, basic TTY input/output processing was added to the kernel:

*   **ICRNL Mapping**: Implemented `ICRNL` (map carriage return to newline) in `sys_read`. This allows shells to recognize the `Enter` key (sent as `\r` by many clients) as a command terminator (`\n`).
*   **Kernel-Level ECHO**: Implemented `ECHO` logic in `sys_read`. Characters typed by the user are now echoed back to the terminal (stdout) immediately by the kernel, providing essential visual feedback.
*   **Default Flags**: Updated `TerminalState` to enable `ICANON | ECHO | ICRNL | ONLCR` by default, matching a standard Linux interactive environment.

### 7.3. Robust Output Draining

The SSH bridge loop was refined to prevent race conditions where fast-running processes (like `ls`) might exit before their output was fully sent.

*   **Aggressive Draining**: The `bridge_process` loop now uses an internal `while` loop to drain all available data from the process channel in every iteration, ensuring no "last bytes" are left behind when a process terminates.

---

## 8. SPSR and Instruction Aborts in Forked Processes

The `elftest` utility revealed a critical issue where forked child processes would immediately crash with an `Instruction abort from EL0 at FAR=0x0`. This occurred despite the parent's execution context (PC, SP) being correctly captured.

### 8.1. Problem: Incorrect SPSR in Child Process

The root cause was traced to the `UserContext` used for the child process. During the `clone` syscall, the parent's saved `SPSR_EL1` (Program Status Register) was being directly copied to the child's `UserContext`.

When `elftest` (the parent process) invoked `clone`, its `SPSR_EL1` often had the `DAIF` bits set (e.g., `0x80000000`). While these bits signify interrupts being masked, the more crucial aspect for process execution is the exception level mode. The mode bits in this SPSR value correctly indicated `EL0t` (thread state at EL0). However, the child process, when initially started by `enter_user_mode`, inherited this SPSR.

The expectation for a newly forked child process is to always begin execution with interrupts enabled and in `EL0t` mode, which corresponds to an `SPSR` value of `0x0`. The inherited `SPSR` with masked interrupts (0x80000000) was not the primary issue, as EL0t was correct.

### 8.2. Problem: SPSR of Parent vs. Child Expectations

The core problem was subtly different from an EL mismatch. The child thread, even though its `PC` was correctly set to resume execution from the `clone` syscall, would ERET with an SPSR that included the parent's interrupt mask. If the kernel's internal state (such as the scheduler or device drivers) expected interrupts to be enabled upon returning to user space, this mismatch could lead to unexpected behavior or an invalid CPU state that ultimately caused the `FAR=0x0` abort. It's akin to the CPU getting into a state where it cannot properly fetch or interpret the next instruction due to an unexpected privilege or interrupt configuration for the target EL0.

### 8.3. The Fix: Enforcing SPSR=0 for Children

To resolve this, the `spsr` field in the child's `UserContext` is now explicitly set to `0x0` (representing EL0t with all interrupts enabled) within `get_saved_user_context` in `src/threading.rs`. This guarantees that all forked child processes begin their execution in a clean, predictable state, irrespective of the parent's current `SPSR` during the `clone` syscall.

This ensures that the CPU returns to a proper EL0 context, preventing the `Instruction abort from EL0 at FAR=0x0` error and allowing forked processes to execute their initial instructions successfully.