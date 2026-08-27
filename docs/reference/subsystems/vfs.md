# VFS / filesystems

Current-state architecture for the VFS layer, file descriptors, ext2, procfs,
mount namespaces, and pipes/PTY.

> **Stability: A (stable).** VFS/ext2/procfs were dormant from Mar 2026 until
> **2026-08-24 ("mount now works")**, which landed real `umount2`/`MS_REMOUNT`,
> multi-disk `mount(2)`, real `statfs`/`fstatfs`, and `/proc/mounts` +
> `/proc/filesystems` + `/etc/mtab` — see "Mount table" and "procfs" below.
> The `VFS_LOCK_OPTIMIZATION_PLAN.md` is planning, not a fire. Safe to trust;
> verify the SubdirFs box-root limitations if doing container rootfs isolation.

## VFS layer

`src/vfs/mod.rs` is the kernel glue: owns the global mount table
(`MOUNT_TABLE`), provides process-aware path resolution, re-exports types from
the `akuma_vfs` crate.

- **Mount table:** `Spinlock<Option<MountTable>>`. Global root is the ext2
  mount; boxes get a scoped namespace (see Namespaces below).
- **Every mount carries an id** (`ResolvedMount::id`, assigned by
  `MountSet::mount_with` from a counter private to `akuma-vfs`, never reused —
  including after unmount, and `replace_pristine_root` mints a fresh one). It is
  what makes an inode number mean something: an inode is only unique *within* a
  filesystem, and `mount(2)` can bring up a second ext2 whose numbers come from
  the same range. Anything storing an inode across time stores the pair —
  the file page cache's key, and `LazySource::File`. See
  [`../../archive/FPCACHE_MOUNT_IDENTITY.md`](../../archive/FPCACHE_MOUNT_IDENTITY.md).
  **Identity is assigned by the table, never reported by the filesystem**: a
  `Filesystem` that could declare its own id could claim another's cached pages,
  and those pages are mapped as executable text.
- **Resolution falls through to the global table.** `resolve_mount` tries the
  spawn override, then the process's namespace, then the global table — so a box
  whose namespace does not resolve a path reaches the host's mounts. A jailed box
  (`root_dir != "/"`) has a `SubdirFs` at `/`, and the `path == "/"` arm matches
  everything, so it never falls through; a box created with `root_dir == "/"`
  gets **no** namespace mount at all and therefore resolves everything globally.
- **`with_fs`** is the VFS critical-section entry (disables preemption before
  the spinlock — the priority-inversion fix). **Never `yield_now()` or do slow
  I/O inside** (see [`scheduler.md`](scheduler.md)).
- **CWD** per-process; `canonicalize_path` / `resolve_path` handle relative
  paths. See `archive/CWD.md`. Both are **single-allocation**: the returned
  `String` is the only thing allocated. `resolve_path` runs on every VFS
  operation (`resolve_mount` calls it before touching any mount) and used to
  cost three — a `format!` to join base and path, a scratch `Vec<&str>` of
  components, and the result. It now walks the two halves straight into the
  output buffer, handling `..` by truncating at the last `/`; feeding the
  halves in sequence is exactly canonicalizing their concatenation, so `..`
  still crosses the base boundary (`/a` + `../b` → `/b`) and still clamps at
  the root. `path_components` returns an iterator for the same reason —
  `akuma_ext2`'s `lookup_path_internal` walks it on every path resolution.
- **`write_at` syscall** (`archive/WRITE_AT_SYSCALL.md`) — the streaming write
  optimization that avoids slurping into kernel heap.
- **Remount / unmount** (`vfs/mod.rs:179,190`, `#[cfg(feature =
  "sc-containers")]`): `remount` flips the global table's stored `MS_RDONLY`
  bit; `unmount` refuses `/` (`FsError::PermissionDenied`) and otherwise drops
  the entry. Both operate on the global table only — a box's namespace mounts
  are composed from outside via `MOUNT_IN_NS` and torn down with the box, so
  neither of these can empty a box's namespace and trigger the `with_fs`
  global-fallback hazard below. Real since 2026-08-24 — before, `sys_umount2`
  returned `EPERM` unconditionally. Syscall boundary:
  [`syscalls/container.md`](syscalls/container.md) "mount / umount2".
