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

### renameat dirfd support (38)

**Symptom:** `WARNING: updating and opening ...: Operation not permitted` — APK downloaded and cached the APKINDEX but couldn't open it after renaming.

**Cause:** `sys_renameat` ignored both `olddirfd` and `newdirfd` arguments. APK opens the cache directory (`/var/cache/apk/`) as a file descriptor and passes it to `renameat`. With dirfds ignored, relative paths like `APKINDEX.f30b41c9.tar.gz.tmp.29` couldn't resolve, so the rename silently failed.

**Fix:** Added `resolve_path_at(dirfd, path)` helper to resolve relative paths against a dirfd's directory (or CWD for `AT_FDCWD`). Updated `sys_renameat` to use it for both old and new paths.

### symlinkat (36)

**Symptom:** `ERROR: musl-1.2.5-r21: failed to extract lib/libc.musl-aarch64.so.1: Function not implemented`

**Cause:** Alpine packages use symlinks extensively (e.g., `lib/libc.musl-aarch64.so.1 -> ld-musl-aarch64.so.1`). Akuma had no symlink support at all.

**Fix (v1 — in-memory, now superseded):** Added in-memory symlink support via a global `SYMLINKS` `BTreeMap`. This was a quick workaround: symlinks survived the session but not a reboot, `unlinkat` couldn't delete them, and directory listings didn't know about them.

**Fix (v2 — ext2 on-disk):** Replaced the in-memory approach with proper ext2 filesystem symlinks. See "ext2 symlink support" under Kernel Fixes below. The VFS layer still has a legacy in-memory fallback for filesystems that don't support symlinks (e.g., memfs, procfs).

Syscall integration:
- `src/syscall.rs`: `sys_symlinkat` creates entries; `sys_readlinkat` reads targets
- `sys_openat` calls `resolve_symlinks()` before checking file existence
- `sys_newfstatat` handles `AT_SYMLINK_NOFOLLOW` and returns `S_IFLNK` mode
- `sys_faccessat` follows symlinks when checking existence
- `sys_unlinkat` deletes ext2 symlink inodes via `remove_file` (handles fast symlinks correctly)

### readlinkat (78)

Implemented alongside `symlinkat`. Returns the symlink target string. Reads from ext2 on-disk symlinks first, falls back to the legacy in-memory table.

### linkat (37)

**Fix:** Simple implementation that copies the source file's content to the destination. Not a true hard link (no shared inode), but sufficient for APK's needs.

### mremap (216)

**Symptom:** ~80 consecutive `Unknown syscall: 216` calls during APKINDEX processing. APK handled the failure but wasted memory by allocating new regions instead of growing existing ones.

