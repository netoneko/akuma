# Process Isolation for Akuma (`akuma-isolation`)

Proposal for a unified isolation crate combining mount namespaces, network namespaces (stubbed), and future namespace types. Replaces the current ad-hoc `root_dir` prefix approach for container filesystem isolation and lays groundwork for per-container networking.

## Problem

Container processes need to see a different filesystem tree than the host. Today, `box open --image` sets `process.root_dir` to the image rootfs path, and the VFS's `with_fs` helper prepends this prefix to all paths. This has several problems:

1. **Inconsistent scoping.** Not all syscall paths go through `with_fs`. Some resolve paths directly (e.g., `sys_execve` calls `resolve_symlinks` then `fs::read_file`, each applying root_dir independently). The result is that some operations work and others silently use host paths.

2. **No `/proc`, `/dev`, `/tmp` inside containers.** These are host-level constructs. A container that opens `/proc/self/maps` sees the host procfs. There's no way to bind-mount a subset of devices or provide a container-specific `/tmp`.

3. **Symlink resolution leaks.** `resolve_symlinks` follows symlinks through `with_fs` (which adds root_dir), but the returned path is in the process-local namespace. If any symlink target is absolute (e.g., `/usr/lib/libfoo.so → /lib/libfoo.so`), the resolution chain jumps between scoped and unscoped lookups depending on which function processes the intermediate path.

4. **No overlay/union filesystem support.** OCI images are layered. Running a container currently requires extracting all layers into a single directory. With mount namespaces, layers could be composed using an overlay mount.

5. **No shared vs. private mounts.** All processes see the same global mount table. A FUSE mount created inside a container is visible to every process.

## Relationship to FUSE

The FUSE proposal (`proposals/FUSE_PROPOSAL.md`) adds userspace filesystem drivers. Mount namespaces are complementary:

- **FUSE** answers "how to implement a filesystem in userspace" (squashfuse, overlayfs-fuse, sshfs)
- **Mount namespaces** answer "which processes see which mounts"

Together they enable the full container story:
1. Pull OCI image layers (already implemented in `box pull`)
2. Mount each layer read-only via squashfuse (FUSE)
3. Stack layers with overlayfs-fuse (FUSE)
4. Create a mount namespace for the container with the overlay as `/`, plus bind-mounts for `/proc`, `/dev/null`, `/dev/urandom`
5. `execve` inside the namespace — the process sees a complete Linux-like filesystem

Mount namespaces can be implemented **before** FUSE. The immediate benefit is correct container filesystem isolation using the current tar-extraction approach.

## Design

### Mount Table Per Namespace

```
┌─────────────────────────────────────────────┐
│  Global Mount Namespace (ns_id=0)           │
│    /        → ext2 (disk)                   │
│    /proc    → procfs                        │
│    /dev     → devfs (future)                │
├─────────────────────────────────────────────┤
│  Container Namespace (ns_id=1)              │
│    /        → ext2 subdir view              │
│              (/var/lib/box/images/X/rootfs)  │
│    /proc    → procfs (virtualized)          │
│    /dev     → devfs (restricted)            │
│    /tmp     → memfs (private)               │
├─────────────────────────────────────────────┤
│  Container Namespace (ns_id=2)              │
│    /        → overlay(layer1, layer2, ...)  │
│    /proc    → procfs (virtualized)          │
│    ...                                      │
└─────────────────────────────────────────────┘
```

Each mount namespace is a separate mount table. Namespaces are reference-counted (shared by all processes in the same container). Fork inherits the parent's namespace.

### Namespace — Top-Level Container

All namespace types are grouped into a single `Namespace` struct. Processes hold `Arc<Namespace>`, which is cloned on fork.

```rust
pub struct Namespace {
    pub id: u32,
    pub mount: MountNamespace,
    pub net: NetworkNamespace,
    // Future: pub pid: PidNamespace,
}
```

### NetworkNamespace (Stubbed)

Network namespaces control which network stack a process uses. For now this is a simple enum that defaults to the shared global stack — no behavioral change from today.

```rust
pub enum NetworkNamespace {
    /// Use the global shared network stack (current behavior).
    Shared,
    // Future:
    // Isolated { veth_id: u32, ip: Ipv4Addr, ... },
}
```

When `Isolated` is implemented later, the kernel's network glue layer will check `proc.namespace.net` and route to the appropriate virtual interface. Until then, `Shared` means all container processes share the single smoltcp stack, exactly as they do today.

### SubdirFs — Scoped View of an Existing Filesystem

For the current tar-extraction approach (no FUSE yet), we need a way to present a subdirectory of ext2 as the root of a new mount. `SubdirFs` wraps an existing `Filesystem` impl and prefixes all paths:

```rust
struct SubdirFs {
    inner: Arc<dyn Filesystem>,
    prefix: String, // e.g., "/var/lib/box/images/alpine/rootfs"
}

impl Filesystem for SubdirFs {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError> {
        let full = format!("{}{}", self.prefix, path);
        self.inner.read_file(&full)
    }
    // ... same for all methods
}
```

This replaces the `root_dir` prefix hack. The container's namespace has `SubdirFs` mounted at `/`, so all path resolution is handled by the VFS mount table — no special-casing in syscalls.

### Path Resolution

Today's approach:
```
sys_openat("/bin/ls") → with_fs adds root_dir → ext2.read("var/lib/box/.../bin/ls")
```

Proposed approach:
```
sys_openat("/bin/ls") → namespace.resolve("/bin/ls")
                      → finds mount at "/"  → SubdirFs
                      → SubdirFs.read("/bin/ls")
                      → ext2.read("var/lib/box/.../rootfs/bin/ls")
```

The key difference: path scoping happens in the mount table, not in individual syscalls. Every path operation goes through `namespace.resolve()`, which is guaranteed to be consistent.

### Process Integration

```rust
struct Process {
    // Replace root_dir: String with:
    namespace: Arc<Namespace>,
    // ...
}
```

`with_fs` changes from checking `proc.root_dir` to looking up the mount in `proc.namespace.mount`:

```rust
fn with_fs<F, R>(path: &str, f: F) -> Result<R, FsError> {
    let absolute = if let Some(proc) = current_process() {
        let abs = resolve_path(&proc.cwd, path);
        let ns = proc.namespace.clone();
        let table = ns.mount.mounts.lock();
        let (fs, rel) = table.resolve(&abs).ok_or(FsError::NotFound)?;
        return f(fs, rel);
    };
    // fallback for kernel context: use global namespace
    let table = GLOBAL_NS.mount.mounts.lock();
    let (fs, rel) = table.resolve(&normalize_path_owned(path)).ok_or(FsError::NotFound)?;
    f(fs, rel)
}
```

### Creating a Container Namespace

When `box open --image` spawns a process:

```rust
// 1. Create a new mount namespace
let mount_ns = MountNamespace::new();

// 2. Mount the rootfs as /
let rootfs = SubdirFs::new(ext2_arc.clone(), "/var/lib/box/images/alpine/rootfs");
mount_ns.mount("/", Box::new(rootfs));

// 3. Bind-mount essential pseudofilesystems
mount_ns.mount("/proc", Box::new(ProcFs::new_for_container(box_id)));
mount_ns.mount("/tmp", Box::new(MemFs::new()));

// 4. Create the combined namespace (network is Shared by default)
let ns = Namespace {
    id: next_ns_id(),
    mount: mount_ns,
    net: NetworkNamespace::Shared,
};

// 5. Set on the new process
process.namespace = Arc::new(ns);
```

## Crate Layout

```
crates/akuma-isolation/
  Cargo.toml            # depends on akuma-vfs, spinning_top, alloc
  src/
    lib.rs              # pub mod mount, net, subdir_fs; re-exports Namespace
    mount.rs            # MountNamespace struct
    net.rs              # NetworkNamespace enum (stubbed)
    subdir_fs.rs        # SubdirFs implements Filesystem
```

## Implementation Plan

### Phase 1 — `akuma-isolation` crate + per-process Namespace

- Create `crates/akuma-isolation/` with `Namespace`, `MountNamespace`, `NetworkNamespace`, `SubdirFs`
- `NetworkNamespace` is just `enum { Shared }` for now — no wiring into smoltcp
- Create a global default `Namespace` from the current `MOUNT_TABLE` static
- Add `namespace: Arc<Namespace>` to `Process`
- Change `with_fs` to use `proc.namespace.mount` instead of the global table + root_dir hack
- Fork copies the `Arc` (shared namespace by default)
- **No behavior change yet** — all processes use the global namespace

### Phase 2 — SubdirFs

- Implement `SubdirFs` that wraps a `Filesystem` with a path prefix
- Implement all `Filesystem` trait methods by delegation with prefix
- Test: mount a SubdirFs at `/mnt/test` pointing to an existing directory, verify reads/writes

### Phase 3 — Container namespace creation

- When `box open --image` spawns a process, create a new `Namespace` with `SubdirFs` at `/` and `NetworkNamespace::Shared`
- Mount procfs at `/proc` (use existing procfs, potentially virtualized)
- Remove `root_dir` field and all `root_dir` special-casing from:
  - `with_fs` (replaced by namespace lookup)
  - `spawn_process_with_channel_ext` (no more path prefixing)
  - `elf_loader` (no more `interp_prefix` parameter)
  - `replace_image` / `replace_image_from_path`
  - `sys_execve` path resolution
