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

## 9. Fork Context and Trap Frame Bugs (`elftest` Crashes)

After the initial `execve`/`fork` implementation (sections 7–8), the `elftest` utility's "Linux Spawn" path (vfork+execve via `clone(0x4111)`) continued to crash with `Instruction abort from EL0 at FAR=0x0, ISS=0x7`. Three distinct bugs were identified and fixed.

### 9.1. Bug: Child Process Context Was All Zeros

**Symptom:** The forked child immediately faulted at address `0x0` despite the parent having valid code at its expected PC.

**Root Cause:** `fork_process` created the child `Process` with `context: UserContext::default()` (all fields zero). The child thread's trampoline (`entry_point_trampoline`) calls `proc.run()`, which enters user mode via `enter_user_mode(&self.context)`. Since `self.context.pc` was `0`, the CPU jumped to virtual address `0x0`.

Meanwhile, `update_thread_context(tid, &child_ctx)` updated the *thread pool's* Context struct (used by the scheduler), but the trampoline never reads those fields—it only uses the Process struct's `context`.

**Fix (`src/process.rs`):** Added `new_proc.context = child_ctx` in `fork_process` before calling `register_process`. This ensures the trampoline enters user mode at the correct PC with the correct SP, x0=0, and all other parent registers preserved.

### 9.2. Bug: `get_saved_user_context` Returned Stale PC/SP

**Symptom:** Even with the context stored in the Process struct, the child's PC would be wrong because `get_saved_user_context` returned `ctx.user_entry` which was `0`.

**Root Cause:** The `user_entry` and `user_sp` fields in the thread's `Context` struct were only set during thread creation (to `0` for kernel-spawned closure threads) and by `update_thread_context`. They were *never updated* when a user process trapped to EL1 for a syscall. The actual user PC (ELR_EL1) and SP (SP_EL0) were saved by the assembly exception handler into a `UserTrapFrame` on the kernel stack, but `get_saved_user_context` didn't read from it.

**Fix (`src/threading.rs`, `src/exceptions.rs`):**

1.  Added a per-thread `CURRENT_TRAP_FRAME` array (`[AtomicU64; MAX_THREADS]`) to store the live trap frame pointer.
2.  In `rust_sync_el0_handler`, the trap frame pointer is saved via `set_current_trap_frame(frame)` at the start of every SVC handler and cleared via `clear_current_trap_frame()` before returning.
3.  Rewrote `get_saved_user_context` to check `CURRENT_TRAP_FRAME` first. When available (i.e., called from within a syscall on the same thread), it reads **all 31 general-purpose registers** plus PC, SP, SPSR, and TPIDR directly from the live `UserTrapFrame`. This gives fork a complete snapshot of the parent's register state, not just PC/SP.

### 9.3. Bug: Missing ProcessInfo Write for Forked Children

**Symptom:** `read_current_pid()` could return `0` or `None` for the forked child, causing `current_process()` lookups to fail.

**Root Cause:** `fork_process` allocated and mapped a process info page for the child but never wrote the `ProcessInfo` struct (pid, parent_pid, box_id) to it. The page was zeroed from `alloc_page_zeroed`, so the pid field read as `0`.

**Fix (`src/process.rs`):** Added a `ProcessInfo::new()` write to the child's process info page in `fork_process`, immediately after mapping it, before spawning the child thread.

### 9.4. Bug: `enter_user_mode` Zeroed All Registers

**Symptom:** Even with the correct PC and full register capture from the trap frame, the forked child still crashed at `FAR=0x0`. The child's code expected callee-saved registers (x19-x28) and the link register (x30) to hold the parent's values, but they were all zero.

**Root Cause:** `enter_user_mode` unconditionally cleared all 31 GP registers to zero (`mov x0, #0` ... `mov x30, #0`) before ERET. This was correct for fresh process launches (where registers should be clean) but catastrophic for forked children. With `x30 = 0`, any `ret` instruction in the child's code immediately jumped to address `0x0`.

**Fix (`src/process.rs`):** Rewrote `enter_user_mode` to load all GP registers from the `UserContext` struct instead of zeroing them. The context pointer is pinned to `x30` via an explicit register constraint (`in("x30")`), and x0-x29 are loaded first via LDP instructions at struct offsets. x30 is loaded last (`ldr x30, [x30, #240]`), which safely overwrites the context pointer that is no longer needed. For regular spawns, `UserContext::new()` initializes all registers to 0, so the behavior is unchanged.