**Cause:** APK (via musl's `realloc`) uses `mremap` to resize mmap'd regions in-place. Without it, each failed `mremap` caused a fallback to `mmap` + `memcpy` + `munmap`, inflating mmap usage to 330+ MB.

**Fix:** Implemented `sys_mremap` in `src/syscall.rs`:
- If shrinking: returns the same address (no-op)
- If growing with `MREMAP_MAYMOVE`: allocates new region, copies data, frees old region
- If growing without `MREMAP_MAYMOVE`: returns `ENOMEM` (in-place growth not supported)

### fchdir (50)

**Symptom:** `Unknown syscall: 50` during trigger script execution. Child process exited with code 127 without ever reaching `execve`.

**Cause:** APK opens the package root directory as an fd, then calls `fchdir(fd)` to change CWD before executing the trigger script. Without `fchdir`, the CWD was wrong and subsequent operations (including execve) failed.

**Fix:** Implemented `sys_fchdir` in `src/syscall.rs`. Looks up the fd in the process's fd_table, extracts the directory path from the `FileDescriptor::File`, validates it's a directory, and calls `proc.set_cwd()`.

### fchmodat (53)

**Symptom:** `Unknown syscall: 53` during package extraction. APK reported errors when installing packages even with `--no-scripts`.

**Cause:** APK calls `fchmodat` to set file permissions (e.g., making binaries executable) after extracting package contents. Without it, APK treated the ENOSYS as an extraction error.

**Fix:** Stubbed as success (return 0) — Akuma doesn't enforce file permissions.

## Kernel Fixes (non-syscall)

### Close-on-exec (CLOEXEC) support

**Symptom:** APK's trigger script execution hung — the parent process blocked reading from a pipe, waiting for EOF that never arrived.

**Cause:** APK creates pipes with `O_CLOEXEC` (via `pipe2(fds, O_CLOEXEC)`) and expects them to be automatically closed when the child calls `execve`. Without CLOEXEC, the forked child inherited all of APK's pipe file descriptors. After `execve`, the child (now `/bin/sh`) still held write ends of pipes that the parent was reading from, so the parent never saw EOF and blocked forever.

**Fix:** Added per-process `cloexec_fds: BTreeSet<u32>` tracking to `Process` in `src/process.rs`:
- `pipe2`: honors `O_CLOEXEC` flag, marks both pipe FDs
- `openat`: honors `O_CLOEXEC` flag on opened files
- `socket`: honors `SOCK_CLOEXEC` (`0x80000`) flag
- `dup`: clears CLOEXEC on new fd (per POSIX)
- `dup3`: sets CLOEXEC on new fd only if `O_CLOEXEC` is in flags
- `close`: clears CLOEXEC tracking
- `fcntl`: `F_GETFD` returns `FD_CLOEXEC` if set; `F_SETFD` sets/clears it
- `execve`: closes all CLOEXEC-marked FDs (with proper pipe/socket cleanup) before loading the new image
- `fork`: clones the parent's `cloexec_fds` set into the child

### Shebang (`#!`) script support in execve

**Symptom:** `execve: replace_image failed for /lib/apk/exec/busybox-1.37.0-r30.post-install: Failed to load ELF` — APK's trigger scripts are shell scripts, not ELF binaries.

**Cause:** Akuma's `execve` only handled ELF binaries. APK's post-install and trigger scripts start with `#!/bin/sh` and need the kernel to detect the shebang, parse the interpreter, and exec the interpreter with the script path as an argument.

**Fix:** Refactored `sys_execve` into `sys_execve` (parses user-space args) + `do_execve` (performs the exec). `do_execve` checks the first two bytes of the file:
- If `#!`: calls `exec_shebang()` which parses the interpreter path and optional argument from the first line, resolves symlinks on the interpreter, builds new argv as `[interpreter, shebang_arg?, script_path, original_args[1:]...]`, and recursively calls `do_execve` with the interpreter binary
- Otherwise: loads as ELF (existing path)

Also resolves symlinks on the original execve path (was previously only done in `openat`).

### `/bin/sh` → `/bin/dash` fallback

**Cause:** Alpine packages use `#!/bin/sh` in scripts, but Akuma ships `dash` as `/bin/dash`. Without a symlink from `/bin/sh`, shebang resolution would fail.

**Fix:** Added built-in fallback in `resolve_symlinks()` (`src/vfs/mod.rs`): when resolving `/bin/sh` and no explicit symlink exists, checks if `/bin/dash` exists on disk and resolves to it. This works for shebangs, direct `execve("/bin/sh", ...)`, and any other path resolution.

### ext2 symlink support

**Symptom:** `apk del busybox` reported "1 error" — APK couldn't delete the symlinks it had created (e.g., `/bin/ls` → `/bin/busybox`) because they existed only in a volatile in-memory table, not on the ext2 filesystem. `unlinkat` called `fs::remove_file` which searched ext2 and found nothing.

**Cause:** The original symlink implementation (v1) stored all symlinks in a global in-memory `BTreeMap<String, String>`. This had several problems:
1. Symlinks were lost on reboot
2. `unlinkat` couldn't remove them (ext2 had no record of them)
3. `getdents64` / directory listings didn't show them
4. They consumed kernel heap indefinitely

**Fix:** Implemented proper ext2 symlink support in `src/vfs/ext2.rs`:

**On-disk format:**
- Symlink inodes use `type_perms = S_IFLNK | 0o777` (`0xA1FF`)
- Directory entries use `file_type = FT_SYMLINK` (7)
- **Fast symlinks** (target ≤ 60 bytes): target string stored directly in the inode's block pointer fields (`direct_blocks[12]` + `indirect_block` + `double_indirect_block` + `triple_indirect_block` = 60 bytes). No data blocks allocated; `sectors_used = 0`
- **Slow symlinks** (target > 60 bytes): target stored in data blocks via `write_inode_data`

**Changes:**
- `src/vfs/ext2.rs`:
  - Added constants: `S_IFLNK = 0xA000`, `FT_SYMLINK = 7`, `FAST_SYMLINK_MAX = 60`, `DEFAULT_SYMLINK_PERMS`
  - `create_symlink_internal()`: allocates inode, writes target (fast or slow), adds `FT_SYMLINK` dir entry
  - `read_symlink_inode()`: reads target from fast (block pointer bytes) or slow (data blocks) storage
  - `remove_file()`: detects fast symlinks and skips `truncate_inode` (block pointers contain target string, not block numbers)
  - Implemented `Filesystem` trait methods: `create_symlink()`, `read_symlink()`, `is_symlink()`
- `src/vfs/mod.rs`:
  - Added `create_symlink()`, `read_symlink()`, `is_symlink()` to the `Filesystem` trait (with default no-op implementations for memfs/procfs)
  - Rewired VFS-level `create_symlink()` to delegate to the mounted filesystem first, falling back to in-memory table only if the filesystem returns `NotSupported`
  - `read_symlink()` and `is_symlink()` check ext2 first, then in-memory table
  - `resolve_symlinks()` now uses the unified `read_symlink()` which checks ext2
- `src/syscall.rs`:
  - `sys_unlinkat`: simplified — ext2's `remove_file` now handles symlink deletion directly; in-memory table cleanup kept as legacy fallback

**Busybox example:** `busybox --install -s /bin` creates ~400 symlinks like `/bin/ls` → `/bin/busybox` (12 bytes, well within the 60-byte fast symlink limit). These are now proper ext2 directory entries with `FT_SYMLINK` type and persist across reboots.

### fork: physical address copy (stale TTBR0)

**Symptom:** `Sync from EL1: EC=0x25 ... WARNING: Kernel accessing user-space address!` during `clone(flags=0x11)` (fork).

**Cause:** `fork_process` in `src/process.rs` copied parent memory by reading directly from user-space virtual addresses (`va as *const u8`). With 330 MB of mmap regions, the copy took long enough for the scheduler to preempt the thread. After a context switch, TTBR0 pointed to a different process's page tables, and the next user VA read faulted.

**Fix:** Added `translate_user_va()` to `src/mmu.rs` — walks the page table from a saved L0 pointer to translate user VAs to physical addresses. `fork_process` now:
1. Snapshots the parent's L0 page table pointer at fork start
2. Translates each user VA to physical address via `translate_user_va()`
3. Copies through kernel identity-mapped memory (`phys_to_virt`)

This is safe across context switches since it doesn't depend on TTBR0.

### fork: skip mmap region copy (OOM)

**Symptom:** Same crash at FAR=0x8001000 (GIC MMIO region — not RAM) persisted after the TTBR0 fix.

**Cause:** APK had 332 MB of mmap regions but only 140 MB of free RAM. Attempting to duplicate all mmap pages during fork exhausted physical memory. The page allocator returned corrupted/zero frames, producing page table entries pointing to non-RAM physical addresses (0x8001000 = QEMU GIC region), causing bus errors.

**Fix:** Fork now skips mmap region copy entirely. Fork is almost always followed by `execve` (which replaces the entire address space), so the mmap data is never used by the child. The child starts with a fresh mmap allocator.

Also optimized the code range copy for PIE binaries: instead of scanning from 0x400000 to brk (scanning ~64K unmapped pages), starts from the entry point's 1 MB boundary.

## Already Implemented (used by APK)

Syscalls that were already in place (many from the XBPS work) and reused by APK:

| Syscall | Number | Notes |
|---------|--------|-------|
| openat | 56 | File I/O (with dirfd support + symlink resolution + CLOEXEC) |
| close | 57 | File I/O (clears CLOEXEC tracking) |
| read | 63 | File I/O |
| write | 64 | File I/O |
| readv | 65 | Scatter-gather read |
| writev | 66 | Scatter-gather write |
| fstat | 80 | File metadata |
| newfstatat | 79 | File metadata with path (symlink-aware) |
| faccessat | 48 | File access checks (symlink-aware) |
| getdents64 | 61 | Directory listing |
| lseek | 62 | File seek |
| mkdirat | 34 | Directory creation |
| unlinkat | 35 | File/dir/symlink removal (handles ext2 fast symlinks) |
| renameat | 38 | File rename (dirfd-aware) |
| getcwd | 17 | Current working directory |
| chdir | 49 | Change working directory |
| brk | 214 | Heap management |
| mmap/munmap | 222/215 | Memory mapping (anonymous + file-backed) |
| madvise | 233 | `MADV_DONTNEED` zeroes pages |
| mprotect | 226 | Memory protection (stubbed — no page protection) |
| dup | 23 | FD duplication (clears CLOEXEC on new fd) |
| dup3 | 24 | FD duplication (honors O_CLOEXEC flag) |
| fcntl | 25 | FD control (F_GETFD/F_SETFD with FD_CLOEXEC) |
| ioctl | 29 | Terminal control |
| flock | 32 | File locking (stubbed) |
| fchmod | 52 | File permissions (stubbed) |
| fchmodat | 53 | File permissions by path (stubbed) |
| fchownat | 54 | File ownership (stubbed) |
| ppoll | 73 | I/O multiplexing |
| clone | 220 | fork (optimized: skip mmap copy, clones CLOEXEC set) |
| execve | 221 | Execute program (ELF + shebang scripts, CLOEXEC cleanup) |
| wait4 | 260 | Wait for child process |
| pipe2 | 59 | Pipe creation (honors O_CLOEXEC) |
| socket | 198 | TCP and UDP creation (honors SOCK_CLOEXEC) |
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
| uname | 160 | System information |
| umask | 166 | File creation mask (stubbed) |

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
- Resolves DNS and connects to the Alpine CDN
- Downloads and verifies APKINDEX signatures (both `main` and `community`)
- Resolves package dependencies (e.g., `bash` pulls in `musl`, `readline`, `ncurses-libs`, `busybox`, `bash`)
- Downloads package `.apk` archives over HTTP
- Extracts package contents (files, directories, symlinks)
- Creates persistent ext2 symlinks (e.g., busybox creates ~400 symlinks in `/bin/`)
- Deletes packages and cleans up symlinks via `unlinkat`
- Sets file permissions via `fchmodat` (stubbed)
- Updates the APK database (`/lib/apk/db/installed`, `/etc/apk/world`)
- Installs simple packages (e.g., `neatvi`) successfully
- Forks child processes for trigger scripts (CLOEXEC ensures proper pipe cleanup)
- Executes shebang scripts (`#!/bin/sh` → resolves to `/bin/dash`)
- Changes working directory via `fchdir` before script execution

### Stale error recovery

If APK reports errors like `2 errors; 1762 KiB in 3 packages` from previous failed install attempts (before kernel fixes were applied), the errors are recorded in the APK database. To clear them:

```
rm /lib/apk/db/installed /lib/apk/db/scripts.tar.gz /lib/apk/db/triggers
```

Then re-run `apk add` to install cleanly with all fixes in place.

### Remaining issues
- Trigger scripts that depend on specific utilities may fail if those utilities aren't yet available
- `execve` may fail to copy argv from mmap'd regions in forked children (fork skips mmap copy to avoid OOM — argv strings allocated via mmap by the parent are inaccessible in the child)
- `getdents64` reports symlinks as `DT_REG` (regular file) instead of `DT_LNK` — the `DirEntry` struct lacks an `is_symlink` field. Most programs use `lstat` to check file type, so this is low-impact
- ext2 path traversal (`lookup_path_internal`) does not follow symlinks in intermediate path components (e.g., if `/usr/bin` were a symlink to `/bin`, lookups through `/usr/bin/foo` would fail). VFS-level `resolve_symlinks` only resolves the final component. This hasn't been an issue in practice since Alpine packages use flat symlinks

### mprotect (226)

**Symptom:** `[exit code: -11]` (SIGSEGV) — APK crashed with a data abort at `FAR=0x7d` (NULL pointer + struct offset write) during cleanup after `apk info -L`.

**Cause:** musl's `malloc` (mallocng) calls `mprotect(PROT_READ|PROT_WRITE)` when expanding the heap or reusing previously freed pages. Without `mprotect`, the syscall fell through to the unknown handler and returned `ENOSYS` (-38). musl interpreted this as a failed memory expansion and returned `NULL` from `malloc`. APK didn't check for NULL and dereferenced it at struct offset `0x7d` (125 bytes), causing a write fault at address `0x7d`.

**Fix:** Fully implemented. `sys_mprotect` walks the L0→L1→L2→L3 page tables via `update_page_flags()` and updates AArch64 permission bits (AP, UXN, PXN) for each page in `[addr, addr+len)`. The `user_flags::from_prot()` function maps Linux `PROT_*` flags to AArch64 page descriptors. TLB is flushed per page. Also required for dynamic linking (the dynamic linker uses `mprotect` to set correct permissions on library segments after loading).

## Dynamic Linking Support

APK can install dynamically linked packages (e.g., `curl`, `bash` with shared library dependencies). The kernel's ELF loader handles `PT_INTERP` segments by loading the dynamic linker (`ld-musl-aarch64.so.1`) at `0x3000_0000`, applying its relocations, and starting execution at the interpreter's entry point. The interpreter then loads shared libraries, resolves symbols, and jumps to the program's `AT_ENTRY`.

Prerequisites:
- `musl` package installed (`apk add musl`) — provides `/lib/ld-musl-aarch64.so.1`
- `mprotect` syscall (226) — interpreter changes page permissions after loading segments
- `mmap` with `MAP_FIXED` — interpreter places shared libraries at specific addresses
- `futex` (98), `prlimit64` (261), `sigaltstack` (132), `set_robust_list` (99) — stubs needed by the dynamic linker's initialization

See `userspace/musl/docs/DYNAMIC_LINKING.md` for full details.

## Potential Future Issues

| Syscall | Number | Used by | Risk |
|---------|--------|---------|------|
| ftruncate | 46 | Cache file management | Medium |
| statfs | 43 | Filesystem queries by path | Low — `fstatfs` covers the fd case |
