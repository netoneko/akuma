# akuma-isolation

Process isolation primitives for the Akuma kernel. Provides mount namespaces, a stubbed network namespace, and `SubdirFs` for scoped filesystem views.

This crate is `no_std` and depends on `alloc`.

## Overview

Each process in Akuma holds an `Arc<Namespace>` that controls which filesystems it can see. Host processes share a global namespace with an empty mount table (falling through to the kernel's global mount table). Container processes get their own namespace with a `SubdirFs`-backed root, providing filesystem isolation without any special-casing in syscall paths.

## Modules

### `mount` — Mount Namespaces

`MountNamespace` is a per-namespace mount table mapping paths to `Arc<dyn Filesystem>` implementations. Up to 16 mounts per namespace.

```rust
use akuma_isolation::mount::MountNamespace;

let mut ns = MountNamespace::new();
ns.mount("/", root_fs.clone())?;
ns.mount("/proc", proc_fs.clone())?;

// Resolve a path to (filesystem, relative_path)
if let Some((fs, rel_path)) = ns.resolve("/proc/self/maps") {
    // fs = proc_fs, rel_path = "/self/maps"
}
```

### `net` — Network Namespaces (Stubbed)

```rust
use akuma_isolation::net::NetworkNamespace;

// Currently only one variant — all processes share the global network stack
let net = NetworkNamespace::Shared;
```

### `subdir_fs` — Scoped Filesystem Views

`SubdirFs` wraps an existing filesystem and prefixes all paths with a base directory. This makes a subdirectory appear as root when mounted at `/` in a namespace.

```rust
use akuma_isolation::subdir_fs::SubdirFs;

// Make /var/lib/box/images/alpine/rootfs appear as /
let scoped = SubdirFs::new(ext2_fs.clone(), "/var/lib/box/images/alpine/rootfs");
// scoped.read_file("/bin/sh") → ext2.read_file("/var/lib/box/images/alpine/rootfs/bin/sh")
```

Path concatenation uses a stack-allocated buffer (`full_path!` macro) to avoid heap allocation on the VFS hot path. Falls back to heap only for paths exceeding 512 bytes.

### `lib` — Namespace Container

`Namespace` groups all namespace types together. Processes hold `Arc<Namespace>`.

```rust
use akuma_isolation::Namespace;

let ns = Namespace::new(1); // id=1
// ns.mount — Spinlock<MountNamespace>
// ns.net   — NetworkNamespace::Shared
```

The global host namespace (id=0) is available via `global_namespace()`.

## Dependencies

- `akuma-vfs` — `Filesystem` trait, `FsError`, `DirEntry`, `FS_MAX_PATH_SIZE`
- `spinning_top` — Spinlock for `MountNamespace` within `Namespace`

## Further Reading

- [Implementation Details](docs/IMPLEMENTATION_DETAILS.md) — design decisions, kernel integration, internals
- [Mount Namespace Proposal](../../../proposals/MOUNT_NAMESPACE_PROPOSAL.md) — original design document (Phases 1–6)