- **`statfs`/`fstatfs`** (`vfs/mod.rs:660` `stats_for_path`, called from
  `syscall::fs`, not `syscall::container`): resolves a path the same way file
  operations do and returns the mount's real `FsStats` + `MS_RDONLY` as
  `ST_RDONLY`. Real since 2026-08-24 — before, `fstatfs` returned hardcoded
  numbers for every fd and `statfs` was undispatched. Detail:
  [`syscalls/fs.md`](syscalls/fs.md) "statfs / fstatfs".
- **`/proc/mounts` / `/proc/filesystems` / `/etc/mtab`** (added 2026-08-24):
  `render_mounts` (`vfs/mod.rs:687`) renders `/proc/mounts` rows
  allocation-free into a caller stack buffer while holding the mount locks,
  via `akuma_primitives::console::FmtBuf` — **not** a hand-rolled writer; the
  console-print rule (`console.md` "Printing rules") applies here too even
  though this isn't a console path, per the pre-commit hook's blanket
  `impl core::fmt::Write` grep. Mount set = the target process's namespace
  mounts, then any global mounts the namespace doesn't already shadow — the
  same set `with_fs` can actually resolve. **Box policy:** a boxed viewer sees
  *which* paths are mounted into it, never *where they came from* — the
  source column reports `none`; the host sees real sources. `/etc/mtab` is
  virtual too, intercepted before `with_fs` (`is_mtab`/`mtab_rows`,
  `vfs/mod.rs:628`) and rendered from the same live tables on every read —
  nothing is stored on disk, so it can never drift stale the way a real file
  would the moment a mount changes.
- **Multi-disk `mount(2)`** (added 2026-08-24): `crate::block` now tracks up to
  `MAX_BLOCK_DEVICES = 4` virtio-blk devices (`vda`..`vdd`,
  `crates/akuma-virtio/src/block.rs`), not just the boot disk. `mount(2)` with
  fstype `"ext2"` and a `source` device name calls
  `crate::vfs::ext2::mount_device(idx, Some(16 MiB))` — a fixed, small cache
  cap rather than the root's global budget, which never shrinks. See "ext2"
  below.

## File descriptors

`FileDescriptor` enum (`crates/akuma-exec/src/process/types.rs:138`):

| Variant | Backing |
|---|---|
| `Stdin` / `Stdout` / `Stderr` | console |
| `File(KernelFile)` | a VFS file handle: path, position, flags, **the inode `open(2)` resolved, and an `InodePin` on it** |
| `Socket(usize)` | smoltcp TcpSocket handle |
| `ChildStdout(Pid)` | a child process's stdout (used for PTY winsize routing) |
| `PipeRead(u32)` / `PipeWrite(u32)` | kernel pipe ends |
| `UnixSocket { rx, tx }` | AF_UNIX socketpair = two unidirectional kernel pipes (peer has rx/tx swapped) |
| `EventFd(u32)` | eventfd |
| `DevNull` / `DevUrandom` / `DevZero` | device nodes |
| `DevDsp` | virtio-sound PCM output (`/dev/dsp`) |
| `TapDevice` | `/dev/net/tap0` raw L2 frames (rump feature) |

**Shared FD tables:** `CLONE_FILES` shares the table across threads
(`archive/SHARED_FD_TABLES.md`). Across **fork** the table is copied (with
CLOEXEC stripping at exec). epoll fds are stripped on fork (not refcounted).