### 9.5. Bug: Duplicate PID Counters

**Symptom:** Clone reported `forking PID 12 -> 22` but the child process ran as PID 20. The parent waited for PID 22 (the return value from clone) but the child had a different PID, causing `wait4` to timeout.

**Root Cause:** Two independent PID counters existed:
1.  A module-level `static NEXT_PID: AtomicU32` starting at 1, used by `Process::from_elf()`.
2.  A function-local `static NEXT_PID: AtomicU32` starting at 20, inside `allocate_pid()`, used by `sys_clone`.

These counters ran independently and eventually assigned the same PID values to different processes, causing lookup failures.

**Fix (`src/process.rs`):** Removed the local `NEXT_PID` from `allocate_pid()` and made it use the single module-level counter, ensuring all PID allocations draw from the same sequence.

### 9.6. Bug: `execve` Did Not Activate New Address Space

**Symptom:** After a successful `replace_image` in `sys_execve`, the child crashed with `Unknown from EL0: EC=0x0, ISS=0x0` at `ELR=0x400000`.

**Root Cause:** `replace_image` called `UserAddressSpace::deactivate()` (resetting TTBR0 to the boot page tables) to safely drop the old address space, then installed the new address space in the Process struct. But neither `replace_image` nor `sys_execve` activated the new address space. When `enter_user_mode` did ERET to EL0, TTBR0 still pointed at the boot page tables. Address `0x400000` in the boot tables maps to QEMU's Flash region, causing the CPU to fetch an invalid instruction encoding (`EC=0x0`).

**Fix (`src/syscall.rs`):** Added `proc.address_space.activate()` in `sys_execve` after `replace_image` and before `enter_user_mode`.

### 9.7. Bug: Exit Code Contamination via Shared ProcessChannel

**Symptom:** After running `elftest` (which exits with code 42), every subsequent command showed `[exit code: 42]` immediately—even interactive programs like `dash` and `hello`. Programs appeared to exit instantly with stale exit codes from prior runs.

**Root Cause:** `spawn_process_with_channel_ext` reused the calling process's `ProcessChannel` via `current_channel()`:

```rust
let channel = if let Some(parent_channel) = current_channel() {
    parent_channel  // shared Arc to the SSH shell's channel
} else {
    Arc::new(ProcessChannel::new())
};
```

This same `Arc<ProcessChannel>` was:
1. Set as the child's `proc.channel` (for I/O).
2. Registered in `PROCESS_CHANNELS[child_tid]` (for exit tracking).
3. Returned to the caller (SSH `interactive_bridge`).

When the child exited, `return_to_kernel` called `remove_channel(child_tid)` and then `channel.set_exited(42)`. Since this was the *same Arc* as the SSH shell's channel, the shell's channel was permanently marked as exited. Every subsequent spawn inherited this contaminated channel, and the `interactive_bridge` immediately saw `has_exited()=true`.

**Fix (`src/process.rs`):** Changed `spawn_process_with_channel_ext` to always create a fresh `ProcessChannel` per spawn instead of reusing the parent's. Each process now has its own exit-tracking channel that cannot contaminate the parent.

### 9.8. Bug: Forked Child Output Invisible

**Symptom:** After fork+execve, the child process (e.g., `/bin/cat`) wrote to stdout successfully (`[ProcessChannel] Write 84 bytes`), but the output never appeared on the SSH terminal.

**Root Cause:** In `fork_process`, the exit-tracking channel was also assigned as the child's I/O channel:

```rust
let exit_channel = Arc::new(ProcessChannel::new());
new_proc.channel = Some(exit_channel.clone());  // overwrites parent's channel
```

This replaced the parent's channel (which the SSH `interactive_bridge` was polling) with a fresh channel that nobody read from. The child's stdout writes went into this orphaned buffer.

