# Container ELF Loading

## Problem

When spawning a process inside a container (`box open --image`), the kernel needs to find both the binary and its dynamic linker inside the container's rootfs, not at the global VFS paths.

Before this fix, `spawn_process_with_channel_ext` resolved paths like `/bin/ash` against the global VFS. Since the actual file lives at `/var/lib/box/images/busybox/rootfs/bin/ash`, the spawn always failed with "failed to spawn".

The same issue affected the ELF interpreter (dynamic linker). A binary with `PT_INTERP = /lib/ld-linux-aarch64.so.1` would cause the loader to look for `/lib/ld-linux-aarch64.so.1` in the host filesystem, not inside the container rootfs.

## Fix

### Binary path resolution

In `spawn_process_with_channel_ext`, when `root_dir` is set to something other than `/`, the binary path is prefixed with `root_dir` before symlink resolution and ELF loading:

```
/bin/ash → /var/lib/box/images/busybox/rootfs/bin/ash → (resolve symlinks) → .../rootfs/bin/busybox
```

`argv[0]` is left as `/bin/ash` so multi-call binaries (busybox, toybox) can identify which applet to run.

### Interpreter path resolution

The `interp_prefix` parameter was added through the ELF loading chain:

- `load_elf(elf_data, interp_prefix)`
- `load_elf_from_path(path, file_size, interp_prefix)`
- `load_elf_with_stack(elf_data, args, env, stack_size, interp_prefix)`
- `load_elf_with_stack_from_path(path, file_size, args, env, stack_size, interp_prefix)`
- `Process::from_elf(name, args, env, elf_data, interp_prefix)`
- `Process::from_elf_path(name, path, file_size, args, env, interp_prefix)`

When `interp_prefix` is `Some("/var/lib/box/images/busybox/rootfs")`, the PT_INTERP path `/lib/ld-linux-aarch64.so.1` is resolved to `/var/lib/box/images/busybox/rootfs/lib/ld-linux-aarch64.so.1`.

### execve within containers

`replace_image` and `replace_image_from_path` (the `execve` path) use `self.root_dir` from the existing Process struct, so programs that exec other binaries from within a container also resolve the interpreter correctly.

## Current limitations and future work

This is a minimal fix to get container binaries loading. Full process isolation requires significantly more work:

- **Path scoping in syscalls**: File I/O syscalls (`openat`, `stat`, `readlink`, etc.) need to be scoped to the container rootfs. Currently a process inside a box can still access host paths if it uses absolute paths that happen to exist.
- **Mount namespace**: Containers should have their own mount table with `/proc`, `/dev`, `/tmp` correctly populated rather than sharing the host VFS.
- **`/proc/self/exe`**: Currently hardcoded to return the process name. Inside a container it should return the path relative to the rootfs.
- **Shared library search paths**: The dynamic linker inside the container will look for shared libraries at paths like `/lib/libc.so.6`. These need to resolve within the rootfs. Currently this works because the ELF loader maps everything at load time, but `dlopen` at runtime would fail.
- **Device access**: Containers should have restricted access to devices. Currently `/dev/null`, `/dev/urandom` etc. are provided by the kernel, but there's no per-container device policy.
- **Resource limits**: No per-container memory, CPU, or file descriptor limits exist yet.
- **Network namespace**: All containers share the host network stack.
- **User namespace / UID mapping**: No UID remapping. All processes run as the same user.

See also: `proposals/DEMO_PROPOSAL.md` and `proposals/FUSE_PROPOSAL.md` for broader container roadmap ideas.
