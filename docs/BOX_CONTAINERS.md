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

*   **`box open <name> [--directory <dir>] [--tmp] <cmd>`**:
    *   Starts a new box daemon.
    *   Spawns `<cmd>` with `root_dir` set to `<dir>`.
    *   `--tmp` (or `-rm`): Equivalent to `docker run --rm`. The root directory is a temporary overlay or the box is deleted upon primary process exit.
*   **`box use <name> <cmd>`**:
    *   Equivalent to `docker exec`.
    *   Injects a new process into an existing box.
    *   Requires the kernel to support `sys_spawn_ext` with an existing `box_id`.
*   **`box peek <name>`**:
    *   Equivalent to `docker attach`.
    *   Attaches to the stdout/stdin of the box's primary process.
    *   Uses a Unix socket for IPC with the daemon.
*   **`box close <name|id>`**:
    *   Equivalent to `docker stop` / `docker rm -f`.
    *   Sends a termination signal to all processes in the box.
    *   The daemon performs cleanup of IPC sockets and temporary mounts.
*   **`box ps`**:
    *   Equivalent to `docker ps`.
    *   Lists all active boxes, their IDs, names, root directories, and primary process PIDs.
*   **`box show <name|id>`**:
    *   Equivalent to `docker inspect`.
    *   Displays detailed information about a box: uptime, resource usage (if available), and a list of all member PIDs.
*   **`box cp <source_dir> <box_name>`**:
    *   Helper command to initialize a box's root directory.
    *   Copies a template or base system into the target directory before `box open`.
    *   Useful for setting up isolated environments for `herd` services.

### 2.3 Docker-Compatible Aliases

To ease transition for users familiar with Docker, the `box` utility should support the following aliases:

| Box Command | Docker Equivalent | Description |
|-------------|-------------------|-------------|
| `box open` | `docker run` | Create and start a container |
| `box use` | `docker exec` | Run a command in a running container |
| `box peek` | `docker attach` | Attach to a container's IO |
| `box close` | `docker stop` / `rm` | Stop and remove a container |
| `box ps` | `docker ps` | List containers |
| `box show` | `docker inspect` | Display container details |

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
*   **`ps` and `procfs` Updates**:
    *   The system `ps` command (and `/proc/all`) should be updated to show a `BOX` column.
    *   It should display the `box_id` (or `None`/`Host`) for each process.
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

## 6. Implementation Recommendations

### 6.1 Supervisor & Shell Strategy

*   **Box Daemon as Supervisor:** For `box open`, the `box` utility acts as the primary supervisor and reaper. For complex multi-service containers, `herd` should be run as the entry point within the box.
*   **Standalone Shell:** The current kernel-integrated shell must be ported to `userspace/shell` to run as a process inside a box. The shell should support basic pipelines and I/O redirection using `libakuma`.
*   **Direct Execution (`noshell`):** `box use` should execute commands directly via the kernel rather than wrapping them in a shell. This avoids PID pollution and simplifies signal propagation.
*   **Terminal-less IO (`noterm`):** Use raw byte-piping for `box peek` and `box use`. The `ProcessChannel` should be treated as a transparent pipe, allowing the host's terminal to handle all escape sequences and line editing.

### 6.2 Attachment (`box peek`) Architecture

1.  **Daemon Ownership:** The `box` daemon maintains the `Arc<ProcessChannel>` for the container's primary process.
2.  **Unix Socket Proxy:** `box peek` connects to the daemon via a Unix socket.
3.  **Transparent Piping:** The daemon proxies data bi-directionally between the Unix socket and the `ProcessChannel` without modification. This ensures that interactive applications (like editors or `top`) render correctly.

### 6.3 ProcFS & Networking Support

*   **Network Sockets:** `procfs` must be extended to support `/proc/net/tcp` and `/proc/net/udp`. This allows tools like `netstat` to function inside boxes.
*   **Namespace Filtering:** Entries in `/proc/net/*` must be filtered by `box_id`. A process inside a box should only see the sockets belonging to processes within the same box (or host sockets if network isolation is not yet enforced).
*   **Process Isolation:** Existing `/proc/<pid>` entries must continue to be filtered so boxes cannot see host processes or other boxes.

### 6.4 Kernel Requirements for Injection (`box use`)

To support "injecting" a process into an existing box:
*   **`sys_spawn_ext`:** Must support a `target_box_id` or `target_pid` parameter.
*   **Context Inheritance:** The injected process must inherit the `root_dir` and `box_id` of the target.

## 7. Future Considerations

*   **Networking Isolation:** Later phases can introduce network namespaces (separate IP stacks per box).
*   **Resource Limits:** Cgroup-like CPU/Memory limits per box.
*   **Image Management:** Tools to extract/manage root filesystems (e.g., from docker images).
