# fs syscalls

open/read/write/stat/getdents/unlink/rename/... — the largest syscall family
(`src/syscall/fs.rs`, 2340 lines). For the VFS layer, `FileDescriptor`
variants, ext2, procfs, mount namespaces, and pipes/PTY, see
[`../vfs.md`](../vfs.md) — this doc covers only the syscall entry-point
layer: which syscalls this file implements, argument/flag validation,
path-resolution entry points, and quirks specific to the syscall boundary.
Do not expect ext2/procfs/namespace mechanics here; they're linked, not
repeated.

> **Stability: A (stable).** Inherits `vfs.md`'s grade — this file's own
> commit history is dominated by *other* subsystems being bootstrapped
> against it (Go, rustc, multikernel, rump) rather than fs-syscall bugs
> themselves; the actual VFS/ext2/procfs layer underneath has been dormant
> since March 2026. The recurring lesson: **most "missing" POSIX semantics
> here are silent gaps, not errors** — `O_EXCL`, real hardlinks, and
> `faccessat2`'s access-mode bits are all accepted and silently ignored
> rather than rejected or partially honored; don't assume an error-free
> return means full POSIX behavior.

## Syscall table

| Syscall | nr | Entry point |
|---|---|---|
| `getcwd` | 17 | `sys_getcwd` |
| `mkdirat` | 34 | `sys_mkdirat` |
| `unlinkat` | 35 | `sys_unlinkat` |
| `symlinkat` | 36 | `sys_symlinkat` |
| `linkat` | 37 | `sys_linkat` |
| `renameat` | 38 | `sys_renameat` |
| `truncate` | 45 | `sys_truncate` |
| `ftruncate` | 46 | `sys_ftruncate` |
| `fallocate` | 47 | `sys_fallocate` |
| `faccessat` | 48 | `sys_faccessat2` (mode arg dropped) |
| `chdir` | 49 | `sys_chdir` |
| `fchdir` | 50 | `sys_fchdir` |
| `fchmod` | 52 | `sys_fchmod` |
| `fchmodat` | 53 | `sys_fchmodat` |
| `openat` | 56 | `sys_openat` |
| `close` | 57 | `sys_close` |
| `getdents64` | 61 | `sys_getdents64` |
| `lseek` | 62 | `sys_lseek` |
| `read` | 63 | `sys_read` |
| `write` | 64 | `sys_write` |
| `readv` | 65 | `sys_readv` |
| `writev` | 66 | `sys_writev` |
| `pread64` | 67 | `sys_pread64` |
| `pwrite64` | 68 | `sys_pwrite64` |
| `preadv` | 69 | `sys_pvec2` |
| `pwritev` | 70 | `sys_pvec2` |
| `preadv2` | 286 | `sys_pvec2` |
| `pwritev2` | 287 | `sys_pvec2` |
| `readlinkat` | 78 | `sys_readlinkat` |
| `newfstatat` | 79 | `sys_newfstatat` |
| `fstat` | 80 | `sys_fstat` |
| `dup` | 23 | `sys_dup` |
| `dup3` | 24 | `sys_dup3` |
| `fcntl` | 25 | `sys_fcntl` |
| `statfs` | 43 | `sys_statfs` |
| `fstatfs` | 44 | `sys_fstatfs` |
| `renameat2` | 276 | `sys_renameat2` |
| `statx` | 291 | `sys_statx` |
| `close_range` | 436 | `sys_close_range` |
| `faccessat2` | 439 | `sys_faccessat2` |

All always-on — `fs.rs` has no `sc-*` gate (see
[`../syscalls.md`](../syscalls.md) "The `src/syscall/` split").

## Path resolution entry point

Every `*at` syscall (`openat`, `mkdirat`, `unlinkat`, `renameat[2]`,
`symlinkat`, `linkat`, `readlinkat`, `fchmodat`, `newfstatat`, `faccessat2`)
shares the same `dirfd` convention, implemented independently in each
function (a shared `resolve_path_at` helper exists at `fs.rs:56` and is used
by the newer call sites — `renameat`/`renameat2`/`symlinkat`/`linkat`; older
ones like `openat`/`mkdirat`/`unlinkat`/`statx`/`faccessat2` inline the same
logic):

