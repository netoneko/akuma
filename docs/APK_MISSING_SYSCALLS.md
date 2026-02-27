# APK (Alpine Package Manager) — Missing Syscalls & Fixes

Syscalls and kernel changes needed to get Alpine's `apk` package manager running on Akuma.

APK's static binary (`apk-tools-static`) is a **static-PIE** executable (`ET_DYN`), which required ELF loader and memory layout changes in addition to syscall work.

## ELF Loader Changes

### Static-PIE (ET_DYN) support

**Symptom:** `Error: Failed to load ELF: Not an executable`

**Cause:** Alpine's `apk.static` binary is compiled as a static-PIE executable. Its ELF header has `e_type = ET_DYN` (shared object), not `ET_EXEC`. Akuma's ELF loader rejected anything that wasn't `ET_EXEC`.

**Fix:** Modified `src/elf_loader.rs` to accept `ET_DYN` binaries. PIE binaries are loaded at `PIE_BASE = 0x1000_0000` — segment virtual addresses, entry point, and PHDR auxiliary vector entries are all offset by this base. Kernel-side relocations (`SHT_RELA`) are skipped for PIE binaries since musl's `__dls2` self-relocates at startup.

### mmap region overlap with PIE code

**Symptom:** `Instruction abort from EL0 at FAR=0x100186e8, ISS=0xf`

**Cause:** PIE binary loaded at `0x1000_0000`, but `ProcessMemory` hardcoded `mmap_start = 0x1000_0000`. When musl called `mmap` for TLS allocation, it overwrote loaded code pages (RX) with data pages (RW), causing an instruction abort.

**Fix:** Changed `src/process.rs` to calculate `mmap_start` dynamically as `(code_end + 0x1000_0000) & !0xFFFF`, placing the mmap region 256 MB after code. This also fixed XBPS, which had crashed due to a 1 MB gap being too small for its ~2 MB heap allocations.

See `userspace/apk-tools/docs/PIE_LOADER.md` for the full memory layout diagram.

## Implemented Syscalls

### pselect6 (72)

**Symptom:** `Unknown syscall: 72` repeated in a tight loop during HTTP fetch. APK hung after connecting to the repository server.

**Cause:** APK uses `pselect6` to wait for TCP socket writability after `connect()`. Without it, APK spun on the failed syscall and never proceeded to send the HTTP request.

**Fix:** Implemented `sys_pselect6` in `src/syscall.rs`. The implementation:
- Saves copies of input `readfds`/`writefds` bitmasks before entering the poll loop
- Each iteration checks socket readiness using the same infrastructure as `ppoll` (smoltcp for TCP/UDP, channel for stdin)
- On ready: writes back only ready fd bits, returns the count
- On timeout: zeros the output sets, returns 0
- Supports up to 1024 file descriptors (`FD_SETSIZE`)

Also added `pselect6` to the syscall debug noise filter alongside `ppoll`.

### dup (23)

**Symptom:** `Unknown syscall: 23` with args `[0x10, ...]` immediately before the "UNTRUSTED signature" warning.

**Cause:** APK calls `dup(fd)` to duplicate file descriptors during I/O setup for signature verification. Without it, APK couldn't set up its internal I/O properly and fell back to reporting the signature as untrusted.

**Fix:** Implemented `sys_dup` in `src/syscall.rs`. Unlike `dup3` (which targets a specific fd number), `dup` allocates the lowest available fd via `proc.alloc_fd()`. Pipe reference counts are properly incremented for cloned pipe fds.

### fstatfs (44)

**Symptom:** `Unknown syscall: 44` early in APK startup. APK then reported `Operation not permitted` when trying to open the cached APKINDEX.

**Cause:** APK calls `fstatfs` to query filesystem properties (type, block size, free space). When it got `ENOSYS`, APK treated downstream operations as forbidden.

**Fix:** Implemented `sys_fstatfs` in `src/syscall.rs`. Returns a `struct statfs` populated with ext2-appropriate values:

| Field | Value | Notes |
|-------|-------|-------|
| `f_type` | `0xEF53` | `EXT2_SUPER_MAGIC` |
| `f_bsize` | 4096 | Block size |
| `f_blocks` | 65536 | Total blocks |
| `f_bfree` | 32768 | Free blocks |
| `f_bavail` | 32768 | Available blocks |
| `f_files` | 16384 | Total inodes |
| `f_ffree` | 8192 | Free inodes |
| `f_namelen` | 255 | Max filename length |
| `f_frsize` | 4096 | Fragment size |

## Already Implemented (used by APK)

