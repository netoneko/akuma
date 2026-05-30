# Git (Alpine apk) -- Missing Syscalls and Fixes

Alpine's `git` (via `apk add git`) required twelve kernel fixes before
`git clone https://...` worked end-to-end. This document records the
root causes and fixes applied.

## Issue 1: No `/dev/null` Device

### Symptoms

```
[syscall] write(fd=2, count=85) "fatal: could not open '/dev/null"
[exception] Process 18 (/usr/bin/git) exited, calling return_to_kernel(128)
```

Git opens `/dev/null` very early in startup (for redirecting unwanted output
and as a safe fallback fd). When `openat("/dev/null", ...)` returned `ENOENT`,
git printed the fatal error and exited with code 128.

### Root Cause

Akuma had no `/dev` filesystem or virtual device support. The VFS only
mounted ext2 at `/` and procfs at `/proc`. Any access to `/dev/null`,
`/dev/zero`, `/dev/urandom`, etc. failed with `ENOENT`.

### Fix: Virtual `/dev/null` Device

Rather than implementing a full devfs, `/dev/null` is handled as a special
`FileDescriptor` variant (`DevNull`) in the process fd table. This avoids
filesystem overhead and matches how the kernel already handles pipes and
eventfds.

**Files:** `src/process.rs`, `src/syscall.rs`

### Changes

**1. New `DevNull` variant in `FileDescriptor` enum**

Added `DevNull` to the `FileDescriptor` enum in `src/process.rs`. It carries
no data -- there is no underlying file, buffer, or state to track.

**2. `sys_openat` intercepts `/dev/null`**

After path resolution and symlink following, if the final path equals
`/dev/null`, a `DevNull` fd is allocated instead of going through the VFS.
`O_CLOEXEC` is respected.

**3. `sys_read` returns EOF**

Reading from a `DevNull` fd returns 0 (EOF), matching Linux behavior.

**4. `sys_write` discards data**

Writing to a `DevNull` fd returns `count` (success), discarding all data.

**5. `sys_fstat` and `sys_newfstatat` return char device metadata**