**Per-fd inode caching** (2026-08-27,
[`../../archive/EXT2_PER_FD_INODE_READ_PATH.md`](../../archive/EXT2_PER_FD_INODE_READ_PATH.md)):
`sys_openat` resolves the path to an inode once (`vfs::open_file_inode`) and
stores it on the `KernelFile`; `read`/`pread64` — and so `readv`/`preadv`/
`preadv2`, which route through them — then call `read_at_by_inode` instead of
re-running a full `lookup_path_internal` directory walk **per syscall**. Three
things follow:

- **An fd is bound to a file, not to a name.** It keeps reading what it opened
  after that name is unlinked or renamed over, which is what POSIX has always
  said and what path-based reads answered `ENOENT` to. The `InodePin` the fd
  holds is what makes reading by a reissuable number safe: ext2 defers the free
  of a pinned inode (`release_last_link`, and see `inode_pin`).
- **`inode == 0` means "read by path"**, and that is the path every filesystem
  without inode addressing takes — procfs, `MemoryFilesystem`, synthetic nodes —
  plus `/etc/mtab`, which must stay a resolve-time synthetic.
- **The mount is still selected by path.** `with_fs` resolves the fd's path to
  find the filesystem, then the inode number is applied to it, so a mount
  replaced under an open fd (or an fd used from a namespace that resolves its
  path elsewhere) aliases. Inherited from the mmap fill path, not introduced;
  the fix is an fd holding its `Arc<dyn Filesystem>`.
- **`fstat`, `statx(AT_EMPTY_PATH)` and `lseek(SEEK_END)` go by inode too**
  (`vfs::metadata_open_file` → `Filesystem::metadata_by_inode`), so they stay
  coherent with `read` on an unlinked-but-open fd. They did not, briefly:
  `fstat` answered `ENOENT` and `SEEK_END` answered `0` — silently reporting a
  live file as empty. Still path-based, and still failing on an unlinked fd:
  `ftruncate`, `fchmod`, `fallocate`, `flock`, and `newfstatat`'s dirfd (that
  one correctly — it is a base for a relative path, not a file identity).

## /dev

`/dev` is virtual, in the same shape `/etc/mtab` is: a resolve-time check ahead
of `with_fs` in `src/vfs/mod.rs`, **not** a mounted `Filesystem`. There is no
`DevFilesystem` and no `mknod` — the set is fixed at boot from what was actually
probed.

Two halves, deliberately kept apart:

| Concern | Where | Covers |
|---|---|---|
| **What exists** (`ls`, `stat`, `statx`, `access`, `chmod`) | `akuma_vfs::dev`, one table | `null`, `zero`, `random`, `urandom`, `dsp`/`audio`, `vda`..`vdd` |
| **What `open()` does** | `sys_openat`'s per-device blocks (`src/syscall/fs.rs`) | the same list, plus `vda`..`vdd` (raw block fd) and `/dev/net/tap0` |

`akuma_vfs::dev` (`crates/akuma-vfs/src/dev.rs`) is pure data: every entry is a
function of a `DevProbe` (`audio`, `block_slots`, `in_box`), so the whole table
is host-unit-tested with no boot. `src/vfs/mod.rs::dev_probe` is the only thing
that reads live state (`crate::audio::is_available`, `crate::block::device_name`,
`box_id`).

**Nothing on this path allocates.** All three tables (static, audio, block) are
`&'static [DevNode]`, `lookup` and `list` return iterators borrowing from them,
and `DevProbe` is a 3-byte `Copy` struct passed by value — so a `stat` on a
device path walks at most ten `&'static DevNode`s and allocates nothing at all.
The only `Vec` is the `DirEntry` listing `vfs::dev_entries` was going to build
anyway; "does `/dev` have any nodes?" is `list(probe).next().is_none()`.

`dev_node(path)` / `dev_node_named(name)` are the single lookup behind
every stat-family syscall — before 2026-08-25 `sys_newfstatat` and `sys_statx`
each hardcoded `/dev/null` and `/dev/zero` independently, so `/dev/random`,
`/dev/urandom` and `/dev/dsp` `open()`ed fine and `stat()`ed `ENOENT`
([`../../archive/DEVFS_MISSING.md`](../../archive/DEVFS_MISSING.md)).