**Fix (`src/process.rs`):** Removed the `new_proc.channel = Some(exit_channel.clone())` line. The child keeps `channel: parent.channel.clone()` from the struct initializer, so its stdout writes go to the same channel the `interactive_bridge` reads. The exit-tracking channel is only registered in `PROCESS_CHANNELS` (for `return_to_kernel` to call `set_exited()`) and `CHILD_CHANNELS` (for the parent's `wait4` to check exit status).

### 9.9. Bug: `wait4(pid=-1)` Not Implemented

**Symptom:** After fork+execve via dash, the child ran and exited successfully, but dash never resumed. No prompt appeared and no further syscalls were logged from the parent.

**Root Cause:** `sys_wait4` had a TODO for the `pid=-1` (wait-for-any-child) case:

```rust
let target_pid = if pid == 0x7FFFFFFF || pid == -1 {
    None // TODO: implement wait for ANY
} else if pid > 0 {
    Some(pid as u32)
} else {
    None
};
```

With `target_pid=None`, the function fell through immediately and returned `0`. Dash called `waitpid(-1, WUNTRACED)` (a blocking wait) and received `0`, which it interpreted as an error or no-op. Dash then entered a state where it could not reap its child or return to its prompt.

Additionally, `CHILD_CHANNELS` only stored `BTreeMap<Pid, Arc<ProcessChannel>>` with no record of which parent owned each child, making it impossible to answer "find any exited child of process P."

**Fix (`src/process.rs`, `src/syscall.rs`, `src/socket.rs`):**

1. Extended `CHILD_CHANNELS` from `BTreeMap<Pid, Arc<ProcessChannel>>` to `BTreeMap<Pid, (Arc<ProcessChannel>, Pid)>` where the second `Pid` is the parent. Updated `register_child_channel` to accept a `parent_pid` parameter.
2. Added `find_exited_child(parent_pid)` — scans `CHILD_CHANNELS` for any entry whose parent matches and whose channel reports `has_exited()`.
3. Added `has_children(parent_pid)` — checks if any children are registered for the given parent.
4. Rewrote `sys_wait4` for `pid=-1`: if no children exist, return `-ECHILD`. Otherwise, loop calling `find_exited_child()` with `yield_now()` between iterations (or return `0` immediately if `WNOHANG` is set). On finding an exited child, write the status, call `remove_child_channel`, and return the child PID.
5. Added `ECHILD=10` to `libc_errno` constants.

### 9.10. Summary of Changes

| File | Change |
|------|--------|
| `src/threading.rs` | Added `CURRENT_TRAP_FRAME` per-thread storage, `set_current_trap_frame`, `clear_current_trap_frame`. Rewrote `get_saved_user_context` to read from live trap frame with full register capture. Both paths enforce `spsr = 0`. |
| `src/exceptions.rs` | In `rust_sync_el0_handler`, save trap frame pointer at SVC entry, clear it before return. |
| `src/process.rs` | Rewrote `enter_user_mode` to load GP registers from `UserContext` instead of zeroing them. In `fork_process`: write `ProcessInfo` to child's info page; set `new_proc.context = child_ctx`; enforce `child_ctx.spsr = 0`; create exit-tracking channel separate from I/O channel. Unified `allocate_pid()` to use the single module-level PID counter. Always create fresh `ProcessChannel` per spawn. Extended `CHILD_CHANNELS` to track parent PID; added `find_exited_child()` and `has_children()`. |
| `src/syscall.rs` | In `sys_execve`: activate new address space after `replace_image` before entering user mode. Implemented `wait4(pid=-1)` with blocking scan over `CHILD_CHANNELS`. Updated `sys_spawn`/`sys_spawn_ext` to pass parent PID to `register_child_channel`. |
| `src/socket.rs` | Added `ECHILD=10` to `libc_errno`. |

## 10. Pipe and File Descriptor Duplication Support

Dash's pipeline support (`echo hello | sha256sum`) exposed three missing kernel features: `dup3`, real kernel pipes, and correct `stat` inode reporting. Several related `unlinkat` bugs were also fixed.

### 10.1. `dup3` Syscall (Syscall 24)

**Symptom:** Dash logged `Unknown syscall: 24` when setting up pipes. Pipe fd redirections silently failed, so `echo` wrote to the terminal instead of the pipe, and `sha256sum` read from the terminal (blocking forever).

**Implementation:** `dup3(oldfd, newfd, flags)` clones the file descriptor entry from `oldfd` to `newfd`:

- If `oldfd == newfd`, returns `EINVAL`
- If `newfd` already points to a pipe fd, properly closes it (decrementing pipe reference counts)
- If the cloned entry is a pipe fd, increments the pipe's reference count
- Added `Process::set_fd(fd, entry)` for inserting at a specific fd number
- Added `EBADF` error constant

### 10.2. Kernel Pipes (Replacing `pipe2` File Stub)

**Symptom:** Even with `dup3`, `echo hello | sha256sum` produced the SHA-256 of an empty string. The old `pipe2` implementation used two separate temp files (`/tmp/pipe_r`, `/tmp/pipe_w`) with no shared buffer — data written to one never appeared in the other.

**Implementation:** Real in-kernel pipe infrastructure:

- `KernelPipe` struct: shared `Vec<u8>` buffer, `write_count`/`read_count` reference counts, optional `reader_thread` for wake-on-data
- Global `PIPES` table (`BTreeMap<u32, KernelPipe>`) with atomic `NEXT_PIPE_ID` counter
- New `FileDescriptor::PipeRead(pipe_id)` and `FileDescriptor::PipeWrite(pipe_id)` variants
- `sys_read` for `PipeRead`: blocking loop — returns data when available, EOF (0) when all write ends are closed
- `sys_write` for `PipeWrite`: appends to shared buffer, wakes blocked reader thread
- `sys_close`: `PipeWrite` decrements `write_count` (delivers EOF to reader only when it reaches 0); `PipeRead` decrements `read_count`; pipe is removed from table when both counts reach 0

### 10.3. Pipe Reference Counting Across Fork and Dup

**Symptom:** After the initial pipe implementation, `sha256sum` still hashed empty input. When dash forks two children for a pipeline, all three processes (parent + 2 children) get copies of both `PipeRead` and `PipeWrite` fds. Without reference counting, the first `close()` of any `PipeWrite` copy (e.g., sha256sum closing its inherited write end) prematurely signaled EOF to the reader.

**Implementation:**

- `pipe_create()` initializes `write_count=1, read_count=1`
- `pipe_clone_ref(id, is_write)` increments the appropriate count — called from:
  - `fork_process()` when cloning the parent's fd table (iterates all pipe fds)
  - `sys_dup3()` when duplicating a pipe fd to a new fd number
- `pipe_close_write()` uses `saturating_sub(1)` and only signals EOF when `write_count` reaches 0
- `pipe_close_read()` uses `saturating_sub(1)` and only removes the pipe when both counts are 0
- `cleanup_process_fds()` handles pipe fd cleanup on process exit

### 10.4. `stat` Inode Bug (`st_ino=0` for All Files)

**Symptom:** `rm -rf /usr/bin` printed `"/" may not be removed` — sbase `rm` uses `stat()` to compare the target's `(st_dev, st_ino)` with the root directory's. Since both returned `(0, 0)`, every path looked like `/`.

**Fix:**

- Added `inode: u64` field to `vfs::Metadata`
- ext2: uses real inode number from `lookup_path()`
- memfs/procfs: FNV-1a hash of the path
- `sys_fstat` and `sys_newfstatat` now set `st_dev=1`, `st_ino=meta.inode`, `st_nlink`, `st_blksize=4096`

### 10.5. `unlinkat` Bugs (dirfd and AT_REMOVEDIR)

**Symptom:** After fixing `stat`, `rm -rf` ran without the root check error but files/directories remained on disk.

**Fix:**

- Resolve relative paths using the dirfd's path (or CWD for `AT_FDCWD`), matching `sys_newfstatat` resolution logic
- Check `flags & AT_REMOVEDIR` (0x200) and call `remove_dir()` for directories, `remove_file()` for regular files

### 10.6. Summary of Changes

| File | Change |
|------|--------|
| `src/syscall.rs` | Implemented `sys_dup3` (syscall 24) with pipe ref count handling. Added `KernelPipe` infrastructure with reference-counted read/write ends. Rewrote `sys_pipe2` to use kernel pipes. Added `PipeRead`/`PipeWrite` handling in `sys_read`, `sys_write`, `sys_close`. Fixed `sys_unlinkat` dirfd resolution and `AT_REMOVEDIR` flag. Fixed `sys_fstat`/`sys_newfstatat` to populate `st_dev`, `st_ino`, `st_nlink`, `st_blksize`. Added `EBADF` constant. |
| `src/process.rs` | Added `PipeRead(u32)` and `PipeWrite(u32)` to `FileDescriptor`. Added `set_fd()` method. Pipe ref count increment in `fork_process` fd table clone. Pipe cleanup in `cleanup_process_fds`. |
| `src/vfs/mod.rs` | Added `inode: u64` to `Metadata` struct. |
| `src/vfs/ext2.rs` | Set `inode: inode_num` in `metadata()`. |
| `src/vfs/memory.rs` | Added `path_inode()` FNV-1a helper, set `inode` in `metadata()`. |
| `src/vfs/proc.rs` | Inline FNV-1a hash for `inode` in `metadata()`. |

## 11. Canonical Mode (ICANON) Line Editing

Backspace did not work in dash. Pressing backspace echoed a raw `0x7F` byte to the terminal instead of erasing the previous character. The kernel had `ICANON` enabled in `lflag` by default but never implemented canonical mode processing — characters were passed through to the process immediately without line buffering or erase handling.

### 11.1. Problem: No Canonical Mode Implementation

The `TerminalState` default flags included `ICANON | ECHO | ECHOE`, matching a standard Linux interactive terminal. However, `sys_read` treated all input identically regardless of `ICANON`:

1. Raw bytes were read from `ProcessChannel::stdin_buffer` and returned to the process immediately (one character at a time).
2. `ECHO` echoed every byte as-is, including control characters like `0x7F` (DEL/backspace).
3. No line buffering occurred — the process received each keystroke individually rather than complete lines.

Dash (and other simple shells) rely on the kernel's canonical mode for basic line editing. Without it, backspace, line-kill, and EOF (Ctrl+D) had no effect.

### 11.2. Fix: Kernel TTY Line Discipline

Added canonical mode line buffering to `sys_read` and the supporting data structures to `TerminalState`.

**Terminal state additions (`src/terminal/mod.rs`):**

- Added `cc_index` module with standard c_cc indices: `VINTR`, `VQUIT`, `VERASE`, `VKILL`, `VEOF`, `VTIME`, `VMIN`, `VEOL`.
- Added `canon_buffer: Vec<u8>` — accumulates the current line being edited.
- Added `canon_ready: VecDeque<u8>` — holds completed lines waiting to be delivered to the process.
- Set default c_cc values: `VERASE=0x7F`, `VEOF=0x04` (Ctrl+D), `VINTR=0x03` (Ctrl+C), `VKILL=0x15` (Ctrl+U), `VQUIT=0x1C`.

**Canonical mode processing in `sys_read` (`src/syscall.rs`):**

When `ICANON` is set in `lflag` and stdin is not a pipe, each incoming byte is processed through the line discipline instead of being returned immediately:

- **VERASE (0x7F) / BS (0x08):** Removes the last character from `canon_buffer`. If `ECHOE` is set, echoes `\b \b` (backspace, space, backspace) to visually erase the character on the terminal.
- **VKILL (Ctrl+U):** Clears the entire `canon_buffer` and echoes the appropriate number of `\b \b` sequences to erase the line visually.
- **Newline (`\n`):** Appends to `canon_buffer`, echoes `\r\n` (if `ONLCR`) or `\n`, then moves the complete line from `canon_buffer` to `canon_ready` for delivery.
- **VEOF (Ctrl+D):** If `canon_buffer` has data, delivers it immediately (without the Ctrl+D itself). If `canon_buffer` is empty, returns 0 (EOF).
- **Other characters:** Appended to `canon_buffer` and echoed individually if `ECHO` is set.

The process's `read()` blocks until `canon_ready` contains data (i.e., a complete line). At the top of each loop iteration, `canon_ready` is checked first so that multi-line pastes and leftover data from previous reads are delivered correctly.

On EOF (stdin closed), any partially buffered line in `canon_buffer` is flushed to `canon_ready` and delivered before returning 0.

Non-canonical mode (raw mode) retains the existing behavior — bytes are echoed and returned immediately without buffering.

### 11.3. Summary of Changes

| File | Change |
|------|--------|
| `src/terminal/mod.rs` | Added `cc_index` module with standard c_cc index constants. Added `canon_buffer` and `canon_ready` fields to `TerminalState`. Set default VERASE, VEOF, VINTR, VKILL, VQUIT values in c_cc array. |
| `src/syscall.rs` | Implemented canonical mode processing in `sys_read`: line buffering, VERASE/backspace handling with ECHOE visual erase, VKILL line-kill, VEOF delivery, newline completion. Added `canon_ready` check at loop top for previously completed lines. Flush `canon_buffer` on EOF. |

## 12. Terminal State Not Restored After Meow Exits

After `meow` exited inside a dash session, typed input was no longer visible and commands would not execute.

### 12.1. Architecture: Shared Terminal State

Children inherit their parent's `terminal_state` and `channel` as `Arc` clones (see `fork_process` in `src/process.rs`). Dash and meow therefore operate on the **same** `TerminalState` and `ProcessChannel`. Any modification meow makes to terminal flags is immediately visible to dash.

### 12.2. Bug 1: Raw Mode Not Restored (broken echo)

`meow`'s TUI calls:
```rust
get_terminal_attributes(fd::STDIN, &mut old_mode);          // saves mode_flags = 0
set_terminal_attributes(fd::STDIN, 0, RAW_MODE_ENABLE);     // clears ECHO | ICANON in lflag
// ... runs TUI ...
set_terminal_attributes(fd::STDIN, 0, old_mode);            // restores old_mode = 0
```

`sys_set_terminal_attributes` dispatched on two explicit constants:

```rust
if mode_flags_arg & RAW_MODE_ENABLE != 0  { /* clear ECHO | ICANON */ }
else if mode_flags_arg & RAW_MODE_DISABLE != 0 { /* restore */ }
// mode_flags_arg = 0  →  neither branch taken  →  ECHO stays cleared
```

Because `old_mode = 0` matched neither constant, `ECHO` and `ICANON` remained cleared after meow exited. Dash received each character but the kernel did not echo it back.

**Partial fix:** changed `else if` to `else`, so any value without `RAW_MODE_ENABLE` restores the flags.

### 12.3. Bug 2: ICRNL Not Restored (commands silently swallowed)

`RAW_MODE_ENABLE` also clears `iflag` bits including `ICRNL`:

```rust
term_state.iflag &= !(0x00000100 | 0x00000040); // clears ICRNL
```

The partial fix only restored `lflag` (`ECHO | ICANON`) and `oflag`, but left `iflag` with `ICRNL` cleared. With `ICANON=1` but `ICRNL=0`, the kernel canonical mode never converts the `'\r'` that SSH clients send on Enter into `'\n'`. The canonical buffer accumulated characters forever and `sys_read` never returned a completed line. Dash echoed characters but commands never executed.

### 12.4. Bug 3: Overwrote Dash's Own Terminal Configuration

Dash is an interactive shell that calls `tcsetattr` at startup to enable its own raw-mode line editing (`ICANON=0`, `ECHO=0`). The partial fix blindly set `ICANON=1 | ECHO=1` on restore, which conflicted with what dash had configured. Even if `ICRNL` had been restored, dash would have been confused by the unexpected return to canonical mode.

### 12.5. Fix: Save and Restore Exact Flag State

When `RAW_MODE_ENABLE` is applied, snapshot the current `iflag`, `oflag`, and `lflag` into three new `Option<u32>` fields on `TerminalState`. On restore (any call without `RAW_MODE_ENABLE`), write those exact values back. Because the terminal state is shared, this snapshot captures whatever dash had previously configured via `tcsetattr`.

```
RAW_MODE_ENABLE  →  saved_{i,o,l}flag = Some(current)  →  apply raw flags
restore (mode=0) →  {i,o,l}flag = saved_{i,o,l}flag.take()  →  dash state is fully recovered
```

If no snapshot exists (raw mode was never entered via this syscall), the fallback restores `OPOST | ONLCR` and `ECHO | ICANON`.

### 12.6. Summary of Changes

| File | Change |
|------|--------|
| `src/terminal/mod.rs` | Added `saved_iflag`, `saved_oflag`, `saved_lflag: Option<u32>` fields to `TerminalState`; initialized to `None` in `Default`. |
| `src/syscall.rs` | `sys_set_terminal_attributes`: on `RAW_MODE_ENABLE` snapshot all three flag sets before clearing them; on restore take and apply the snapshots, falling back to a sane default if no snapshot exists. |