# FUSE: Userspace Filesystems for Akuma

Proposal for adding FUSE (Filesystem in Userspace) support to Akuma, enabling any userspace binary to implement a filesystem without kernel changes.

## Why FUSE

Akuma's kernel currently has two filesystem implementations: ext2 (read/write, on-disk) and memfs (in-memory). Adding a new filesystem today requires writing Rust `no_std` code inside the kernel and implementing the `Filesystem` trait in `src/vfs/mod.rs`. This limits what filesystems are available and ties every addition to a kernel rebuild.

FUSE inverts this: the kernel provides a generic forwarding layer, and the actual filesystem logic runs as a normal userspace process. Any FUSE-compatible binary compiled for aarch64-musl can provide a filesystem. The ecosystem of existing FUSE implementations becomes available without kernel work:

- **squashfuse** — mount compressed SquashFS images read-only (OCI container layers)
- **overlayfs-fuse** — union/overlay filesystem (compose OCI layers into a single view)
- **sshfs** — mount remote directories over SSH
- **s3fs-fuse** — mount S3 buckets as local directories
- **ntfs-3g** — NTFS read/write support
- **rclone mount** — mount any cloud storage (Google Drive, Dropbox, etc.)
- **archivemount** — mount tar/zip archives as directories

## Architecture

```
┌──────────────────────────────────────────┐
│  Userspace                               │
│                                          │
│  ┌──────────────┐   ┌────────────────┐   │
│  │  sshfs       │   │  squashfuse    │   │
│  │  (daemon)    │   │  (daemon)      │   │
│  └──────┬───────┘   └───────┬────────┘   │
│         │ read/write        │            │
│         ▼                   ▼            │
│     /dev/fuse fd        /dev/fuse fd     │
├─────────┼───────────────────┼────────────┤
│  Kernel │                   │            │
│         ▼                   ▼            │
│     FuseChannel         FuseChannel      │
│         │                   │            │
│     FuseFs              FuseFs           │
│     mounted at          mounted at       │
│     /mnt/remote         /mnt/squash      │
│         │                   │            │
│     VFS mount table                      │
│         │                                │
│     syscall layer (read, write, stat...) │
└──────────────────────────────────────────┘
```

### Data Flow: `cat /mnt/remote/hello.txt`

```
1. Process calls sys_read(fd) on an open file under /mnt/remote/
2. VFS resolves /mnt/remote/ → FuseFs instance
3. FuseFs serializes a FUSE_READ request into the FuseChannel
4. Daemon's read() on /dev/fuse fd returns the serialized request
5. Daemon processes it (e.g. sshfs reads from SSH connection)
6. Daemon writes the FUSE_READ response back to /dev/fuse fd
7. FuseFs deserializes the response, returns data to VFS
8. VFS returns data to sys_read caller
```

## What Exists Today

| Component | Status | Location |
|-----------|--------|----------|
| `Filesystem` trait | Done | `src/vfs/mod.rs:169-271` — 20+ methods |
| VFS mount table | Done | `src/vfs/mod.rs:278-365` — `vfs::mount(path, fs)`, max 8 mounts |
| Pipes (blocking channel) | Done | `src/syscall.rs:163-292` — `KernelPipe` with waker-based blocking |
| Eventfd | Done | `src/syscall.rs:294-416` — full support including `EFD_SEMAPHORE` |
| Thread blocking/waking | Done | `src/threading.rs` — `schedule_blocking` + `get_waker_for_thread` |
| FD table per process | Done | `src/process.rs` — BTreeMap, up to 1024 fds |
| Epoll (basic) | Partial | `src/syscall.rs:4005+` — type-specific readiness checks |
| `/dev/` filesystem | No | Only hardcoded `/dev/null`, `/dev/urandom` in `sys_openat` |
| `mount` syscall | No | `vfs::mount()` is kernel-internal only |
| ioctl for devices | Minimal | Only fd 0-2 (stdin/stdout/stderr) |

## Implementation Plan

### Phase 1 — FuseChannel (kernel-side message passing)

A bidirectional channel between the kernel's `FuseFs` and the userspace daemon. Reuses the existing pipe/waker pattern from `KernelPipe`.

```rust
struct FuseChannel {
    request_queue: VecDeque<FuseRequest>,
    response_map: BTreeMap<u64, FuseResponse>,
    daemon_thread: Option<usize>,
    next_unique: u64,
}
```