Details that bite:

- **`open()` on `vda`..`vdd` returns a working raw block fd**
  (`FileDescriptor::BlockDev { idx, pos, writable }`,
  [`../../archive/RAW_BLOCK_DEVICE_FD.md`](../../archive/RAW_BLOCK_DEVICE_FD.md),
  implemented 2026-08-25) — `read`/`write`/`lseek`/`fstat` all work, backed by
  the block driver's byte-granular `read_bytes_at`/`write_bytes_at`
  (`crates/akuma-virtio/src/block.rs`), so no sector-alignment burden lands on
  the syscall layer. **Write-open of a *mounted* device is refused with
  `EBUSY`** (`crate::vfs::device_is_mounted`) — a raw write bypasses
  `Ext2Filesystem`'s cache, so this is what keeps a stray `dd` onto the
  mounted root (`vda`) from silently corrupting it. Reads are unrestricted on
  every device. `mount(2)` still resolves its `source` by *name*
  (`device_index_by_name` strips an optional `/dev/` prefix and never touches
  the filesystem or this fd machinery), so mounting a second disk needs none
  of this.
- **`/dev` itself is synthesized when the image has no real one**, otherwise
  `ls /dev` would fail at `open("/dev")` before reaching the listing.
- **A real on-disk node shadows a synthetic one**, matching how a mount point
  shadows an existing directory.
- **`/dev/net/tap0` is not in the table** — nested path, `open()`-only, rump
  feature. It is unaffected by all of the above.
- **`getdents64` asks the table for `d_type`** (`DT_CHR`/`DT_BLK`), because
  `DirEntry` carries only `is_dir`/`is_symlink` and would otherwise report
  `DT_REG`. The check is hoisted: non-`/dev` listings pay one string compare.

### Boxes get no synthetic /dev

A scope decision made 2026-08-25 to keep this simple, not a limitation
discovered. `DevProbe::in_box` (set when `box_id != 0`) empties the table, so a
box sees only whatever its own rootfs holds — the host's disks and sound card
never appear in a box's `ls /dev`. Two carve-outs:

- `null` and `zero` still answer `stat` in a box, because they did before the
  table existed and turning `stat("/dev/null")` into `ENOENT` inside a box would
  be a regression. They are still absent from the *listing* — the asymmetry
  preserves the old behavior rather than inventing new behavior for boxes.
- **`/dev/net/tap0` keeps working**, since it was never in the table and
  `sys_openat` is untouched. A `stack = rump` box's `rump_server` opens it to
  drive the NetBSD stack ([`rump-stack.md`](rump-stack.md)), so this is the one
  device a box genuinely needs.

Expand later if something asks for it; nothing does today.

## ext2

`src/vfs/ext2.rs` bridges `akuma_ext2` to the kernel block devices
(`KernelBlockDevice { idx }` → `crate::block::{read,write}_bytes_at` — VirtIO
block, `crates/akuma-virtio/src/block.rs`). Boot mounts device 0 (`vda`) at
`/`; `mount_device(idx, cache_cap)` (added 2026-08-24) mounts any other
registered device (`crate::block::device_index_by_name`), giving a runtime
data disk its own `Ext2Filesystem` instance with a small fixed cache rather
than sharing or re-committing the root's global cache budget. See "Mount
table" above and [`syscalls/container.md`](syscalls/container.md) "mount /
umount2".

- **`first_data_block`:** off-by-one fix (`archive/EXT2_FIRST_DATA_BLOCK_FIX.md`).
- **`getdents64`:** directory cache fix (`archive/GETDENTS64_DIR_CACHE_FIX.md`).
- **`fs-cache` feature** (`Cargo.toml`): large ext2 block cache (clock
  eviction) — keeps the read-only toolchain resident across the many process
  spawns in a self-host `cargo build`. Opt-in; not combinable with `extreme`.
  Gives ~19× metadata speedup. See `archive/AKUMA_SELF_HOSTING.md` §7c.
