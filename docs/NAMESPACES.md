# Mount Namespaces

Per-box mount namespaces allow each container (box) to have its own filesystem mounts (e.g., `/proc`, `/tmp`) independent of the host. Box definitions persist on disk as OCI bundles and are manually started by herd or users.

See also: [Mount Namespace Proposal](../proposals/MOUNT_NAMESPACE_PROPOSAL.md), [Box Containers](BOX_CONTAINERS.md)

## How It Works

Previously, all filesystem mounts lived in a single global `MOUNT_TABLE`. Every process — host or container — shared the same mount points. Container isolation relied solely on `root_dir` path prefix scoping.

Now each box can have its own mount namespace. The VFS path resolution checks the box-local namespace **before** applying `root_dir` scoping:

1. Resolve relative path against CWD to get an absolute path
2. If `box_id > 0`: check the box's `MountNamespace` — if the path matches a box-local mount, use it and return
3. Apply `root_dir` scoping (prepend `root_dir` to the path)
4. Resolve against the global host mount table

This means a box can have its own `/proc` (seeing only its own processes) and `/tmp` (private tmpfs) while still accessing the host ext2 filesystem for everything else via `root_dir` scoping.

## Kernel Changes

### VFS Layer (`src/vfs/mod.rs`)

- `MountEntry.fs` changed from `Box<dyn Filesystem>` to `Arc<dyn Filesystem>` to allow sharing filesystem instances across namespaces.
- `MountNamespace` struct: per-box mount table with up to 8 entries, supporting `mount`, `unmount`, and longest-prefix path resolution.
- `MOUNT_NAMESPACES`: global registry (`BTreeMap<u64, MountNamespace>`) keyed by `box_id`.
- `with_fs()` modified to check box-local namespace before `root_dir` scoping.
- `get_child_mount_points()` updated to include namespace mounts in directory listings.

Public API:
- `create_mount_namespace(box_id)` — create an empty namespace for a box
- `remove_mount_namespace(box_id)` — tear down a box's namespace
- `mount_in_namespace(box_id, path, fs)` — mount a filesystem in a box's namespace
- `unmount_in_namespace(box_id, path)` — unmount from a box's namespace

### Syscalls (`src/syscall.rs`)

Three new syscalls:

| Syscall | Number | Signature | Description |
|---------|--------|-----------|-------------|
| `mount` | 40 | `mount(source, target, fstype, flags, data)` | Mount in the caller's box namespace. Box 0 mounts to the global table. Supports `proc` and `tmpfs`. |
| `umount2` | 39 | `umount2(target, flags)` | Unmount from the caller's box namespace. |
| `mount_in_ns` | 325 | `mount_in_ns(box_id, target, target_len, fstype, fstype_len)` | Mount into a foreign box's namespace. Only callable from box 0 (host). Used by herd to set up container mounts. |

### Box Lifecycle

- `sys_register_box` now calls `create_mount_namespace()` to create a namespace when a box is registered.
- `sys_kill_box` now calls `remove_mount_namespace()` to clean up before killing processes.

## OCI Bundle Format

Boxes are stored on disk as OCI bundles at `/var/boxes/<name>/`:

```
/var/boxes/alpine/
    config.json     # OCI runtime spec (subset)
    rootfs/         # Container root filesystem
```

Supported subset of `config.json`:

```json
{
    "ociVersion": "1.0.2",
    "root": { "path": "rootfs" },
    "process": {
        "args": ["/bin/sh"],
        "env": ["PATH=/usr/bin:/bin"],
        "cwd": "/"
    },
    "mounts": [
        { "destination": "/proc", "type": "proc", "source": "proc" },
        { "destination": "/tmp", "type": "tmpfs", "source": "tmpfs" }
    ]
}
```

Ignored fields (for now): `linux.namespaces`, `linux.resources`, `linux.seccomp`, `linux.devices`, `annotations`.

Bundles persist across reboots. Boxes are transient — they must be explicitly started after each boot.

## Herd Integration

Herd service configs support a `bundle` option:

```ini
command = /bin/sh
bundle = /var/boxes/alpine
restart_delay = 1000
```

When `bundle` is set, herd:

1. Reads `config.json` from the bundle directory
2. Registers the box in the kernel (creating a mount namespace)
3. Sets up mounts from the OCI `mounts` array via `MOUNT_IN_NS` syscall
4. Spawns the process defined in `process.args` with `root_dir` pointing to the bundle's rootfs
5. On stop: kills the box, which tears down the mount namespace

## Userspace API (`libakuma`)

Two new functions:

- `mount(source, target, fstype) -> i32` — wraps `SYS_MOUNT` (40)
- `umount(target) -> i32` — wraps `SYS_UMOUNT2` (39)

## Files Modified

| File | Change |
|------|--------|
| `crates/akuma-vfs/src/mount.rs` | `Box<dyn Filesystem>` → `Arc<dyn Filesystem>` |
| `crates/akuma-vfs/src/tests.rs` | Updated to use `Arc` |
| `src/vfs/mod.rs` | Mount namespace registry, `with_fs()` resolution, namespace-aware `get_child_mount_points()` |
| `src/vfs/ext2.rs` | Return `Arc<dyn Filesystem>` |
| `src/fs.rs` | Use `Arc` for ProcFilesystem |
| `src/syscall.rs` | `mount`, `umount2`, `mount_in_ns` syscalls; namespace lifecycle in `register_box`/`kill_box` |
| `userspace/libakuma/src/lib.rs` | `mount()`, `umount()` wrappers |
| `userspace/herd/src/main.rs` | OCI config.json parser, `bundle` config option, mount setup |