- **Kernel side (FuseFs):** Serializes a VFS call into a `FuseRequest`, enqueues it, wakes the daemon, and blocks the calling thread until the matching `FuseResponse` arrives (keyed by `unique` ID).
- **Daemon side:** Reads serialized requests from `/dev/fuse`, processes them, writes serialized responses back.
- **Blocking:** Calling thread uses `schedule_blocking(timeout)` while waiting for a response. Daemon uses the same `schedule_blocking` pattern when the request queue is empty.
- **Concurrency:** Multiple kernel threads can have in-flight requests simultaneously (each keyed by `unique`). The daemon processes them one at a time (or a multithreaded daemon can process them in parallel).

### Phase 2 — `/dev/fuse` file descriptor type

Add a new `FileDescriptor::FuseDev(channel_id)` variant.

**Opening:** Hardcode `/dev/fuse` in `sys_openat` (same pattern as `/dev/null`), or add a `FUSE_DEV_OPEN` custom syscall that returns a FuseDev fd.

**Read (daemon reads requests):**
```
sys_read(fuse_fd, buf) → dequeue next FuseRequest, serialize into buf
                         if queue empty: block via schedule_blocking
```

**Write (daemon writes responses):**
```
sys_write(fuse_fd, buf) → deserialize FuseResponse, insert into response_map
                          wake the kernel thread waiting for this unique ID
```

**Epoll integration:** `FuseDev` reports `EPOLLIN` when the request queue is non-empty.

### Phase 3 — FuseFs (VFS implementation)

`FuseFs` implements the `Filesystem` trait by forwarding every call through the `FuseChannel`:

```rust
struct FuseFs {
    channel_id: u32,
    mount_path: String,
}

impl Filesystem for FuseFs {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError> {
        let req = FuseRequest::Read { path, ... };
        let resp = channel_send_and_wait(self.channel_id, req)?;
        match resp {
            FuseResponse::Data(bytes) => Ok(bytes),
            FuseResponse::Error(e) => Err(e.into()),
        }
    }
    // ... same pattern for all Filesystem methods
}
```

### Phase 4 — Mount interface

Two options (can implement both):

**Option A: Custom syscall**
```rust
sys_fuse_mount(fuse_fd, mount_path_ptr, mount_path_len) -> Result<(), Errno>
```
Creates a `FuseFs` backed by the given FuseDev fd and calls `vfs::mount(path, fuse_fs)`. Restricted to processes with appropriate permissions (e.g., not inside a box, or box with mount capability).

**Option B: Kernel shell command**
```
mount -t fuse /dev/fuse /mnt/remote
```
Calls `vfs::mount` internally. Simpler, no new syscall, but only available from the kernel shell.

### Phase 5 — FUSE protocol compatibility (optional)

The above phases use a simplified Akuma-specific binary protocol. For compatibility with existing Linux FUSE daemons (sshfs, squashfuse, etc.), implement the standard FUSE kernel protocol:

- `FUSE_INIT` — negotiate protocol version and capabilities
- `FUSE_LOOKUP` — resolve a name in a directory (returns inode + attributes)
- `FUSE_GETATTR` — get file attributes by inode
- `FUSE_READ` / `FUSE_WRITE` — read/write file data
- `FUSE_READDIR` / `FUSE_READDIRPLUS` — list directory entries
- `FUSE_OPEN` / `FUSE_RELEASE` — open/close file handles
- `FUSE_MKDIR` / `FUSE_UNLINK` / `FUSE_RMDIR` — directory operations
- `FUSE_RENAME` — rename files
- `FUSE_STATFS` — filesystem statistics
- `FUSE_CREATE` — create + open in one call
- `FUSE_SYMLINK` / `FUSE_READLINK` — symbolic links

The protocol is well-documented: each request has a `fuse_in_header` (len, opcode, unique, nodeid, uid, gid, pid) followed by opcode-specific data. Responses have a `fuse_out_header` (len, error, unique) followed by response data.