- **Write path:** ext2 is read-write; `write_at` avoids heap slurp.

## procfs

`src/vfs/proc.rs`. Entries:

| Path | Content |
|---|---|
| `/proc/<pid>/stat` | Linux compact single-line format (what `ps`/`top` parse) |
| `/proc/<pid>/status` | human-readable; includes `Uid`/`Gid` (always 0) and `CapInh`/`CapPrm`/`CapEff`/`CapBnd`/`CapAmb` |
| `/proc/<pid>/cmdline` | argv |
| `/proc/<pid>/fd/` | fd listing + `/proc/<pid>/fd/<n>` symlinks |
| `/proc/self/…` | resolves to the calling process's pid (see below) |
| `/proc/cores` | static single-row table (`0 online bsp`) |
| `/proc/boxes` | box listing |
| `/proc/mounts` | the caller's mount set (its namespace if boxed, the global table on the host) — `crate::vfs::render_mounts`, added 2026-08-24 |
| `/proc/filesystems` | supported fstypes, one per line: `ext2`/`proc`/`tmpfs`, plus `overlay` under `sc-containers` — added 2026-08-24 |
| `/proc/meminfo` | `MemTotal`/`MemFree`/`MemAvailable`/`Cached`/`Swap*`, real numbers off `pmm::stats()` + `file_page_cache::len()` — added 2026-08-25, what busybox `free` reads |
| `/proc/stat` | `cpu`/`cpuN` lines + `processes`/`procs_running`/`procs_blocked` — added 2026-08-25, what busybox `top` reads (unconditionally; missing this file makes `top` refuse to start at all) |
| `/proc/uptime` | `uptime_seconds idle_seconds`, Linux SMP semantics (idle summed across cores) — added 2026-08-25 |
| `/proc/loadavg` | `runnable/total` and `last_pid` are real; the three load-average figures are always `0.00` — no decaying run-queue average is tracked — added 2026-08-25 |
| `/etc/mtab` | not procfs, but the same idea and the same renderer: a virtual file, intercepted in `src/vfs/mod.rs` before any real filesystem is touched — see "Mount table" above |

