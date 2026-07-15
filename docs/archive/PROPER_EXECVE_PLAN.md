# Plan: Proper `execve` Implementation

The current "spawn-as-exec" implementation breaks standard Unix process semantics, causing lost output (grandchild problem) and broken job control (PID mismatches). This plan outlines the transition to a true `execve` that replaces the process image in-place.

## 1. The Core Problem

Currently, `sys_execve`:
1.  Creates a **new** process (new PID, new memory, new channel).
2.  Returns the new PID (or 0).
3.  The caller (parent) continues running.

**Standard `execve` behavior:**
1.  **Preserves PID:** The process ID stays the same.
2.  **Preserves FDs:** Open file descriptors (stdin/out/err) remain open (unless `O_CLOEXEC`).
3.  **Replaces Memory:** The old code/data/stack are unmapped, and the new ELF is loaded.
4.  **Never Returns:** On success, execution jumps to the new ELF's entry point.

## 2. Implementation Strategy

### Phase 1: `Process::replace_image` (Kernel Core)

We need a method on `Process` that performs the "brain transplant":

```rust
impl Process {
    pub fn replace_image(&mut self, elf_data: &[u8], args: &[String], env: &[String]) -> Result<usize, ElfError> {
        // 1. Deactivate current address space (switch to kernel TTBR0)
        // 2. Drop/Free existing UserAddressSpace (frees all old pages)
        // 3. Load new ELF (allocates new pages, creates new AddressSpace)
        // 4. Update process fields (brk, entry_point, etc.)
        // 5. Re-initialize stack with new args/env
        // 6. Return new entry point
    }
}
```

### Phase 2: `sys_execve` Update (Syscall Handler)

Refactor `sys_execve` in `src/syscall.rs`:

1.  **Lock Current Process:** Get exclusive access.
2.  **Read ELF:** Read the binary from the filesystem.
3.  **Call `replace_image`:** Perform the swap.
4.  **Context Switch:**
    *   Update the `UserContext` (SP, PC/ELR) for the *current* thread.
    *   **Do not return 0.** Instead, directly context switch to the new entry point.

### Phase 3: `vfork` Support (`sys_clone`)

Since Akuma doesn't support full COW `fork`, we rely on `vfork` semantics (parent blocks until child calls `execve` or `exit`).

1.  **`sys_clone`:**
    *   Must actually create a new "skeleton" process or thread that shares the parent's resources temporarily.
    *   For now, we can implement a "copy-on-call" approach: `sys_clone` creates a full copy of the process struct (duplicating FDs, sharing the channel) but with a new PID.
    *   The child (new PID) immediately calls `sys_execve`.

### Phase 4: Output Inheritance

With true `execve`:
1.  `dash` (PID 15) calls `fork` -> creates Copy (PID 16).
2.  PID 16 shares `ProcessChannel` (or inherits FDs pointing to it) with PID 15.
3.  PID 16 calls `execve("ls")`.
4.  PID 16's memory is replaced by `ls`, but **PID remains 16** and **Channel/FDs are preserved**.
5.  `ls` writes to FD 1.
6.  Since FD 1 points to the *shared* `ProcessChannel`, the **SSH Bridge (watching PID 15's channel)** *might* need updating, OR:
    *   **Better:** The SSH Bridge should watch the *session*.
    *   **Simpler Fix:** If `vfork` duplicates the `ProcessChannel` reference (Arc), then `ls` writes to the *same* channel object. The bridge reads from that channel object. Output works!

## 3. Immediate Steps

1.  **Implement `Process::replace_image`**: This is the heavy lifting.
2.  **Fix `sys_clone`**: Stop returning fake PIDs. Actually duplicate the process metadata.
3.  **Fix `sys_execve`**: Use `replace_image` instead of `spawn`.

## 4. Why SSH Bridge isn't watching `dash` (Answer)

The SSH bridge *is* watching `dash` (the session leader). The problem is that currently, `execve` creates a **disconnected** process with a separate output channel. With the `replace_image` fix, the child process will inherit the *same* output channel reference, so data written by the child will appear in the stream the bridge is already reading.