**Compatibility tradeoff:** Full FUSE protocol compatibility lets existing daemons work unmodified. But the protocol is inode-based (not path-based like Akuma's `Filesystem` trait), so `FuseFs` would need to maintain a path-to-inode mapping or the `Filesystem` trait would need inode-based methods (some already exist: `resolve_inode`, `read_at_by_inode`).

## FUSE Protocol: Akuma-Specific vs. Linux-Compatible

| Approach | Effort | Benefit |
|----------|--------|---------|
| **Akuma-specific protocol** | ~500 lines | Simple, path-based, matches `Filesystem` trait directly. Custom daemons only. |
| **Linux FUSE protocol** | ~1500 lines | Any existing FUSE daemon works unmodified (sshfs, squashfuse, etc.). Requires inode management in kernel. |
| **Hybrid** | ~800 lines | Akuma-specific first, add Linux FUSE compat header translation later. |

**Recommendation:** Start with the Akuma-specific protocol (Phases 1-4). This validates the architecture and lets you write custom FUSE daemons in Rust using `libakuma`. Add Linux FUSE protocol compatibility (Phase 5) later when you want to run unmodified sshfs/squashfuse binaries.

## Performance

FUSE adds one kernel-to-userspace round-trip per VFS operation:

| VFS operation | Direct (ext2) | FUSE overhead | Total with FUSE |
|---------------|---------------|---------------|-----------------|
| read 4KB block | ~10 us | ~20-50 us (2 context switches + serialize) | ~30-60 us |
| readdir | ~5 us | ~20-50 us | ~25-55 us |
| stat | ~2 us | ~20-50 us | ~22-52 us |
| write 4KB | ~15 us | ~20-50 us | ~35-65 us |

For I/O-bound workloads (reading large files, streaming), the overhead is amortized by using larger read/write buffers (e.g., 128KB per FUSE_READ). For metadata-heavy workloads (find, ls -R), the per-call overhead dominates. This is the same tradeoff as Linux FUSE.

**Mitigation:** The FUSE protocol supports `READDIRPLUS` (returns attributes with directory entries, avoiding separate stat calls) and attribute caching (`entry_valid`, `attr_valid` timeouts). These can be added in Phase 5.

## Relevance to Other Proposals

### Container Demo (DEMO_PROPOSAL.md)

OCI container images are layered tarballs. With FUSE:
- **squashfuse** mounts each layer as a read-only filesystem (no extraction needed)
- **overlayfs-fuse** stacks the layers into a single union view
- `box run --image node:22-alpine` becomes: pull layers, mount via squashfuse + overlay, set box root to the union mount

This eliminates the tar extraction step, reduces disk usage (layers stay compressed), and enables sharing base layers between containers.

### Kernel Modularization Plan

`FuseFs` is just another implementation of the `Filesystem` trait from `akuma-vfs`. It fits the generic composition model:

```rust
// Monolithic — ext2 in kernel
type MonolithicKernel = Kernel<VirtioBlock, Ext2Fs<VirtioBlock>, ...>;

// FUSE — ext2 in userspace via fuse-ext2 daemon
type FuseKernel = Kernel<VirtioBlock, FuseFs, ...>;
```

The VFS layer, syscalls, and all callers are unchanged regardless of which `Filesystem` implementation is mounted.

### Microkernel Direction

FUSE is the first step toward moving filesystem logic out of the kernel. Once proven stable, the same IPC pattern can be applied to other subsystems (network stack, block device drivers) — each becoming a userspace server that implements a kernel trait via message passing.

## Estimated Effort

| Phase | Lines of kernel code | Depends on |
|-------|---------------------|------------|
| 1. FuseChannel | ~200 | Existing pipe/waker infrastructure |
| 2. /dev/fuse fd | ~150 | Phase 1 |
| 3. FuseFs | ~300 | Phase 1, akuma-vfs trait |
| 4. Mount interface | ~100 | Phase 3 |
| 5. Linux FUSE compat | ~800 | Phase 1-4, inode mapping |
| **Total (Phases 1-4)** | **~750** | |
| **Total (all)** | **~1550** | |

Plus a test daemon (~200 lines of Rust in `userspace/`) that implements a simple in-memory filesystem over the FUSE channel, to validate the round-trip.

## Success Criteria

1. A userspace Rust daemon can mount a directory via `/dev/fuse` and serve file reads/writes
2. Processes (including those inside a box) can transparently access FUSE-mounted paths via standard syscalls
3. `ls`, `cat`, `cp` work on FUSE-mounted files without modification
4. (Phase 5) An unmodified `sshfs` binary compiled for aarch64-musl can mount a remote directory