- Absolute path (`starts_with('/')`) → `canonicalize_path`, dirfd ignored.
- `dirfd == AT_FDCWD (-100)` → resolve relative to `proc.cwd`.
- `dirfd >= 0` → must resolve to an open `FileDescriptor::File`; anything
  else (a pipe, a socket, an absent fd) → `EBADF`.
- Any other negative `dirfd` → treated as `/` (not an error) in most of
  these functions — a syscall-boundary looseness worth knowing if porting a
  binary that passes a deliberately-invalid negative dirfd expecting `EBADF`.

Symlink resolution (`crate::vfs::resolve_symlinks`) is applied to the
resolved path in `openat`/`fchmodat`/`statx`/`faccessat2` before the
filesystem is touched — see `../vfs.md` for how symlinks are stored; this
file only calls into it.

## `openat` flag semantics

`sys_openat` (`fs.rs:1090`) handles device nodes (`/dev/null`, `/dev/zero`,
`/dev/urandom`, `/dev/random`, `/dev/dsp`/`/dev/audio` when
`audio::is_available()`, `/dev/net/tap0` when the `rump` feature's tap is
ready) and `/proc/self/exe` before ever touching the real filesystem — see
`../vfs.md` "procfs" for what those resolve to.

Only `open()` behavior lives here. **What a device path *is*** — whether it
exists, and what `stat` reports — comes from one table (`akuma_vfs::dev`, via
`crate::vfs::dev_node`); see `../vfs.md` "/dev", including the raw block fd
`open()` on `/dev/vda`..`vdd` now returns (write-open refused `EBUSY` on a
mounted device) and why a box sees no synthetic `/dev`.

For a real path:
- File doesn't exist and `O_CREAT` not set → `ENOENT`.
- File doesn't exist, `O_CREAT` set, but the **parent directory** doesn't
  exist either → `ENOENT` (checked explicitly via `split_path` before
  creation, so a dangling multi-level path doesn't attempt a
  create-into-nowhere).
- File doesn't exist, `O_CREAT` set → created via `write_file(path, &[])`;
  if `mode & 0o7777 != 0`, `chmod`'d to that mode.
- File exists and `O_TRUNC` set → truncated to zero via the same
  `write_file(path, &[])`.
