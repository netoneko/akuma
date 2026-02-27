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

### flock (32)

**Symptom:** `xbps-install` fails with:
```
ERROR: failed to lock file: /var/db/xbps/lock: Function not implemented
```

**Cause:** `xbps_pkgdb_lock()` in `lib/pkgdb.c:92` calls `flock(fd, LOCK_EX|LOCK_NB)` to exclusively lock the package database. Akuma had no handler for syscall 32.

**Fix:** Stubbed as no-op returning 0 (success). Akuma is single-user with no concurrent package manager instances, so locking is unnecessary.

### umask (166)

**Symptom:** `[syscall] Unknown syscall: 166` in kernel log during `xbps-install`.

**Cause:** XBPS calls `umask()` to set default file permission masks before creating files during package installation. Without it, newly created files could get overly permissive modes.

**Fix:** Stubbed to always return `0o022` (the standard default mask) and ignore the argument. Akuma doesn't enforce file permissions, so the actual mask value is irrelevant.

### UDP socket support (SOCK_DGRAM)

**Symptom:** `Unknown resolver error` when running `xbps-install -S`.

**Cause:** Musl's DNS resolver uses UDP sockets (`SOCK_DGRAM`) to query nameservers. Akuma only supported TCP (`SOCK_STREAM`). Without UDP, DNS resolution failed silently and XBPS couldn't resolve repository hostnames.

**Fix:** Full UDP socket support added across multiple files:
- `src/socket.rs` — Added `Datagram` variant to `SocketType`, `socket_send_udp`, `socket_recv_udp`
- `src/smoltcp_net.rs` — Added `udp_socket_create`, `udp_socket_bind`, `udp_socket_send`, `udp_socket_recv`
- `src/syscall.rs` — Updated `sys_socket` to accept `SOCK_DGRAM`, updated `sys_sendto`/`sys_recvfrom` for UDP, updated `sys_ppoll` for UDP readiness