- Test: `box open -i alpine /bin/sh` → `ls`, `cat`, `exec` all work

### Phase 4 — Private mounts and `/dev`

- Add a minimal `DevFs` for `/dev/null`, `/dev/urandom`, `/dev/zero`
- Container namespaces mount `DevFs` at `/dev`
- Private `/tmp` via `MemFs::new()` per container
- `unshare` semantics: allow creating a copy of the namespace for modification

### Phase 5 — Network namespace isolation (future)

- Add `NetworkNamespace::Isolated { veth_id, ip, ... }` variant
- Kernel network glue checks `proc.namespace.net` to route to the correct virtual interface
- Each container gets its own IP, port space, and routing table
- Wire into smoltcp's interface management

### Phase 6 — Integration with FUSE

When FUSE is implemented (see `proposals/FUSE_PROPOSAL.md`):
- OCI layers mounted via squashfuse as read-only filesystems
- Overlay composed via overlayfs-fuse
- The overlay is mounted at `/` in the container namespace instead of `SubdirFs`
- Eliminates tar extraction entirely — layers stay compressed on disk

## What Needs to Change (Audit)

Syscalls and kernel paths that currently do path resolution and need namespace-aware resolution:

| Syscall | Current resolution | Needs change |
|---------|-------------------|--------------|
| `sys_openat` | `resolve_symlinks` + `with_fs` | `with_fs` update covers it |
| `sys_execve` | `resolve_symlinks` + `fs::read_file` | `with_fs` update covers it |
| `sys_faccessat2` | `resolve_symlinks` + `fs::exists` | `with_fs` update covers it |
| `sys_fstatat` | `resolve_symlinks` + `vfs::metadata` | `with_fs` update covers it |
| `sys_readlinkat` | direct path check | Needs namespace-aware resolution |
| `sys_mkdirat` | `resolve_path_at` + `vfs::create_dir` | `with_fs` update covers it |
| `sys_unlinkat` | `resolve_path_at` + `vfs::remove` | `with_fs` update covers it |
| `sys_symlinkat` | `resolve_path_at` + `vfs::create_symlink` | `with_fs` update covers it |
| `sys_linkat` | `resolve_path_at` + `fs::read_file/write_file` | `with_fs` update covers it |
| `sys_renameat` | `resolve_path_at` + `fs::rename` | `with_fs` update covers it |
| `sys_getcwd` | returns `proc.cwd` | No change needed (cwd is namespace-relative) |
| `sys_chdir` | sets `proc.cwd` | Needs existence check via namespace |
| `sys_spawn` | `resolve_symlinks` + `read_file` | Covered by `with_fs` |
| `sys_spawn_ext` | reads path + options | Covered by `with_fs` + remove root_dir logic |
| ELF loader | `interp_prefix` hack | Replace with namespace-aware `read_file` |
| Shebang handler | `resolve_symlinks` on interpreter | Covered by `with_fs` |

The key insight: if `with_fs` correctly uses the process's mount namespace, most syscalls require **zero changes** because they already delegate to `with_fs` indirectly through `vfs::*` and `fs::*` functions. The remaining work is:

1. `resolve_symlinks` — must also go through namespace-aware resolution (it already calls `read_symlink` which uses `with_fs`, so it should work)
2. ELF loader interpreter loading — uses `runtime().read_file` which goes through the VFS, but the process hasn't been created yet during spawn. Needs special handling (pass namespace explicitly or set up namespace before loading ELF).
3. `sys_readlinkat` for `/proc/self/exe` — special case, no mount involved

## Estimated Effort

| Phase | Lines of code | Depends on |
|-------|--------------|------------|
| 1. akuma-isolation crate + per-process Namespace | ~200 | None |
| 2. SubdirFs | ~200 | Phase 1 |
| 3. Container namespace creation | ~200 + cleanup | Phase 2 |
| 4. DevFs + private mounts | ~150 | Phase 3 |
| 5. Network namespace isolation | ~300 | Phase 3 + smoltcp veth |
| 6. FUSE integration | ~100 | Phase 4 + FUSE proposal |
| **Total (Phases 1-4, immediate)** | **~750** | |

## Success Criteria

1. `box open -i alpine /bin/sh` drops into a shell where `ls`, `cat /etc/os-release`, `env` all work
2. Container processes cannot access host paths outside their rootfs
3. `/proc` inside a container shows only the container's processes
4. Multiple containers can run simultaneously with independent filesystem views
5. Host processes are unaffected — all existing behavior preserved
