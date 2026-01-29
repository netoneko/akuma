# Process Filesystem (procfs)

A virtual, in-memory filesystem mounted at `/proc` that exposes process stdin/stdout as files.

## Overview

The procfs provides file-based access to process I/O buffers, enabling:
- Reading stdout from running processes
- Writing to stdin of spawned child processes
- Process introspection via standard filesystem operations

## Filesystem Structure

```
/proc/
├── <pid>/
│   └── fd/
│       ├── 0    # stdin buffer
│       └── 1    # stdout buffer
├── <pid>/
│   └── fd/
│       ├── 0
│       └── 1
...
```

## Permission Model

| File | Read | Write |
|------|------|-------|
| `/proc/<pid>/fd/0` (stdin) | Anyone | Spawner process or kernel only |
| `/proc/<pid>/fd/1` (stdout) | Anyone | Owning process only |

### Write Permission Details

- **stdin (fd/0)**: Only the process that spawned the target can write. If the kernel spawned it, any caller is allowed.
- **stdout (fd/1)**: Only the owning process itself can write (via syscall writes to fd 1).

## Size Limits (OOM Prevention)

Buffers are capped to prevent memory exhaustion:

| Buffer | Limit | Config Constant |
|--------|-------|-----------------|
| stdin | 8 KB | `PROC_STDIN_MAX_SIZE` |
| stdout | 8 KB | `PROC_STDOUT_MAX_SIZE` |

**Policy: Last Write Wins**

When a write would exceed the limit:
1. The entire buffer is cleared
2. Only the new data is stored (even if larger than limit)

This prevents indefinite growth while preserving the most recent data.

## Thread Safety

Both stdin and stdout buffers are protected by `Spinlock<StdioBuffer>`:

```rust
pub struct StdioBuffer {
    pub data: Vec<u8>,
    pub pos: usize,  // Read position for stdin
}
```

All procfs operations lock the appropriate buffer before reading/writing, preventing races between:
- User-space syscall writes
- Procfs reads/writes from other processes
- Kernel operations

## Usage Examples

### Shell Commands

```bash
# List all processes
ls /proc

# View process stdout
cat /proc/123/fd/1

# Write to process stdin (if spawned by current shell)
write /proc/123/fd/0 "input data"

# View mounted filesystems
mount
```

### From Code

```rust
// Read stdout of process 42
let output = fs::read_file("/proc/42/fd/1")?;

// Write to stdin of child process
fs::write_file("/proc/42/fd/0", b"input")?;
```

## Implementation Files

| File | Purpose |
|------|---------|
| `src/vfs/proc.rs` | `ProcFilesystem` implementation |
| `src/vfs/mod.rs` | VFS mount table, `list_mounts()` |
| `src/process.rs` | `StdioBuffer`, `Spinlock` wrappers |
| `src/config.rs` | `PROC_STDIN_MAX_SIZE`, `PROC_STDOUT_MAX_SIZE` |
| `src/fs.rs` | Mounts procfs at `/proc` |

## Limitations

- **Read-only structure**: Cannot create/delete files or directories
- **No stderr**: Only fd 0 (stdin) and fd 1 (stdout) are exposed
- **No append semantics**: Writes always use "last write wins" policy
- **Dynamic content**: Directory listings reflect current process list at query time

## Mount Command

The `mount` shell command displays all mounted filesystems:

```
$ mount
ext2 on / type ext2
proc on /proc type proc
```

Mount points also appear in `ls /` output alongside regular directories.
