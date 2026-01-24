# Kill Command

The `kill` command terminates a process by PID and automatically cleans up all associated resources including file descriptors, sockets, and memory.

## Usage

```
kill <pid>
```

## Example

```
akuma:/> httpd &
httpd: Starting HTTP server on port 8080
httpd: Listening for connections...

akuma:/> ps
  PID  PPID  STATE     NAME
    1     0  blocked   httpd

akuma:/> kill 1
Killed process 1
[exit code: 137]

akuma:/> httpd
httpd: Starting HTTP server on port 8080
httpd: Listening for connections...
```

## Exit Code

Killed processes exit with code **137** (128 + SIGKILL signal 9), following Unix conventions.

## Implementation

### Process Tracking

Each process tracks its associated thread ID in the `Process` struct:

```rust
pub struct Process {
    // ...
    pub thread_id: Option<usize>,
}
```

This is set when the process is spawned on a user thread, enabling the kill mechanism to locate and terminate the correct thread.

### Kill Mechanism

The `kill_process(pid)` function in `src/process.rs` performs the following steps:

1. **Set Interrupt Flag**: Signals the process channel to interrupt any blocked syscalls (like `accept()`). This is critical for releasing port bindings.

2. **Yield to Blocked Thread**: Gives the blocked syscall a chance to detect the interrupt and abort its resources (e.g., abort TcpSocket to release port binding).

3. **Socket Cleanup**: Closes all sockets in the process's file descriptor table via `cleanup_process_sockets()`.

4. **Mark Process State**: Sets `exited = true`, `exit_code = 137`, and `state = Zombie(137)`.

5. **Unregister Process**: Removes the process from the global process table.

6. **Remove Channel**: Removes the process channel and notifies any waiting callers.

7. **Terminate Thread**: Marks the thread as terminated so the scheduler stops scheduling it.

### Handling Blocked Syscalls

When a process is killed while blocked in a syscall (e.g., `accept()` waiting for connections), the interrupt mechanism ensures proper cleanup:

1. The kill command sets the interrupt flag on the process channel
2. Blocking syscalls check `is_current_interrupted()` on each poll iteration
3. When interrupted, the syscall aborts its resources (e.g., TcpSocket) and returns `EINTR`
4. This properly releases any bound ports or other network resources

### Race Condition Handling

The `return_to_kernel()` function handles the case where a process is killed externally:

- Checks if the thread is already terminated before cleanup
- Skips redundant cleanup operations
- All cleanup operations are idempotent (safe to call multiple times)

## Files Modified

- `src/process.rs` - Added `thread_id` field, `kill_process()` function, race handling in `return_to_kernel()`
- `src/shell/commands/builtin.rs` - Added `KillCommand`
- `src/shell/commands/mod.rs` - Exported and registered the command

## Related Documentation

- [MULTITASKING.md](MULTITASKING.md) - Process and thread management
- [SYSCALL_BLOCKING.md](SYSCALL_BLOCKING.md) - How blocking syscalls work
- [USERSPACE_SOCKET_API.md](USERSPACE_SOCKET_API.md) - Socket management
