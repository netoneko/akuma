# Process Filesystem (procfs)

A virtual, in-memory filesystem mounted at `/proc` that exposes process stdin/stdout as files, per-process syscall logs, and SysV IPC state.

## Overview

The procfs provides file-based access to process I/O buffers, enabling:
- Reading stdout from running processes
- Writing to stdin of spawned child processes
- Process introspection via standard filesystem operations
- Per-process syscall history for live and recently-exited processes
- SysV message queue state for debugging IPC

## Filesystem Structure

```
/proc/
├── <pid>/
│   ├── fd/
│   │   ├── 0        # stdin buffer
│   │   └── 1        # stdout buffer
│   └── syscalls     # ring-buffer log of recent syscalls (when PROC_SYSCALL_LOG_ENABLED)
├── net/
│   ├── tcp          # active TCP sockets
│   └── udp          # (placeholder)
├── sysvipc/         # SysV IPC state (when PROC_SYSVIPC_ENABLED)
│   └── msg          # message queue snapshot
└── boxes            # container list (host box only)
```

Recently-exited PIDs are visible in `ls /proc` for up to `PROC_SYSCALL_LOG_RETAIN_MS` (10 s by default), so their `syscalls` log can still be read after the process dies.

## Permission Model

| File | Read | Write |
|------|------|-------|
| `/proc/<pid>/fd/0` (stdin) | Anyone | Spawner process or kernel only |
| `/proc/<pid>/fd/1` (stdout) | Anyone | Owning process only |
| `/proc/<pid>/syscalls` | Anyone (box-isolated) | Read-only |
| `/proc/sysvipc/msg` | Anyone (box-isolated) | Read-only |
| `/proc/net/tcp` | Anyone | Read-only |
| `/proc/boxes` | Host box only | Read-only |

### Write Permission Details

- **stdin (fd/0)**: Only the process that spawned the target can write. If the kernel spawned it, any caller is allowed.
- **stdout (fd/1)**: Only the owning process itself can write (via syscall writes to fd 1).

### Box Isolation

Box N (a container) only sees processes and queues that belong to box N. Box 0 (host) sees everything. This applies to:
- `/proc/<pid>/` directory listings and all files within
- `/proc/sysvipc/msg` (filters to box-owned queues only)

## Per-Process Syscall Log (`/proc/<pid>/syscalls`)

When `PROC_SYSCALL_LOG_ENABLED = true`, each process has a ring-buffer of its N most recent syscalls (default N = 500). The file is readable after the process exits for up to `PROC_SYSCALL_LOG_RETAIN_MS` milliseconds (default 10 s).

### Format

```
# pid=48
# TIMESTAMP_US       NR  DUR_US  RESULT
  1742123456789001    1     123       6
  1742123456789124    3      45    4096
```

- **TIMESTAMP_US** — `uptime_us()` value at syscall entry
- **NR** — Linux AArch64 syscall number
- **DUR_US** — time spent inside the syscall handler (µs)
- **RESULT** — raw return value (unsigned; errors are large 64-bit values matching `-errno`)

Entries are oldest-first. The log is written to under IRQs-disabled to keep overhead minimal.

### Configuration

| Constant | Default | Effect |
|----------|---------|--------|
| `PROC_SYSCALL_LOG_ENABLED` | `true` | Enable/disable the entire feature (zero overhead when false) |
| `PROC_SYSCALL_LOG_MAX_ENTRIES` | `500` | Ring-buffer size per process |
| `PROC_SYSCALL_LOG_RETAIN_MS` | `10000` | How long (ms) a dead process's log is kept |

### Usage

```bash
# Watch what syscalls a stuck go build process is making
cat /proc/49/syscalls

# Check a process that just died
cat /proc/73/syscalls   # works up to 10 s after exit
```

---

## SysV Message Queue Snapshot (`/proc/sysvipc/msg`)

When `PROC_SYSVIPC_ENABLED = true`, `/proc/sysvipc/msg` mirrors the Linux `/proc/sysvipc/msg` format — one row per live message queue.

### Format

```
       key      msqid perms      cbytes       qnum lspid lrpid   stime   rtime   ctime
         0          1   644           0          0     0     0       0       0       0
```

Fields `lspid`, `lrpid`, and timestamps are always 0 (not tracked). `cbytes` is the current byte count of all queued messages; `qnum` is the message count.

### Configuration

| Constant | Default | Effect |
|----------|---------|--------|
| `PROC_SYSVIPC_ENABLED` | `true` | Enable/disable the sysvipc directory |

---

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
# List all processes (includes recently-exited PIDs with retained logs)
ls /proc

# View process stdout
cat /proc/123/fd/1

# Write to process stdin (if spawned by current shell)
write /proc/123/fd/0 "input data"

# Inspect recent syscalls for a running process
cat /proc/49/syscalls

# Check message queues (e.g. during go build debugging)
cat /proc/sysvipc/msg

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
| `src/syscall/log.rs` | Syscall ring-buffer log (`record`, `mark_exited`, `get_formatted`) |
| `src/syscall/msgqueue.rs` | `list_msg_queues()`, `MsgQueueSnapshot` |
| `src/config.rs` | All `PROC_*` constants |
| `src/fs.rs` | Mounts procfs at `/proc` |

## Limitations

- **Read-only structure**: Cannot create/delete files or directories
- **No stderr**: Only fd 0 (stdin) and fd 1 (stdout) are exposed
- **No append semantics**: Writes always use "last write wins" policy
- **Dynamic content**: Directory listings reflect current process list at query time
- **Syscall log**: `lspid`/`lrpid`/timestamps in `/proc/sysvipc/msg` are always 0 (not tracked)

## Mount Command

The `mount` shell command displays all mounted filesystems:

```
$ mount
ext2 on / type ext2
proc on /proc type proc
```

Mount points also appear in `ls /` output alongside regular directories.
