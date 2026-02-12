# Box Containerization System Proposal

## Overview

"Box" is a lightweight containerization mechanism for AkumaOS, designed to provide process and filesystem isolation ("cats love boxes"). It allows running processes in restricted environments with their own filesystem root and process view, while sharing the networking stack (for now).

The system consists of:
1.  **Kernel Primitives:** VFS path scoping, Process ID isolation, and a new syscall interface.
2.  **`box` Userspace Utility:** A daemon/client tool for managing box life-cycles and attaching to sessions (`peek`).
3.  **`herd` Integration:** Native support for spawning system services within boxes.

## 1. Kernel Architecture

### 1.1 Process Structure Updates (`src/process.rs`)

The `Process` struct will be augmented with isolation context:

```rust
pub struct Process {
    // ... existing fields ...
    
    /// Virtual Root Directory (for chroot-like behavior)
    /// Defaults to "/" for host processes.
    pub root_dir: String,

    /// Box ID (Namespace ID)
    /// None = Host System
    /// Some(id) = Inside a Box
    pub box_id: Option<u64>,
}
```

### 1.2 VFS Path Resolution (`src/vfs/mod.rs`)

The VFS layer's path resolution (`resolve` and `normalize_path`) must be aware of the current process's `root_dir`.

*   **Logic:** When a process requests a path starting with `/`:
    1.  If `process.root_dir` is `/`, treat as normal.
    2.  If `process.root_dir` is `/my/box`, the requested path `/etc/config` is rewritten to `/my/box/etc/config` before mount table resolution.
    3.  **Safety:** `..` traversal must be sanitized to ensure it never ascends above the virtual root.

### 1.3 ProcFS Virtualization (`src/vfs/proc.rs`)

`ProcFilesystem::read_dir` will be modified to filter entries based on `box_id`.

*   **Host Process (Box ID = None):** Sees all processes and the `/proc/boxes` registry.
*   **Boxed Process (Box ID = X):** Sees only processes with `box_id == X`.
*   **Isolation Guard:** The `/proc/boxes` virtual file **MUST NOT** be mounted or accessible from within a boxed environment. This prevents processes from discovering other boxes or the host-level management registry.

### 1.4 Syscall Interface (`src/syscall.rs`)

We will introduce a new syscall `sys_enter_box` (or extend `sys_spawn`). Given `box` needs to run as a daemon, extending `spawn` is cleaner.

**Option A: `sys_spawn_ext`**
New syscall `315` (SPAWN_EXT) taking a struct of options:
```rust
#[repr(C)]
struct SpawnOptions {
    cwd: *const u8,
    root_dir: *const u8, // If not null, enable boxing
    // ... future flags
}
```

## 2. Userspace `box` Utility

The `box` binary acts as both the container manager and the session host.

### 2.1 "Open" - The Daemon Mode
`box open <name> --directory <dir> <cmd>`

1.  **Init:** `box` starts.
2.  **Setup:** It prepares the target directory (e.g., mounts required FSs if needed, though mostly relies on existing structure).
3.  **Spawn:** It calls `sys_spawn_ext` with `root_dir = <dir>` to run `<cmd>`.
4.  **Session Loop:**
    *   The `box` process remains running as the "supervisor" for the container.
    *   It listens on a unix domain socket (or abstract namespace socket) named `akuma.box.<name>`.
    *   It captures the child's `stdin`/`stdout`/`stderr` (via the existing `ProcessChannel` mechanism).

### 2.2 Commands

*   **`box open <name> --directory <dir> <cmd>`**:
    *   Starts a new box daemon.
    *   Spawns `<cmd>` with `root_dir` set to `<dir>`.
*   **`box peek <name>`**:
    *   Attaches to the stdout/stdin of the box.
    *   Uses a Unix socket for IPC with the daemon.
*   **`box close <name|id>`**:
    *   Sends a termination signal to all processes in the box.
    *   The daemon performs cleanup of IPC sockets and temporary mounts.
    *   The box entry is removed from the system.
*   **`box ps`**:
    *   Lists all active boxes, their IDs, names, root directories, and primary process PIDs.
*   **`box show <name|id>`**:
    *   Displays detailed information about a box: uptime, resource usage (if available), and a list of all member PIDs.

## 3. Kernel Architecture Updates

### 3.1 Box Registry

The kernel will maintain a global registry of active boxes to facilitate `ps` and `close` operations.

```rust
pub struct BoxInfo {
    pub id: u64,
    pub name: String,
    pub root_dir: String,
    pub creator_pid: Pid,
}

static BOX_REGISTRY: Spinlock<BTreeMap<u64, BoxInfo>> = ...;
```

### 3.2 Process Management

*   **`sys_kill_box(box_id)`**: A new syscall to kill all processes sharing a specific `box_id`. This ensures that even if a process forks, the entire container can be brought down.
*   **`procfs` Extension**: `/proc/boxes` will expose the box registry to userspace, allowing `box ps` to work without needing a centralized daemon (though individual box daemons still handle I/O).

## 4. Herd Integration (`userspace/herd`)

Update `ServiceConfig` to support boxing:

```toml
# /etc/herd/enabled/my-service.conf
command = /bin/myservice
boxed = true
box_root = /data/jail/myservice
```

`herd` will use the `sys_spawn_ext` (or equivalent) mechanism directly, bypassing the `box` CLI tool for system services, effectively acting as the "box daemon" for those services.

## 5. Implementation Steps

1.  **Kernel VFS:** Implement `root_dir` logic in `Process` and VFS resolution.
2.  **Kernel ProcFS:** Implement `box_id` filtering.
3.  **Syscall:** Implement `sys_spawn_ext` (or modify `sys_spawn` logic).
4.  **Userspace:** Create `userspace/box` binary.
5.  **Herd:** Update parser and spawn logic.

## 6. Future Considerations

*   **Networking Isolation:** Later phases can introduce network namespaces (separate IP stacks per box).
*   **Resource Limits:** Cgroup-like CPU/Memory limits per box.
*   **Image Management:** Tools to extract/manage root filesystems (e.g., from docker images).