Both stat syscalls return a `Stat` struct with:
- `st_mode = 0o20666` (S_IFCHR | 0666 -- character device, world-readable/writable)
- `st_rdev = makedev(1, 3)` (major 1, minor 3 -- Linux's `/dev/null` device numbers)
- `st_size = 0`

A `makedev(major, minor)` helper was added for constructing device numbers.

The `sys_fstat` function was also restructured from a single `if let` into a
`match` to handle `DevNull`, stdin/stdout/stderr, and pipe fds with
appropriate stat metadata.

**6. `sys_lseek` returns 0**

Seeking on `/dev/null` returns 0 (success), matching Linux behavior.

**7. No changes needed for `close`, `dup`, `dup3`, `fcntl`, `ioctl`**

- `close`: The `_ => {}` wildcard in the close cleanup already handles `DevNull` as a no-op.
- `dup`/`dup3`: Clone the `DevNull` variant via the existing `Clone` derive.
- `fcntl`: Operates on generic per-fd flags (cloexec, nonblock), no fd-type dispatch.
- `ioctl`: Returns `ENOTTY` for fd > 2, correct for `/dev/null`.

## Issue 2: `mkdirat` Returns Wrong Errno

### Symptoms

```
git clone https://github.com/netoneko/meow
Cloning into 'meow'...
/meow/.git/: Operation not permitted
[exit code: 1]
```

Kernel log showed two `mkdirat` calls for the `.git` directory:

```
[syscall] mkdirat: /meow/.git
[syscall] mkdirat: /meow/.git/
[syscall] write(fd=2, count=11) "/meow/.git/"
[syscall] write(fd=2, count=23) "Operation not permitted"
```

### Root Cause

`sys_mkdirat` had two bugs:

1. **Wrong errno for all errors.** It returned `!0u64` (`-1` = `-EPERM`) for
   every failure, regardless of the actual filesystem error. When git called
   `mkdir("/meow/.git/")` (trailing slash) after `/meow/.git` already existed,
   ext2 returned `AlreadyExists` but the syscall returned `EPERM` instead of
   `EEXIST`. Git handles `EEXIST` gracefully (the directory is already there)
   but treats `EPERM` as a fatal permissions error.

2. **Ignored `dirfd` parameter.** The `dirfd` argument was prefixed with `_`
   and never used. Relative paths were always resolved against the process CWD
   instead of the directory referenced by `dirfd`. This could cause wrong
   directory creation when programs use `mkdirat(fd, "subdir", mode)`.

### Fix

**File:** `src/syscall.rs`

Rewrote `sys_mkdirat` to match the pattern used by `sys_unlinkat`:

- **Proper path resolution:** If the path is absolute, canonicalize it
  directly. If relative, resolve it against `dirfd` (or CWD when
  `dirfd == AT_FDCWD`). Handles `AT_FDCWD` (-100) and real directory fds.

- **Proper errno return:** Uses `fs_error_to_errno()` to map filesystem
  errors to Linux errno values (`AlreadyExists` → `EEXIST`,
  `NotFound` → `ENOENT`, `NoSpace` → `ENOSPC`, etc.) instead of
  returning `-EPERM` for everything.

## Issue 3: `chmod` / `stat` Permissions Not Working

### Symptoms

```
ls -al /usr/libexec/git-core/git-remote-http
-rw-r--r--    1 0        0            800784 Jan 01  1970 /usr/libexec/git-core/git-remote-http
```

All files showed `644` (`-rw-r--r--`) permissions regardless of actual ext2
inode permissions. `chmod +x` appeared to succeed (returned 0) but had no
effect — the permission bits were never read from or written to the ext2 inode.

### Root Cause

Three bugs combined:

1. **`st_mode` hardcoded in stat syscalls.** Both `sys_fstat` and
   `sys_newfstatat` returned `0o100644` for all files and `0o40755` for all
   directories, ignoring the actual `type_perms` field stored in the ext2
   inode.

2. **`fchmod`/`fchmodat` were no-op stubs.** Both syscalls returned 0 without
   modifying anything. Programs like `chmod` and `install` thought they
   succeeded, but the inode was never updated.

3. **VFS `Metadata` lacked a `mode` field.** The struct only had `is_dir`,
   `size`, `inode`, and timestamps — no way to propagate the actual
   permissions from the filesystem to the syscall layer.

### Fix

**Files:** `src/vfs/mod.rs`, `src/vfs/ext2.rs`, `src/vfs/memory.rs`,
`src/vfs/proc.rs`, `src/syscall.rs`

1. **Added `mode: u32` to `Metadata`** — carries the full file type +
   permission bits (e.g., `0o100755` for an executable file).

2. **ext2 returns actual inode permissions** — `metadata()` now reads
   `inode.type_perms` and stores it in `meta.mode`. memfs and procfs return
   appropriate defaults (`0o40555` for procfs dirs, etc.).

3. **stat syscalls use `meta.mode`** — `sys_fstat` and `sys_newfstatat` use
   `meta.mode` directly instead of hardcoded values. Also added timestamps
   (`st_atime`, `st_mtime`, `st_ctime`) from metadata.

4. **Added `chmod()` to `Filesystem` trait** — default returns `NotSupported`,
   ext2 implementation updates `inode.type_perms` preserving the file type
   nibble and writing the updated inode to disk.

5. **Implemented `sys_fchmod` and `sys_fchmodat`** — both resolve the path
   and call `vfs::chmod()`. `fchmodat` handles `AT_FDCWD` and real directory
   fds for proper relative path resolution.

## Issue 4: `O_CREAT` Did Not Create File on Disk

### Symptoms

```
error: chmod on /meow/.git/config.lock failed: No such file or directory
fatal: could not set 'core.filemode' to 'true'
```

### Root Cause

`sys_openat` with `O_CREAT` (without `O_TRUNC`) only allocated a file
descriptor but never created the file on the ext2 filesystem. The file was
created lazily on first `write()` (ext2's `write_at` auto-creates missing
files). Git opens lockfiles with `O_CREAT | O_WRONLY | O_EXCL` (no `O_TRUNC`)
and calls `fchmod(fd, mode)` before writing any data. Since the file didn't
exist on disk yet, `fchmod` → `vfs::chmod` → ext2 `lookup_path` returned
`NotFound`.

### Fix

**File:** `src/syscall.rs`

Changed `sys_openat` to create the file on disk immediately when `O_CREAT` is
set and the file doesn't exist, rather than waiting for the first write. The
`O_TRUNC` path was also adjusted to only truncate when the file already
exists (previously it could create-then-truncate redundantly).

## Issue 5: `clone3` Syscall Not Implemented

### Symptoms

```
fatal: unable to find remote helper for 'https'
```

Kernel log showed pipes being created and immediately destroyed with no
`[syscall] clone(...)` or `[Process] Spawning ...` output, even though
`SYSCALL_DEBUG_INFO_ENABLED` was on. Git was unable to fork a child process
to run `git-remote-https`.

### Root Cause

Musl libc on Alpine (>= 1.2.4) uses `clone3` (syscall 435) in
`posix_spawn` before falling back to `clone` (syscall 220). The kernel had
no handler for syscall 435, so it returned `ENOSYS`. Depending on musl
version, the fallback to `clone` may not work correctly for all
`posix_spawn` use cases, causing the spawn to fail silently.

### Fix

**File:** `src/syscall.rs`

Added `clone3` support:

- New syscall constant `CLONE3 = 435` and dispatch entry.
- `sys_clone3` reads the `clone_args` struct from userspace, extracts
  `flags`, `exit_signal`, `stack`, and `stack_size`, then delegates to
  the existing `sys_clone` implementation.
- The `clone_args` struct follows the Linux ABI: flags and exit_signal are
  combined (clone3 separates them, clone combines them in the low bits),
  and stack is passed as base + size (clone3 uses base/size, clone uses
  top-of-stack).

## Issue 6: `O_CREAT` Ignoring `mode` Parameter (File Permissions Lost)

### Symptoms

```
fatal: unable to find remote helper for 'https'
```

Syscall tracing revealed that git stat'd `/usr/bin/git` and got
`mode=0o100644` (no execute bit). Git's `is_executable()` check requires
`S_IXUSR` and returned false. Without finding itself in PATH, git could
not derive `GIT_EXEC_PATH` and could not locate `git-remote-https` in
`/usr/libexec/git-core/`. Git gave up without even attempting fork/exec.

### Root Cause

`sys_openat` with `O_CREAT` created new files via `write_file(&path, &[])`
which always gives ext2's default permissions (`0644`). The `mode`
parameter from the syscall (e.g., `0755` for executables) was completely
ignored (named `_mode`). When `apk` extracted packages, executables were
created as `0644` instead of `0755`.

### Fix

**File:** `src/syscall.rs`

Changed `sys_openat` to apply the `mode` parameter after creating a new
file with `O_CREAT`:

```rust
if !file_existed && (flags & O_CREAT != 0) {
    let _ = crate::fs::write_file(&path, &[]);
    if mode & 0o7777 != 0 {
        let _ = crate::vfs::chmod(&path, mode & 0o7777);
    }
}
```

This ensures newly created files get the permissions specified by the
caller (e.g., `0755` for executables), matching Linux behavior.

**Note:** Requires re-populating the disk image so that packages are
extracted with the fix in place.

## Issue 7: Fork Doesn't Copy Dynamic Linker Pages

### Symptoms

After fixing permissions (issue 6) and clone3 (issue 5), git successfully
calls `clone(flags=0x11)` to fork a child process. The child immediately
crashes:

```
[Fault] Instruction abort from EL0 at FAR=0x3004839c, ISS=0x6
```

ISS=0x6 is a level-2 translation fault: the page is not mapped.

### Root Cause

The dynamic linker (`ld-musl-aarch64.so.1`) is loaded at `0x3000_0000` by
`load_interpreter()` in `elf_loader.rs`. These pages are mapped directly
into the address space but **not tracked in `mmap_regions`**. When
`fork_process` copies memory, it copies:

1. Stack (`0x3ffc0000–0x40000000`)
2. Code + heap (`0x10000000` to `brk`)
3. `mmap_regions` (tracked mmap pages)

The interpreter at `0x3000_0000` falls outside all three ranges.

### Fix

**File:** `src/process.rs`

Added an explicit copy of the interpreter region in `fork_process`, before
the mmap copy. Scans `0x3000_0000` through `0x3020_0000` (2 MB) using the
existing `copy_range_phys` helper, which skips unmapped pages automatically.

## Issue 8: Fork Doesn't Copy Main Binary Code Pages (Dynamic Binaries)

### Symptoms

After fixing the interpreter copy (issue 7), the child still crashes:

```
[Fault] Instruction abort from EL0 at FAR=0x101b7924, ISS=0x6
```

The fault address is in the main binary's code region (`0x10000000` range),
not the interpreter.

### Root Cause

`fork_process` derived `code_start` from `parent.entry_point`. For
dynamically linked binaries, `entry_point` points to the **interpreter's**
entry (e.g., `0x3006XXXX`), not the main binary's start (`0x10000000`).
This caused `code_start` to be computed as `0x30000000`, and since `brk`
(`~0x10326000`) was less than that, the entire code + heap copy was skipped.

### Fix

**File:** `src/process.rs`

Changed `fork_process` to derive `code_start` from `parent.memory.code_end`
(which is always in the main binary's range) instead of `parent.entry_point`:

```rust
let code_start = if parent.memory.code_end >= 0x1000_0000 {
    0x1000_0000  // PIE binary base
} else {
    0x400000     // non-PIE binary base
};
```

## Issue 9: `CLONE_THREAD` Not Implemented (pthread_create fails)

### Symptoms

```
[syscall] clone(flags=0x7d0f00, stack=0x20443af0)
[syscall] clone: flags not supported, returning ENOSYS
error: cannot create async thread
fatal: fetch-pack: unable to fork
```

Git's fetch-pack creates an async thread via `pthread_create` for sideband
filtering. musl's `pthread_create` calls `clone` with thread-creation flags.

### Root Cause

`sys_clone` only handled two flag patterns:
- `CLONE_VFORK` (0x4000) — vfork-like clone
- `SIGCHLD` (flags & 0x11 == 0x11) — regular fork

Thread creation flags `0x7d0f00` (CLONE_VM | CLONE_FS | CLONE_FILES |
CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM | CLONE_SETTLS |
CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID) matched neither pattern.

Additionally, syscall 283 (`membarrier`) was unhandled. musl calls
`membarrier(MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED)` during
`pthread_create`, causing a spurious "Unknown syscall" warning.

### Fix

**Files:** `src/process.rs`, `src/syscall.rs`, `src/mmu.rs`

**1. `UserAddressSpace::new_shared()` (mmu.rs)**

Added a constructor that creates an address space view sharing the parent's
L0 page table. Uses its own ASID but points at the same physical page
tables. The `shared` flag prevents `Drop` from freeing the parent's pages.

**2. `clone_thread()` (process.rs)**

New function for `CLONE_THREAD | CLONE_VM`. Creates a child Process that
shares the parent's address space (not a copy). The child thread gets:
- Same page tables as parent (shared memory via `UserAddressSpace::new_shared`)
- Its own kernel thread with a separate stack pointer
- TLS set via `CLONE_SETTLS` (stored in `UserContext.tpidr`)
- Clone returns 0 to child, child PID to parent

**3. `THREAD_PID_MAP` (process.rs)**

Thread clones share the parent's ProcessInfo page (at `0x1000`), so
`read_current_pid()` would return the parent's PID for child thread
syscalls. A `THREAD_PID_MAP` (thread_id → PID) is checked first in
`current_process()` to correctly route syscalls to the child's Process.
Entries are cleaned up in `return_to_kernel()`.

**4. `sys_clone` dispatch (syscall.rs)**

Added detection of `CLONE_THREAD | CLONE_VM` flags before the existing
fork paths. Delegates to `clone_thread()`.

**5. `membarrier` stub (syscall.rs)**

Added syscall 283 as a no-op returning 0. Safe on a single-CPU system.

## Issue 10: `pread64` Not Implemented (index-pack fails)

### Symptoms

```
fatal: cannot pread pack file: Function not implemented
fatal: fetch-pack: invalid index-pack output
```

Git successfully received all 991 objects, but `index-pack` failed when
trying to read individual objects from the downloaded pack file.

### Root Cause

Syscall 67 (`pread64`) was not implemented. `pread64` reads from a file
at a specific offset without changing the file descriptor's position —
essential for random access into pack files. Syscall 103 (`setitimer`)
was also missing; git uses it for progress display timing.

### Fix

**File:** `src/syscall.rs`

- **`sys_pread64`** — reads from a file at a caller-specified offset via
  `crate::fs::read_at()`, without modifying the fd position. Handles
  `DevNull` (returns 0/EOF).
- **`sys_pwrite64`** — write counterpart, added for completeness.
- **`setitimer`** — stubbed as no-op returning 0.

## Issue 11: `CLONE_CHILD_CLEARTID` Not Implemented (pthread_join hangs)

### Symptoms

```
Resolving deltas: 100% (637/637), done.
```

Git completed the clone (100% objects, 100% deltas) but never returned
control to the shell. The kernel heartbeat showed `wait=1` — one thread
blocked forever.

### Root Cause

After the async thread (PID 32, created via `CLONE_THREAD` in issue 9)
exited, `pthread_join` in the main git process looped on
`futex_wait(&thread->tid, tid_value)`. Linux's `CLONE_CHILD_CLEARTID`
contract requires the kernel to:

1. Write 0 to `*clear_child_tid` when the thread exits
2. Wake the futex at that address

Neither step was implemented. The TID address still contained the
child's PID, so `futex_wait` never saw a value change.

### Fix

**Files:** `src/process.rs`, `src/syscall.rs`

**1. Added `clear_child_tid: u64` to `Process` struct**

Stores the userspace address where the kernel must write 0 on exit. Set
by `clone_thread()` (from the `child_tid` argument) and by the
`set_tid_address` syscall (which musl calls during thread init).

**2. `set_tid_address` syscall now functional**

Previously returned a hardcoded 1. Now stores the pointer in
`proc.clear_child_tid` and returns the process PID.

**3. `return_to_kernel()` clears TID and wakes futex**

Before deactivating the user address space (while pages are still
mapped), writes 0 to `clear_child_tid` and calls `futex_wake()`. This
unblocks any `pthread_join` waiter.

**4. Added `pub fn futex_wake()`**

Public wrapper in `syscall.rs` callable from `process.rs`.

## Issue 12: CLONE_THREAD Exit Destroys Shared FD Table (git-clone exits 128)

### Symptoms

`git clone` completed object download but failed with exit code 128. The
failure occurred after the sideband thread (created via `pthread_create`) had
exited. git-index-pack never received pack data even though git-remote-http had
successfully downloaded all objects.

### Root Cause

`sys_exit` called `proc.fds.close_all()` unconditionally. CLONE_THREAD siblings
created by `pthread_create` share the parent's `Arc<SharedFdTable>` — they do
**not** have their own fd table. When the sideband thread (tid=12, a CLONE_THREAD
sibling with `tgid != pid`) called `exit()`, `sys_exit` drained the shared table,
closing every pipe and socket visible to the entire thread group. The pipe that
git-fetch-pack's main thread (tid=13) was about to use for pack data was among
the destroyed fds, so git-index-pack never received data and git-clone failed.

On Linux, `sys_exit()` for a non-leader thread must NOT close the shared FD
table. Only `sys_exit_group()` (or the thread-group leader) does that.

### Fix

**File:** `src/syscall/proc.rs`

Added a guard around `close_all()` in `sys_exit`:

```rust
// CLONE_THREAD siblings share the parent's Arc<FdTable>. Only the
// thread-group leader (tgid == pid) may close the table on exit.
if proc.tgid == proc.pid {
    proc.fds.close_all();
}
```

`sys_exit_group` was already correct (it always closes the table, because
`exit_group` terminates the entire thread group).

**Kernel test:** `test_clone_thread_exit_preserves_shared_fd_table` in
`src/process_tests.rs` — verifies that simulating a sibling exit (`tgid != pid`)
leaves the shared table intact, while a leader exit (`tgid == pid`) clears it.

## Issue 13: CLONE_THREAD Registered as waitpid Child (git hangs 110 s)

### Symptoms

`git clone` transferred the pack successfully (index-pack exited normally,
pack/idx files written) but never populated the working tree. The SSH session
ran for ~110 seconds then was killed by the Python `subprocess.run(timeout=120)`
watchdog. No `hello.c` or other working-tree files existed afterward; only
the `.git` directory was present.

Kernel PSTATS showed git (PID 118) frozen at exactly 948 syscalls from T+20s
through T+110s — zero new syscalls, constant `in_kernel` time. The sideband
demux pthread (tid=11, created by `start_async`) was alive in the scheduler
but never recycled.

### Root Cause

`clone_thread()` (called for `CLONE_THREAD | CLONE_VM`) called
`register_child_channel(child_pid, exit_channel, parent_pid)`, registering
the new pthread as a waitpid-able "child" of the calling process.

On Linux, `CLONE_THREAD` threads belong to the same thread group and are
**never** visible to `waitpid` / `wait4`. Programs that call `wait4(-1, ...)`
only expect fork-created children to appear.

In git's `fetch-pack` flow:

1. git calls `start_async(&demux)` → `pthread_create` → `clone_thread` → demux
   thread (tid=11) registered in `CHILD_CHANNELS` with `ppid = PID 118`.
2. git calls `wait4(-1, ...)` waiting for real subprocesses (index-pack, remote-https).
3. `has_children(118)` returns true (demux thread is in `CHILD_CHANNELS`).
4. `find_exited_child(118)` correctly returns index-pack (PID 122) once it exits.
5. After reaping index-pack, git calls `wait4(-1, ...)` again.
6. `has_children(118)` still true — the demux thread (clone_pid) is still in
   `CHILD_CHANNELS` and has never exited.
7. `find_exited_child(118)` returns None; git blocks forever.

git never reached the pack-rename or checkout steps; the SSH watchdog killed
it 110 seconds later.

### Fix

**File:** `crates/akuma-exec/src/process/mod.rs`

Removed the `register_child_channel` call from `clone_thread`:

```rust
// CLONE_THREAD threads are NOT visible to waitpid on Linux — they belong
// to the same thread group and are never reaped by the parent.
// Registering them in CHILD_CHANNELS caused wait4(-1) to block forever
// on git's sideband demux pthread.
let exit_channel = Arc::new(ProcessChannel::new());
register_channel(tid, exit_channel.clone());
// register_child_channel(child_pid, exit_channel, parent_pid);  ← REMOVED
```

`register_channel(tid, ...)` is kept so the thread's own exit notification
channel works correctly (for `set_tid_address` / `CLONE_CHILD_CLEARTID`
wakeups). Only the `wait4`-visible "child" entry is removed.

After this fix, `wait4(-1)` from PID 118 only sees fork children (index-pack,
remote-https). Once both exit and are reaped, `has_children` returns false and
`wait4(-1)` returns `ECHILD`. git proceeds to rename the pack files and
populate the working tree.

## Summary

With all 13 fixes, `git clone https://...` works end-to-end on Akuma:

| Issue | Error | Root Cause |
|-------|-------|------------|
| 1 | `could not open '/dev/null'` | No `/dev/null` device |
| 2 | `Operation not permitted` | `mkdirat` returned wrong errno |
| 3 | Permissions always 644 | `chmod`/`stat` not wired to ext2 |
| 4 | `chmod on config.lock failed` | `O_CREAT` lazy file creation |
| 5 | `unable to find remote helper` | `clone3` syscall missing |
| 6 | `unable to find remote helper` | `O_CREAT` ignoring `mode` |
| 7 | Child crash at `0x3004XXXX` | Fork missing interpreter pages |
| 8 | Child crash at `0x101bXXXX` | Fork missing main binary pages |
| 9 | `cannot create async thread` | `CLONE_THREAD` not implemented |
| 10 | `cannot pread pack file` | `pread64` not implemented |
| 11 | Hangs after "done" | `CLONE_CHILD_CLEARTID` missing |
| 12 | Exit 128 after pack download | `sys_exit` closed shared FD table for CLONE_THREAD sibling |
| 13 | Hangs 110 s, no working tree | `clone_thread` registered pthread in `CHILD_CHANNELS`; `wait4(-1)` blocked on it forever |

## Future Work

Other device files that programs commonly expect:

| Device | Behavior | Priority |
|--------|----------|----------|
| `/dev/zero` | Reads return zero bytes, writes discarded | Medium |
| `/dev/urandom` | Reads return random bytes | Medium |
| `/dev/random` | Same as urandom on modern Linux | Low |
| `/dev/tty` | Alias for controlling terminal | Low |
| `/dev/fd/N` | Alias for fd N (`/proc/self/fd/N`) | Low |

These can follow the same pattern: intercept in `sys_openat`, add a
`FileDescriptor` variant, handle in read/write/stat.