**`/proc/stat`'s `cpu`/`cpuN` lines and `/proc/<pid>/stat`'s `utime` field are
real**, off the same per-thread microsecond counter the custom `/bin/top`
binary's CORE column already reads via `sys_get_cpu_stats`
(`akuma_exec::threading::get_thread_cpu_time`; bucketed by core in `proc.rs`'s
`cpu_time_snapshot`). There is no user/kernel-time split tracked, so all busy
time lands in `utime`/the `user` field; `idle` is derived as
`wall_time_per_core - busy_time`, which is enough for busybox `top`'s %CPU
(computed from two reads' deltas) to move correctly. Before 2026-08-25 neither
file existed at all: busybox `free` errored `can't open '/proc/meminfo'` and
busybox `top` errored `can't open 'stat'` — not a parsing bug, the files were
simply missing.

> `ps`/`top` parse `/proc/<pid>/stat` (compact), not `status`. The `stat` file
> was added after `ps` showed nothing (`archive/PROCFS.md`).

**`/proc/self` is rewritten, not chased.** The VFS hands procfs the literal
path rather than resolving the `self` symlink first, so `resolve_self`
(`src/vfs/proc.rs`) rewrites a leading `self/` to the caller's pid at the top of
`read_dir`/`read_at`/`read_file`/`exists`/`metadata`. The bare string `self`
is deliberately left alone so `readlink("/proc/self")` keeps working.

Before 2026-08-16 this did not happen at all: `read_symlink` reported `self`
correctly, but `open("/proc/self/status")` arrived as the string `self/status`
and matched nothing — `cat /proc/self/status` said `No such file or directory`
in box 0 and in containers alike, for every file that existed under
`/proc/<pid>/`. It is the reason `redis-server` could not start for four days
(`/proc/self/smaps`, `archive/LONG_ROAD_TO_REDIS.md`) and, later, the reason
libcap-ng failed inside a container (`archive/REDIS_END_TO_END.md` §4). If you
add a `/proc` entry, test it through `/proc/self/` too — the two paths take
different code.

**Adding a virtual file means touching four functions, not one.** `read_file`
renders the content, but `metadata`, `list_dir` and — separately — `read_at`
each keep their own idea of which paths exist, and `read_at` is the one that
serves the actual `read()`. `/proc/cores` was in three of the four until
2026-08-20: `open()` and `stat()` succeeded, `ls /proc` listed it, and every
`read()` returned `NotFound`. busybox renders that as `read error: No such file
or directory` rather than `can't open`, which is the only tell that the file was
found and the *read* was what failed
([`../../archive/DEVBOX_ISSUES.md`](../../archive/DEVBOX_ISSUES.md) Issue 4).
`read_at`'s whitelist must name every path `read_file` renders; the boot-suite
check is `test_procfs_virtual_files_are_readable` (`src/process_tests.rs`),
which drives `openat` + `read` rather than calling `read_file` — testing the
renderer directly is what kept the gap invisible.

The `Cap*` lines report a full-root set (`000001ffffffffff`). Nothing enforces
capabilities; the lines exist because **libcap-ng reads a process's
capabilities from `/proc/self/status`**, not from `capget(2)`, and returns -1
without setting errno when it cannot — which surfaces as the useless
`setpriv: activate capabilities: No error information`.

## Mount namespaces & box isolation

`BOX_NAMESPACES: Spinlock<BTreeMap<u64, Arc<Namespace>>>` (`vfs/mod.rs`). Each
box can get a `Namespace` (from the `akuma_isolation` crate). If a box's
`root_dir` is non-"/", a `SubdirFs` scoped to that dir mounts at `/` in the box's
namespace.

- **`SPAWN_NS_OVERRIDE`** — per-thread namespace override for ELF loading during
  spawn (`set_spawn_namespace`/`clear_spawn_namespace`).
- **SubdirFs limitations:** `archive/BOX_SUBDIR_FS_LIMITATIONS.md` — the fresh-
  root isolation isn't a full chroot; some paths leak.

## Pipes, PTY, TTY

- **Pipes** (`src/syscall/pipe.rs`): `PipeRead`/`PipeWrite` backed by kernel
  buffers. SIGPIPE delivery to the writer can spin on `cmd | head`
  (`../../runbooks/debug-devbox.md` — open wedge mode).
- **PTY** (`SPAWN_FLAG_PTY`): child gets a fresh `TerminalState` Arc (see
  [`ssh.md`](ssh.md) "Terminal handling"). `TIOCSWINSZ` reaches the child via
  `ChildStdout(pid)`.
- **TTY processing:** `archive/PIPE_TTY_FIX.md`.
- **`stat`/`unlinkat`/`dup3`** + kernel pipes fix: `archive/STAT_AND_UNLINKAT_FIX.md`.

## Background

- `archive/PROCFS.md`, `archive/NAMESPACES.md`, `archive/CWD.md`.
- `archive/EXT2_FIRST_DATA_BLOCK_FIX.md`, `archive/GETDENTS64_DIR_CACHE_FIX.md`.
- `archive/VFS_LOCK_OPTIMIZATION_PLAN.md` (planning, not a fire).
- `archive/BOX_SUBDIR_FS_LIMITATIONS.md`, `archive/STAT_AND_UNLINKAT_FIX.md`,
  `archive/PIPE_TTY_FIX.md`, `archive/WRITE_AT_SYSCALL.md`.
- `archive/MOUNT_MISSING_SYSCALLS.md` — the pre-2026-08-24 audit of everything
  under "Mount table" above: what was missing (`umount2`, `MS_REMOUNT`, real
  `statfs`, `/proc/mounts`, multi-disk) and why, before it was built.
