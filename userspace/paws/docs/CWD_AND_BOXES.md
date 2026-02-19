# CWD and Box Isolation in Paws

This document describes how `paws` handles the Current Working Directory (CWD) and how it interacts with Akuma's "Box" containerization system.

## The Source of Truth: ProcessInfo

Unlike traditional Unix systems that often rely on the `$PWD` environment variable, Akuma uses a kernel-managed structure called `ProcessInfo`, mapped read-only at `0x1000` in every process's address space.

- **Storage:** The CWD is stored in the `cwd_data` field (256 bytes) of the `ProcessInfo` page.
- **Retrieval:** `paws` calls `libakuma::getcwd()`, which reads directly from this memory-mapped page.
- **Updates:** When you run `cd` in `paws`, it invokes the `sys_chdir` syscall. The kernel updates its internal process state **and** overwrites the data in the `ProcessInfo` page.

## Box Isolation (Root Scoping)

When `paws` runs inside a Box, it operates within a virtualized filesystem view. This is achieved through "Root Scoping" in the kernel VFS layer.

### Two-Layer Resolution

1.  **Process-Level CWD:** `paws` thinks it is in `/` (or any path relative to its virtual root).
2.  **Kernel-Level Root:** The kernel knows this process is actually restricted to a host path (e.g., `/boxes/mybox`).

When `paws` performs a filesystem operation (like `ls .`), the kernel performs the following translation:
`Relative Path` -> `Absolute Virtual Path (via CWD)` -> `Scoped Host Path (via root_dir)`

**Example:**
- **Box Root:** `/boxes/webserver`
- **Paws CWD:** `/var/www`
- **Command:** `cat index.html`
- **Resolution:**
    1. Resolve `index.html` against CWD (`/var/www`) -> `/var/www/index.html`
    2. Apply Box Scope -> `/boxes/webserver/var/www/index.html`

## Path Normalization

The kernel provides robust path normalization via `vfs::canonicalize_path`. This ensures that:
- `.` and `..` components are resolved before scoping is applied.
- Processes cannot escape their Box using `cd ../../`.
- The prompt in `paws` always shows a clean, absolute virtual path.

## Key Fixes (v0.3.0)

Previously, `paws` prompts would fail to update because:
1.  **Sync Missing:** The `sys_chdir` syscall updated the kernel's tracking but didn't refresh the `ProcessInfo` page.
2.  **Scoping Errors:** Relative paths were sometimes scoped against the host root instead of the virtual root.

These have been resolved by centralizing path resolution in the VFS layer and adding mandatory `ProcessInfo` synchronization to the `set_cwd` kernel method.
