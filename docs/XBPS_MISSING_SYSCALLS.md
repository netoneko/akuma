# XBPS — Missing Syscalls

Syscalls that were missing or needed attention to get XBPS running on Akuma.

## Implemented

### uname (160)

**Symptom:** `xbps-install` fails immediately with:
```
ERROR: Failed to initialize libxbps: Not supported
```

**Cause:** `xbps_init()` in `lib/initend.c:125` calls `uname()` to detect the machine architecture (`un.machine`). Akuma had no handler for syscall 160, so musl's `uname()` got `ENOSYS`, returned -1, and xbps returned `ENOTSUP`.

**Fix:** Added `sys_uname()` to `src/syscall.rs`. Fills the `utsname` struct (6 fields, 65 bytes each):

| Field | Value |
|-------|-------|
| sysname | `Akuma` |
| nodename | `akuma` |
| release | `0.1.0` |
| version | `Akuma OS` |
| machine | `aarch64` |
| domainname | `(none)` |

The `machine` field is the critical one — xbps uses it to select the correct architecture for package downloads (e.g., `aarch64-repodata`).

## Already implemented (used by XBPS)

| Syscall | Number | Notes |
|---------|--------|-------|
| openat | 56 | File I/O |
| close | 57 | File I/O |
| read | 63 | File I/O |
| write | 64 | File I/O |
| fstat | 80 | File metadata |
| newfstatat | 79 | File metadata with path |
| faccessat | 48 | File access checks |
| mkdirat | 34 | Directory creation |
| unlinkat | 35 | File/dir removal |
| getdents64 | 61 | Directory listing |
| getcwd | 17 | Current working directory |
| getuid/geteuid/getgid/getegid | 174-177 | All return 0 (root) |
| getrandom | 278 | Used by LibreSSL |
| socket/connect/send/recv | 198+ | Networking for package fetch |
| mmap/munmap | 222/215 | Memory mapping |
| brk | 214 | Heap management |

## Filesystem directories required

XBPS expects these directories to exist at runtime:

| Path | Purpose |
|------|---------|
| `/etc/xbps.d/` | User repository configuration (`*.conf` files) |
| `/usr/share/xbps.d/` | System default configuration |
| `/var/db/xbps/` | Package database (installed package metadata) |
| `/var/cache/xbps/` | Downloaded package cache |

These were added to `bootstrap/` so they're present on the disk image.

## Potential future issues

Syscalls that XBPS or its dependencies may call but are not yet implemented. These haven't been hit yet but could surface during package install/remove operations:

| Syscall | Number | Used by | Risk |
|---------|--------|---------|------|
| symlinkat | 36 | Package install (symlinks) | High — packages often contain symlinks |
| readlinkat | 78 | Package verification | Medium |
| fchmodat | 53 | Setting file permissions | Medium |
| fchownat | 54 | Setting file ownership | Low (single-user OS) |
| utimensat | 88 | Setting file timestamps | Low |
| linkat | 37 | Hard links | Low |
