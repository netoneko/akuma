# VFS / filesystems

Current-state architecture for the VFS layer, file descriptors, ext2, procfs,
mount namespaces, and pipes/PTY.

> **Stability: A (stable).** VFS/ext2/procfs have been dormant since Mar 2026 —
> the lowest-churn subsystem. The `VFS_LOCK_OPTIMIZATION_PLAN.md` is planning,
> not a fire. Safe to trust; verify the SubdirFs box-root limitations if doing
> container rootfs isolation.

## VFS layer

`src/vfs/mod.rs` is the kernel glue: owns the global mount table
(`MOUNT_TABLE`), provides process-aware path resolution, re-exports types from
the `akuma_vfs` crate.

- **Mount table:** `Spinlock<Option<MountTable>>`. Global root is the ext2
  mount; boxes get a scoped namespace (see Namespaces below).
- **`with_fs`** is the VFS critical-section entry (disables preemption before
  the spinlock — the priority-inversion fix). **Never `yield_now()` or do slow
  I/O inside** (see [`scheduler.md`](scheduler.md)).
- **CWD** per-process; `canonicalize_path` / `resolve_path` handle relative
  paths. See `archive/CWD.md`.
- **`write_at` syscall** (`archive/WRITE_AT_SYSCALL.md`) — the streaming write
  optimization that avoids slurping into kernel heap.

## File descriptors

`FileDescriptor` enum (`crates/akuma-exec/src/process/types.rs:138`):

| Variant | Backing |
|---|---|
| `Stdin` / `Stdout` / `Stderr` | console |
| `File(KernelFile)` | a VFS file handle |
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

## ext2

`src/vfs/ext2.rs` bridges `akuma_ext2` to the kernel block device
(`KernelBlockDevice` → `src/block.rs` VirtIO block). Mount at boot.

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
