# akuma-isolation — Implementation Details

This document covers the design decisions, internals, and integration points of the `akuma-isolation` crate. For usage and API overview, see the [README](../README.md). For the original proposal, see [`proposals/MOUNT_NAMESPACE_PROPOSAL.md`](../../../proposals/MOUNT_NAMESPACE_PROPOSAL.md).

## What Was Implemented (Phases 1–3)

The crate implements Phases 1–3 of the mount namespace proposal:

1. **Phase 1** — Created the crate with `Namespace`, `MountNamespace`, and a stubbed `NetworkNamespace`. Added `namespace: Arc<Namespace>` to `Process`, replacing the old `root_dir: String` field.
2. **Phase 2** — Implemented `SubdirFs`, a `Filesystem` adapter that presents a subdirectory as root. Optimized path concatenation with a stack-allocated `full_path!` macro.
3. **Phase 3** — Wired container namespace creation into the kernel. The old `root_dir` prefix hack, `interp_prefix` in the ELF loader, and `scope_path` in VFS were all removed.

## Architecture

### Namespace struct

```
Namespace
├── id: u64              — unique identifier (0 = host)
├── mount: Spinlock<MountNamespace>  — per-namespace mount table
└── net: NetworkNamespace            — network isolation (stubbed)
```

Processes hold `Arc<Namespace>`. Fork clones the `Arc` (shared namespace). Container processes get a new `Namespace` with their own mount table.

A lazily-initialized global namespace (`GLOBAL_NAMESPACE`) is used for host processes and kernel-context operations. It has `id=0` and an empty mount table (host path resolution falls through to the kernel's global `MOUNT_TABLE`).

### MountNamespace

A small ordered table of `(path, Arc<dyn Filesystem>)` entries, capped at 16 mounts per namespace. Mounts are sorted longest-path-first so `resolve()` finds the most specific match.

**Resolution algorithm:**
1. Normalize trailing slashes from the input path.
2. Iterate mounts (longest prefix first).
3. If mount path is `/`, return the filesystem with the full path.
4. If the input path equals or starts with the mount path (followed by `/`), return the filesystem with the remaining path suffix.
5. If no mount matches, return `None` (caller falls back to the global mount table).

### SubdirFs

Wraps an existing `Filesystem` and transparently prefixes all paths with a base directory. When mounted at `/` in a container namespace, it makes a subdirectory (e.g., `/var/lib/box/images/alpine/rootfs`) appear as the root filesystem.

All 18 `Filesystem` trait methods are implemented by delegation. `rename` is a special case — both the source and destination paths are prefixed independently.

### full_path! macro

Path concatenation in `SubdirFs` is on the VFS hot path. The `full_path!` macro avoids heap allocation for typical paths:

1. Compute the combined length of `prefix + path`.
2. If it fits in `FS_MAX_PATH_SIZE` (512 bytes, defined in `akuma-vfs`), use a stack-allocated `[u8; FS_MAX_PATH_SIZE]` buffer.
3. Otherwise, fall back to a heap-allocated `String`.
4. Bind the result as a `&str` for the caller.

The `/` path is handled specially — when the input path is `/`, only the prefix is used (no trailing slash appended).

### NetworkNamespace

Currently a single-variant enum (`Shared`). All processes use the global smoltcp network stack. The enum exists so that future network isolation (Phase 5 of the proposal) can add an `Isolated` variant without changing the `Namespace` struct.

## Kernel Integration

### VFS (`src/vfs/mod.rs`)

The `with_fs` function resolves paths through the process's namespace:

1. Check `SPAWN_NS_OVERRIDE` (temporary per-thread override during ELF loading).
2. Lock `proc.namespace.mount` and call `resolve(path)`.
3. If no namespace mount matches, fall through to the global `MOUNT_TABLE`.

This means host processes (with an empty global namespace) behave exactly as before — their namespace has no mounts, so resolution always hits the global mount table.

**Container namespace creation** (`create_box_namespace`):
- Allocates a new `Namespace` with the box ID.
- If `root_dir != "/"`, wraps the root ext2 filesystem in a `SubdirFs` scoped to `root_dir` and mounts it at `/` in the new namespace.
- Stores the namespace in `BOX_NAMESPACES` for lookup during spawn.

### Process spawning (`crates/akuma-exec/src/process.rs`)

`spawn_process_with_channel_ext` handles namespace assignment:

1. If `box_id != 0`, look up the box's namespace via `runtime().get_box_namespace(box_id)`.
2. Activate a temporary **spawn namespace override** so the ELF loader (which runs in the host context) resolves paths through the container's mount table.
3. After ELF loading, clear the override.
4. Assign the namespace to the new process.

The spawn namespace override is necessary because the ELF loader runs before the new process exists. Without it, `runtime().read_file()` would resolve paths against the host filesystem, failing to find binaries inside the container rootfs.

### Syscalls (`src/syscall.rs`)

- `sys_register_box` → calls `create_box_namespace(id, root_dir)`.
- `sys_kill_box` → calls `remove_box_namespace(box_id)`.
- `sys_mount` / `sys_umount2` → operate on `proc.namespace.mount` directly (container-scoped mounts).

### ELF loader

The `interp_prefix` parameter was removed from all ELF loading functions. Dynamic linker resolution (finding `ld-musl-aarch64.so.1`) now goes through the namespace-aware VFS automatically — `SubdirFs` handles the path translation.

`replace_image` and `replace_image_from_path` set `interp_prefix = None` when the process's namespace has mounts, letting the namespace handle path resolution.

## What Was Removed

- `Process.root_dir: String` — replaced by `Process.namespace: Arc<Namespace>`.
- `interp_prefix` in ELF loader calls — namespace handles path scoping.
- `scope_path` in VFS — `SubdirFs` does this at the filesystem level.
- `MOUNT_NAMESPACES: BTreeMap<u64, MountNamespace>` — replaced by `BOX_NAMESPACES: BTreeMap<u64, Arc<Namespace>>`.
- `root_dir` fields in `SpawnOptions`, `BoxInfo` (userspace side) — kernel-side namespace creation uses the root dir at registration time, userspace no longer passes it at spawn time.

## Concurrency

- `MountNamespace` is protected by a `Spinlock` inside `Namespace`. Mount/unmount/resolve all acquire this lock.
- `BOX_NAMESPACES` and `SPAWN_NS_OVERRIDE` are kernel-side statics protected by their own `Spinlock`s.
- `Arc<Namespace>` is cloned freely across threads. The `Spinlock` ensures safe concurrent access to the mount table.

## Limitations and Future Work

- **16 mounts per namespace** — hardcoded limit. Sufficient for current use (root + proc + tmp + dev = 4 mounts). Increase `MAX_NS_MOUNTS` if needed.
- **No overlay/union filesystem** — containers use `SubdirFs` over extracted tar contents. Phase 6 (FUSE integration) would replace this with squashfuse + overlayfs.
- **No private `/dev`** — Phase 4 of the proposal. Containers currently see host `/dev` if it exists.
- **No network isolation** — `NetworkNamespace::Shared` is the only variant. Phase 5 would add per-container virtual interfaces.
- **No `unshare` semantics** — all processes in a container share the same namespace `Arc`. Creating a modified copy (like Linux `unshare(CLONE_NEWNS)`) is not yet supported.