- **`O_EXCL` is not implemented at all** — there is no `open_flags::O_EXCL`
  constant and no check for it anywhere in `fs.rs`. `O_CREAT|O_EXCL` on an
  already-existing file silently succeeds and (if `O_TRUNC` is also set,
  which many `O_EXCL` callers don't set) may truncate it, instead of
  returning `EEXIST`. Anything relying on `O_EXCL` for exclusive-create
  locking semantics will not get them here.
- `O_CLOEXEC` is honored (`proc.set_cloexec`); `O_APPEND` is honored at
  `read`/`write` time (see below), not at open time.
- **The fd is bound to an inode here, once** (`vfs::open_file_inode`), together
  with an `InodePin` on it, so `read`/`pread64` need no directory walk of their
  own — see `read`/`pread64` below.

## `read` / `pread64` — bound to an inode, not to a name

Since 2026-08-27 (`../../../archive/EXT2_PER_FD_INODE_READ_PATH.md`) a `File`
fd carries the inode `open(2)` resolved, and `read`/`pread64` serve it via
`read_at_by_inode` rather than re-resolving `KernelFile::path` on every call.
`readv`, `preadv` and `preadv2` route through those two, so they inherit it.

Consequences at this boundary:

- **Unlinked-but-open works.** `read` on an fd whose name has been removed, or
  renamed over, returns the file the fd opened. It used to return `ENOENT` (for
  the unlink) or the *new* file's bytes (for the rename) — both wrong per POSIX.
- **`EISDIR` is unchanged** for a `read` on a directory fd: `read_at_by_inode`
  refuses `S_IFDIR` exactly where `read_at` does.
- **`fstat`/`lseek(SEEK_END)` did not follow** — they still resolve `f.path`, so
  on an unlinked-but-open fd they fail while `read` succeeds. Not a regression
  (both used to fail); it needs a `Filesystem::metadata_by_inode`.
- **An fd whose filesystem has no inode addressing keeps reading by path**
  (`inode == 0`): procfs, `MemoryFilesystem`, synthetic nodes, and `/etc/mtab`.

## `write`/`pwrite64` and `O_APPEND`

`sys_write` resolves the write position **once**, before the chunking loop:
if `O_APPEND` is set, it re-reads `crate::fs::file_size` on every call (so a
concurrent writer's growth is respected — true POSIX append semantics, not
just "whatever position was cached at open"); otherwise it uses the fd's
tracked `position`. Each 64 KiB chunk is written via `vfs::write_at` (the
streaming optimization documented in `../vfs.md` "VFS layer" — linked, not
re-derived here) and a **short write from any chunk stops the loop and
returns the partial total**, not an error — this is legal POSIX short-write
behavior but means every caller of a large `write()` must already handle
partial completion. `pwrite64`/`pread64` reject a negative `offset` with
`EINVAL` up front (the `File` arm is a single un-chunked `read_at`/
`write_at` call, no APPEND handling — POSIX `pwrite` ignores `O_APPEND` by
design).

## `readv`/`writev` — a short transfer ends the vector

Both loop `sys_read`/`sys_write` over the iovecs and **stop at the first one
that transfers fewer bytes than it was given**, returning the running total. A
zero-length iovec is skipped, not treated as short.

This is not an optimization, it is the contract. A partial transfer means the
tail of that iovec did not happen; continuing to the next one writes it directly
after the truncated bytes, while the caller — which learns only the total —
resumes from a point that never corresponds to what actually went out. The
stream ends up with a hole in it, and on a socket that is silent corruption of
someone else's protocol.

It matters constantly rather than rarely, because short writes are the *normal*
case here: `socket_send` returns whatever fit in smoltcp's 16 KB TX buffer, and
`alloc_net_bounce` degrades to a single page under memory pressure. Worse,
`socket_send` ends with a `poll()` that drains the TX buffer, so the next iovec
usually *succeeds* — which turns a dropped tail into a splice rather than a
harmless stall.

`writev` lacked the guard until 2026-08-16 while `readv` always had it; every
Redis reply larger than the TX window came out spliced. Full A/B (4/16 KiB
clean, 64 KiB / 256 KiB / 1 MiB corrupt, first wrong byte `0x0d` — the `\r\n` of
the next iovec):
[`../../../archive/WRITEV_SHORT_WRITE_SPLICE.md`](../../../archive/WRITEV_SHORT_WRITE_SPLICE.md).
The rule is a named predicate (`writev_stops_after`) with a boot-suite check,
`run_writev_short_write_tests`.

## `preadv`/`pwritev`/`preadv2`/`pwritev2` — all four, or none

All four go to one entry point, `sys_pvec2`, which walks the iovecs with
`sys_pread64`/`sys_pwrite64` and advances the offset by what was actually
transferred. The same short-transfer rule as above applies, for the same reason.

**All four have to exist for the family to work**, because musl decides which
one to issue and only reaches the `2` variants when `flags` is nonzero:

| what the caller asked for | what musl actually issues |
|---|---|
| `flags == 0`, `offset == -1` | `writev` / `readv` (66 / 65) |
| `flags == 0`, `offset >= 0` | `pwritev` / `preadv` (70 / 69) |
| `flags != 0` | `pwritev2` / `preadv2` (287 / 286) |

Implementing only 286/287 therefore leaves the common path falling through to
the dispatcher's `-ENOSYS` catch-all, **which prints a line per attempt**. That
was the `[ENOSYS] nr=287` console flood: the writes still succeeded through a
caller-side fallback, but each one paid a wasted syscall plus a console print,
and the print is the expensive half under load
([`../../../archive/DEVBOX_ISSUES.md`](../../../archive/DEVBOX_ISSUES.md)
Issue 13).

Two details worth not re-deriving:

- **`pos_h` (arg 4) carries nothing on a 64-bit kernel.** Linux reassembles the
  offset with `pos_from_hilo(pos_h, pos_l)`, whose two 32-bit shifts of a 64-bit
  value make the high word contribute zero; `pos_l` already holds the whole
  offset. `sys_pvec2` does not take it as a parameter, deliberately — folding it
  in would break every offset above 4 GB.
- **Unsupported `RWF_*` flags return `EOPNOTSUPP`, not `EINVAL`.** None of
  `HIPRI`/`DSYNC`/`SYNC`/`NOWAIT`/`APPEND` are implemented. `EINVAL` reads as
  "bad argument" and stops a caller from retrying without the flag; `EOPNOTSUPP`
  is Linux's own answer and invites the retry.

Boot-suite check: `test_pvec2` (`src/process_tests.rs`), whose load-bearing case
is the two-iovec positional write — an offset that fails to advance between
iovecs makes the second overwrite the first.

**`sendmsg`/`recvmsg` are a different story** and still only process `iovs[0]` —
see [`net.md`](net.md). That is a legal short transfer rather than a splice, but
it is a real gap versus POSIX.

## `getdents64` directory cache

`sys_getdents64` (`fs.rs:2235`) reads the whole directory listing once via
`crate::fs::list_dir` and caches it on the fd (`f.dir_cache`) as a flat
`Vec<DirCacheEntry>`; subsequent calls on the same fd paginate through that
cached snapshot by `f.position` (an entry index, not a byte offset — same
field `KernelFile::position` that a regular file uses for its byte offset).
**Syscall-boundary consequence:** a directory modified between two
`getdents64` calls on the same fd is invisible until the cache is
invalidated — which only happens via `lseek(fd, 0, SEEK_SET)` (`fs.rs:1419`
clears `dir_cache` on seek-to-zero) or closing and reopening the fd. See
`../vfs.md` "ext2" `GETDENTS64_DIR_CACHE_FIX.md` for why this caching exists
at all (a directory-cache bug, not a design from scratch).

## `statfs` / `fstatfs` — real mount stats since 2026-08-24

Before 2026-08-24, `fstatfs` returned hardcoded fiction (`f_type=0xEF53`,
65536 blocks) for every fd and `statfs` was undispatched entirely — every
filesystem sized identically to `df`-style tools
(`docs/archive/MOUNT_MISSING_SYSCALLS.md` §3.2). Both now resolve the target
the way file operations do (`crate::vfs::stats_for_path`, which goes through
the same spawn-override → namespace → global-table order as every other path
lookup) and report the real mount's `FsStats` plus its `MS_RDONLY` bit as
`ST_RDONLY`.

`sys_fstatfs` (`fs.rs:1420`) resolves the fd to its `File`'s path first —
non-file fds (pipes, sockets) have no path, so it reports the root mount
instead, matching what nothing in-tree actually branches on. `sys_statfs`
(`fs.rs`, next to it) resolves a user-supplied path directly. Both funnel
through the same `statfs_into` writer, which fills the standard 120-byte
`struct statfs` layout and maps `Filesystem::name()` to a `statfs` magic
number (`fs_magic`: `ext2`→`0xEF53`, `proc`→`0x9FA0`, `memfs`/`tmpfs`→
`0x01021994`, `overlay`/`subdirfs`→`0x794C7630`, else `0xADF5`).

**This code lives in `fs.rs`, not `container.rs`, on purpose.** `STATFS`/
`FSTATFS` dispatch unconditionally in `syscall/mod.rs` — they are ordinary
POSIX syscalls, not container-specific — but `container` is `#[cfg(feature =
"sc-containers")]`-gated and absent from the `extreme-size` build. The
functions were briefly implemented in `container.rs` during the same change
and had to move once `extreme-size` clippy caught the missing-module
compile error; `mount`/`umount2`/`mount_in_ns` and everything else box-shaped
stay in `container.rs`, correctly gated.

## Other syscall-boundary gaps worth knowing

- **`linkat` is not a real hardlink.** `sys_linkat` (`fs.rs:2177`) implements
  a "hardlink" by `read_file` + `write_file` — a full content copy into a
  new inode. There is no shared-inode/`nlink` semantics; the two paths are
  independent files after the call, not the same file under two names. Any
  caller that hardlinks then mutates one path expecting the other to see it
  will not get that behavior.
- **`faccessat2`/`faccessat` ignore the `mode` argument entirely.** Both
  dispatch to `sys_faccessat2`, which only checks existence
  (`crate::fs::exists` or `is_symlink`) → `0` or `ENOENT`. `R_OK`/`W_OK`/
  `X_OK` bits and `AT_EACCESS` are accepted but never inspected — a caller
  probing for write permission on a read-only mount gets a false "yes".
- **`fcntl`'s advisory locks are no-op stubs.** `F_GETLK`/`F_SETLK`/
  `F_SETLKW` all return `0` (success) unconditionally — there is no lock
  state, so two processes "locking" the same file never actually contend.
  `F_DUPFD`/`F_DUPFD_CLOEXEC`/`F_GETFD`/`F_SETFD`/`F_GETFL`/`F_SETFL` are
  fully implemented (cloexec/nonblock bits + fd duplication with pipe
  refcount bumping); any other `cmd` → `EINVAL` (logged as `UNSUPPORTED`).
- **`lseek` error selection is fd-type-aware.** A bad fd → `EBADF`; a real
  seekable `File` with a resulting negative offset or unknown `whence` →
  `EINVAL`; a valid-but-non-seekable fd (pipe, socket, tty, eventfd, ...) →
  `ESPIPE` — this three-way split matters because musl/Rust std probe
  seekability of stdio by calling `lseek(fd, 0, SEEK_CUR)` and branch on
  `ESPIPE` specifically.
- **`renameat2`'s `RENAME_NOREPLACE`/`RENAME_EXCHANGE` are only partially
  real:** `RENAME_NOREPLACE` does check-then-`EEXIST` if the destination
  exists; both flags together → `EINVAL`; but there is no actual atomic
  exchange implementation for `RENAME_EXCHANGE` — it falls through to a
  plain `crate::fs::rename`, which is a rename, not a swap.
- **`unlinkat`'s `AT_REMOVEDIR`** routes to `crate::fs::remove_dir` instead
  of `remove_file`; without the flag, a plain `unlinkat` also always calls
  `crate::vfs::remove_symlink` first (harmless no-op if the target isn't a
  symlink) before `remove_file`.
- **`statx`** takes device stat data from `crate::vfs::dev_node` (one table,
  shared with `newfstatat`; it does not go through `crate::vfs::metadata`
  because `Metadata` carries no `rdev`) and supports `AT_EMPTY_PATH` (empty
  path + valid `dirfd` → stat the fd itself) and `AT_SYMLINK_NOFOLLOW`; only
  `STATX_BASIC_STATS` fields are ever populated regardless of the requested
  `mask`. Until 2026-08-25 it synthesized `/dev/null`/`/dev/zero` inline, as a
  second independent copy of `newfstatat`'s — which is why every *other* device
  `stat`ed `ENOENT` (`../../archive/DEVFS_MISSING.md`).

## Background

- `archive/GETDENTS64_DIR_CACHE_FIX.md` — why `getdents64` caches (linked
  from `../vfs.md` too; the fix is in `sys_getdents64` itself).
- `archive/STAT_AND_UNLINKAT_FIX.md` — `stat`/`unlinkat`/`dup3` + kernel
  pipes interaction fix.
- `archive/RUST_TOOLCHAIN.md` §4 — the CLOEXEC-pipe-read `EBADF` symptom
  that `READ_EBADF_TRACE` (`fs.rs:20`) exists to localize.
- `archive/WRITE_AT_SYSCALL.md` — the streaming write path `sys_write`/
  `sys_pwrite64` call into (full detail in `../vfs.md`, not here).
- `archive/MOUNT_MISSING_SYSCALLS.md` §3.2 — the `statfs`/`fstatfs` audit
  behind the real-stats fix above; §3.11 behind `mount_errno`'s errno mapping
  (see [`container.md`](container.md) "mount / umount2").
