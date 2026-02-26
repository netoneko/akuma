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

## Already implemented (used by XBPS)

| Syscall | Number | Notes |
|---------|--------|-------|
| openat | 56 | File I/O |
| close | 57 | File I/O |
| read | 63 | File I/O |
| write | 64 | File I/O |
| writev | 66 | Scatter-gather I/O |
| fstat | 80 | File metadata |
| newfstatat | 79 | File metadata with path |
| faccessat | 48 | File access checks |
| mkdirat | 34 | Directory creation |
| unlinkat | 35 | File/dir removal |
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
| mmap/munmap | 222/215 | Memory mapping |
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
| fchownat | 54 | Setting file ownership | Low (single-user OS) |
| linkat | 37 | Hard links | Low |
| pselect6 | 72 | Would be needed if `fetchTimeout` is set | Low — currently 0 |