Also required `/etc/resolv.conf` with `nameserver 10.0.2.3` (QEMU's DNS forwarder).

### sendmsg (211) / recvmsg (212)

**Symptom:** DNS resolution failed even after UDP support was added.

**Cause:** Musl's `res_msend.c` uses `recvmsg()` to receive DNS responses (to extract source address metadata via `cmsg`). Without it, the resolver loop never completed.

**Fix:** Implemented `sys_sendmsg` and `sys_recvmsg` to process `msghdr` and `iovec` structs, routing data through the existing socket send/recv paths.

### getsockname (204) / getpeername (205)

**Symptom:** `[syscall] Unknown syscall: 204` during DNS resolution.

**Cause:** Musl's resolver calls `getsockname()` to determine the local address of a connected UDP socket (used to detect which interface/route was selected).

**Fix:** Implemented `sys_getsockname` to return the local IP/port. `sys_getpeername` stubbed to return 0.

### setsockopt (208) / getsockopt (209)

**Symptom:** LibreSSL (used by XBPS for HTTPS) called these during TLS setup.

**Cause:** LibreSSL sets socket options like `TCP_NODELAY` and `SO_KEEPALIVE` during connection setup.

**Fix:** Stubbed to return 0. The options aren't critical for correctness.

### utimensat (88)

**Symptom:** `[syscall] Unknown syscall: 88` after successfully downloading repodata.

**Cause:** After downloading a file, XBPS sets the file's modification time to match the server's `Last-Modified` header via `utimensat()`.

**Fix:** Stubbed to return 0. File timestamps are not critical for XBPS operation.

### fdatasync (83) / fsync (82)

**Symptom:** `[syscall] Unknown syscall: 83` after importing a repository public key.

**Cause:** XBPS calls `fdatasync()` to flush the public key file to disk after writing it to `/var/db/xbps/keys/`.

**Fix:** Stubbed to return 0. Akuma uses a single block device with no write-back cache, so data is effectively written immediately.

### fchmod (52)

**Symptom:** `[syscall] Unknown syscall: 52` immediately after `fdatasync`, still during key import.

**Cause:** After writing the public key file, XBPS calls `fchmod(fd, 0644)` to set restrictive permissions on it.

**Fix:** Stubbed to return 0. Akuma doesn't enforce file permissions.

### madvise (233)

**Symptom:** `[syscall] Unknown syscall: 233` during RSA signature verification, causing `the RSA signature is not valid!`.

**Cause:** Musl's `mallocng` allocator calls `madvise(addr, len, MADV_DONTNEED)` to release freed pages. On Linux, `MADV_DONTNEED` discards page contents so future accesses fault in zeroed pages. Without it, the allocator reuses pages expecting them to be zeroed, but stale data remains, corrupting LibreSSL's internal state during RSA operations.

**Fix:** Implemented `sys_madvise`. For `MADV_DONTNEED` (advice=4), zeroes the specified page-aligned memory range. All other advice values are accepted and ignored.

### readv (65)

**Symptom:** RSA signature verification failed with `the RSA signature is not valid!` — no unknown syscalls logged.

**Cause:** Musl's `__stdio_read` (the backend for `fread()`) calls `readv` to fill both the user buffer and its internal buffer in one syscall. Without `readv`, every `fread()` returned 0 bytes. XBPS uses `fread()` to read the `.sig2` signature file, so the signature data was empty and `RSA_verify` failed.

**Fix:** Implemented `sys_readv` mirroring `sys_writev` — iterates over the iovec array, calling `sys_read` for each entry, stopping on short reads or errors.

### mmap file-backed mappings (222)

**Symptom:** RSA signature verification failed — public key plist file opened and fstat'd but never read.

**Cause:** XBPS's proplib library uses `mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0)` to memory-map plist files (including the repository public key). Akuma's `sys_mmap` only accepted 4 of the 6 Linux mmap arguments — `fd` and `offset` were ignored. All mappings returned zeroed pages regardless of the file descriptor, so proplib parsed zeros and got no valid public key.

**Fix:** Extended `sys_mmap` to accept all 6 arguments. When `MAP_ANONYMOUS` (0x20) is NOT set and a valid file descriptor is provided, the file content is read into the mapped pages after allocation.

### openat dirfd support + path canonicalization (56)

**Symptom:** `failed to extract file './usr/bin/busybox.static': Operation not permitted` during package unpack.

**Cause (dirfd):** `sys_openat` ignored the `dirfd` parameter entirely, resolving all relative paths against CWD. Libarchive (used by XBPS for tar extraction) opens parent directories with `O_PATH|O_DIRECTORY` and then creates files relative to those fds. Without dirfd support, `openat(fd_for_usr, "bin")` opened `/bin` instead of `/usr/bin`, causing extraction to target wrong paths.

**Cause (canonicalization):** Paths containing `.` or `..` components (e.g. `./usr/bin/busybox.static`) were not normalized. The old code constructed `/./usr/bin/busybox.static` with a raw `format!()`, which the VFS could not resolve, causing parent-directory checks to fail with ENOENT (reported as "Operation not permitted").

**Fix:** `sys_openat` now resolves relative paths using the directory path from the `dirfd` file descriptor. If `dirfd` is `AT_FDCWD` (-100), resolves relative to process CWD. All constructed paths are routed through `vfs::resolve_path` / `vfs::canonicalize_path` to normalize `.` and `..` components. Failed opens are logged for debugging.

### fchownat (54)

**Symptom:** `[SC] nr=54` in catch-all syscall trace during package extraction.

**Cause:** Libarchive calls `fchownat()` to set file ownership when extracting archives as root (`getuid() == 0`), because `ARCHIVE_EXTRACT_OWNER` is automatically enabled.

**Fix:** Stubbed to return 0. Akuma is a single-user OS with no ownership enforcement.

### unlinkat proper error codes (35)

**Symptom 1:** `failed to extract file './usr/bin/busybox.static': Operation not permitted` during package unpack (after dirfd fix).

**Cause:** Libarchive uses `ARCHIVE_EXTRACT_UNLINK` mode, which calls `unlinkat()` to remove existing files before creating new ones. When the target file does not exist (normal case for first install), libarchive expects `ENOENT` (-2) to mean "nothing to remove, proceed with creation." But `sys_unlinkat` returned `!0u64` (-1 = `EPERM`) on any failure, which libarchive treated as a fatal permission error. Additionally, paths with `./` prefixes were not canonicalized.

**Symptom 2:** `busybox-static-1.34.1_12: failed to remove './usr/bin': Operation not permitted` during `xbps-remove`. The directory was still in use by other packages, so the removal correctly failed — but with the wrong error code.

**Cause:** During package removal, XBPS calls `unlinkat(AT_REMOVEDIR)` on each directory that was part of the package. For shared directories like `/usr/bin`, ext2's `remove_dir` correctly returned `FsError::DirectoryNotEmpty`, but `sys_unlinkat` discarded the error variant and returned `!0u64` (-1 = `EPERM`). XBPS treats `EPERM` as a real error and prints an ERROR line, whereas `ENOTEMPTY` is silently ignored since it's expected for shared directories.

**Fix:** `sys_unlinkat` now properly maps `FsError` variants to Linux errno values via a `fs_error_to_errno` helper:

| FsError | errno | Value |
|---------|-------|-------|
| `NotFound` | `ENOENT` | 2 |
| `PermissionDenied` | `EPERM` | 1 |
| `AlreadyExists` | `EEXIST` | 17 |
| `NotADirectory` | `ENOTDIR` | 20 |
| `NotAFile` | `EISDIR` | 21 |
| `DirectoryNotEmpty` | `ENOTEMPTY` | 39 |
| `NoSpace` | `ENOSPC` | 28 |
| `ReadOnly` | `EROFS` | 30 |
| `InvalidPath` | `EINVAL` | 22 |

Paths are also canonicalized via `vfs::resolve_path`.

## Already implemented (used by XBPS)

| Syscall | Number | Notes |
|---------|--------|-------|
| openat | 56 | File I/O (with dirfd support + path canonicalization) |
| close | 57 | File I/O |
| read | 63 | File I/O |
| readv | 65 | Scatter-gather read (used by musl fread) |
| write | 64 | File I/O |
| writev | 66 | Scatter-gather I/O |
| fstat | 80 | File metadata |
| newfstatat | 79 | File metadata with path |
| faccessat | 48 | File access checks |
| mkdirat | 34 | Directory creation |
| unlinkat | 35 | File/dir removal (proper errno mapping: ENOENT, ENOTEMPTY, etc.) |
| getdents64 | 61 | Directory listing |
| getcwd | 17 | Current working directory |
| chdir | 49 | Change directory |
| getuid/geteuid/getgid/getegid | 174-177 | All return 0 (root) |
| getrandom | 278 | Used by LibreSSL |
| socket | 198 | TCP and UDP socket creation |
| bind | 200 | Socket address binding |
| connect | 203 | TCP connection, UDP peer association |
| sendto | 206 | UDP datagram send |
| recvfrom | 207 | UDP datagram receive |
| sendmsg | 211 | Socket message send (DNS) |
| recvmsg | 212 | Socket message receive (DNS) |
| getsockname | 204 | Local socket address query |
| setsockopt | 208 | Socket options (stubbed) |
| getsockopt | 209 | Socket options (stubbed) |
| ppoll | 73 | I/O multiplexing |
| fdatasync | 83 | Flush file data (stubbed) |
| fsync | 82 | Flush file data (stubbed) |
| fchmod | 52 | File permissions (stubbed) |
| fchownat | 54 | File ownership (stubbed) |
| mmap/munmap | 222/215 | Memory mapping (anonymous + file-backed) |
| madvise | 233 | Memory advisory (MADV_DONTNEED zeroes pages) |
| brk | 214 | Heap management |
| dup3 | 24 | File descriptor duplication |
| fcntl | 25 | File descriptor control |
| ioctl | 29 | Terminal control |

## Configuration notes

### Repository URL
The correct Void Linux aarch64-musl repository is at:
```
repository=http://repo-default.voidlinux.org/current/aarch64
architecture=aarch64-musl
```
**Not** `/current/musl/` — that directory only has armv6l, armv7l, and x86_64 musl builds. The aarch64 musl packages live under `/current/aarch64/` with architecture `aarch64-musl`.

### Environment variables
Processes need `SSL_NO_VERIFY_PEER=1` and `SSL_NO_VERIFY_HOSTNAME=1` because the HTTP repo redirects to HTTPS and Akuma lacks CA certificates. These are set as default env vars in `spawn_process_with_channel_ext` when no env is provided.

### DNS
`/etc/resolv.conf` must contain `nameserver 10.0.2.3` (QEMU user-mode networking DNS forwarder).

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
| linkat | 37 | Hard links | Low |
| pselect6 | 72 | Would be needed if `fetchTimeout` is set | Low — currently 0 |