Syscalls that were already in place (many from the XBPS work) and reused by APK:

| Syscall | Number | Notes |
|---------|--------|-------|
| openat | 56 | File I/O (with dirfd support) |
| close | 57 | File I/O |
| read | 63 | File I/O |
| write | 64 | File I/O |
| readv | 65 | Scatter-gather read |
| writev | 66 | Scatter-gather write |
| fstat | 80 | File metadata |
| newfstatat | 79 | File metadata with path |
| faccessat | 48 | File access checks |
| getdents64 | 61 | Directory listing |
| lseek | 62 | File seek |
| mkdirat | 34 | Directory creation |
| unlinkat | 35 | File/dir removal |
| renameat | 38 | File rename (used to atomically place cached APKINDEX) |
| getcwd | 17 | Current working directory |
| brk | 214 | Heap management |
| mmap/munmap | 222/215 | Memory mapping (anonymous + file-backed) |
| madvise | 233 | `MADV_DONTNEED` zeroes pages |
| dup3 | 24 | File descriptor duplication |
| fcntl | 25 | File descriptor control |
| ioctl | 29 | Terminal control |
| flock | 32 | File locking (stubbed) |
| ppoll | 73 | I/O multiplexing |
| socket | 198 | TCP and UDP creation |
| bind | 200 | Socket address binding |
| connect | 203 | TCP connection, UDP peer association |
| sendto | 206 | UDP send |
| recvfrom | 207 | UDP receive |
| sendmsg | 211 | Socket message send (DNS) |
| recvmsg | 212 | Socket message receive (DNS) |
| getsockname | 204 | Local socket address |
| setsockopt | 208 | Socket options (stubbed) |
| shutdown | 210 | Socket shutdown |
| getrandom | 278 | Crypto RNG |
| getuid/geteuid/getgid/getegid | 174–177 | All return 0 (root) |
| getpid | 172 | Process ID |
| rt_sigprocmask | 135 | Signal mask |
| rt_sigaction | 134 | Signal handlers |
| set_tid_address | 96 | Thread setup |
| clock_gettime | 113 | Time queries |
| exit/exit_group | 93/94 | Process exit |

## APK-Specific Configuration

### Bootstrap files

APK requires these files/directories on the disk image (created by `userspace/apk-tools/build.rs`):

| Path | Purpose |
|------|---------|
| `/bin/apk` | APK binary (renamed from `apk.static`) |
| `/etc/apk/repositories` | Repository URLs (main + community) |
| `/etc/apk/arch` | Architecture (`aarch64`) |
| `/etc/apk/world` | Explicitly installed packages (empty initially) |
| `/etc/apk/keys/*.rsa.pub` | Signing keys for signature verification |
| `/lib/apk/db/installed` | Installed package database (empty initially) |
| `/lib/apk/db/triggers` | Trigger tracking (empty initially) |

### Repository URLs

```
http://dl-cdn.alpinelinux.org/alpine/latest-stable/main
http://dl-cdn.alpinelinux.org/alpine/latest-stable/community
```

### Signing keys

The `alpine-keys-2.6-r0` package provides the RSA public keys. For aarch64, the relevant keys are:
- `alpine-devel@lists.alpinelinux.org-58199dcc.rsa.pub`
- `alpine-devel@lists.alpinelinux.org-616ae350.rsa.pub` (4096-bit RSA, signs current `latest-stable` APKINDEX)

### IPv6

APK attempts to create IPv6 sockets (`domain=10, AF_INET6`) which Akuma doesn't support. APK handles this gracefully and falls back to IPv4.

## Current Status

APK successfully:
- Resolves DNS and connects to the Alpine repository
- Downloads and caches APKINDEX files (both `main` and `community`)
- Calls `dup` and `fstatfs` without errors

Remaining issue: APK reports "Operation not permitted" when trying to open the cached APKINDEX. This may require additional syscalls (signature verification path) or filesystem fixes. Investigation ongoing.

## Potential Future Issues

| Syscall | Number | Used by | Risk |
|---------|--------|---------|------|
| symlinkat | 36 | Package install (symlinks) | High — Alpine packages use symlinks heavily |
| readlinkat | 78 | Package verification, symlink handling | High |
| ftruncate | 46 | Cache file management | Medium |
| fchmodat | 53 | File permissions during install | Medium |
| linkat | 37 | Hard links | Low |
| statfs | 43 | Filesystem queries by path | Low — `fstatfs` covers the fd case |
| mprotect | 226 | Memory protection changes | Low — may be needed by crypto |
