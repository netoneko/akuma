//! File descriptors, and the file syscalls that use them.
//!
//! Stage O. Until now ring 3 could `write` to the console and nothing else; the
//! kernel could read files (Stage N) and userspace could not. This is the layer
//! that closes that gap, and it is the last thing between the target and an
//! interactive shell.
//!
//! # The descriptor type is `akuma-exec-core`'s, not a local one
//!
//! [`FileDescriptor`] and [`KernelFile`] come from `akuma-exec-core` — the
//! unsafe-free core of the kernel's execution crate, which builds for
//! `x86_64-unknown-none` behind only `akuma-primitives`, `akuma-mmap` and
//! `akuma-syscalls-linux`. This module first defined its own `OpenFile`, which
//! was the wrong instinct twice: it duplicated a type the tree already has, and
//! the tree's version is *better* — it carries a `dir_cache` for `getdents64`,
//! and it addresses a file by `(mount_id, inode)` so a `read` never re-resolves
//! a path that could now mean a different file.
//!
//! `KernelFile::new(path, flags)` leaves the inode 0, which its own doc defines
//! as "no inode: read by path". That is exactly this target's situation — there
//! is one filesystem and no mount table — so the shared type is used in the mode
//! it already has for the case, rather than being extended for it.
//!
//! # Contents are **not** cached any more (C2 slice 5)
//!
//! `open` used to read the whole file through `fs::read_file` and hold the
//! bytes alongside the descriptor, so a file occupied its own size in kernel
//! heap for as long as it was open and a write `resize`d that `Vec` — which
//! doubles. Writing an N-byte file needed ~3N of heap at the last doubling, and
//! `alloc_error_handler` calls `halt()`, so one large write permanently removed
//! a core (`proposals/AMD64_FD_WHOLE_FILE_HEAP.md`).
//!
//! A descriptor now carries an **empty** buffer and every read and write goes
//! to the VFS at the cursor in [`MAX_IO`]-bounded chunks (`fs::read_at`,
//! `akuma_vfs_glue::write_at`), disk I/O outside the table lock. The heap
//! cost of a write is one 64 KiB chunk rather than three times the file.
//!
//! **Synthetic `/proc` renders are re-rendered per `read(2)`** (step 4b). The
//! render used to be cached in the description at `open` — Linux `seq_file`'s
//! snapshot semantics — and that cache was the last field only [`Entry`]
//! carried. Re-rendering per read is what the mounted `ProcFilesystem` —
//! the fold destination, and what `akuma-syscalls-glue` serves `/proc`
//! through on AArch64 — already does, so the target converges on the tree's
//! behaviour instead of carrying a second structure (a side table) or
//! extending a shared type (`KernelFile`) for one architecture's sake. The
//! cost is the snapshot: two reads of one `/proc` file can see two renders.
//! A synthetic **directory** still snapshots into `KernelFile::dir_cache`
//! and needs nothing.
//!
//! # The descriptor table is the registered `SharedFdTable` (C2 step 4b)
//!
//! The table used to be one flat 64-entry array shared by every task, with an
//! `owner` field on each entry and a sweep at exit to reclaim what a task had
//! not closed. That was stated here as "right for now: this target runs one
//! interactive program at a time" — which stopped being true the day it grew
//! `fork`, and had three separate costs by 2026-09-06:
//!
//! - **One budget for the machine.** `apk` installing 14 packages ran the
//!   table dry on its own, and the next `apk` started from a full one.
//! - **A `close` in a child reached into its parent.** Both named the same
//!   array slot, so `sh -c 'prog > file'` could not work even in principle.
//! - **Descriptor numbers were machine-wide.** A program's first `open`
//!   returned whatever slot happened to be free.
//!
//! The 2026-09-06 fix was the POSIX split — one row per process (`FDS`), one
//! refcounted machine-wide table of descriptions (`FILES`) — with the
//! registered `SharedFdTable` as a mirror that owned nothing. That mirror era
//! is over: **the registered table is the only name→description map**, and the
//! refcount authority lives in it. `FDS`, `FILES` and `Entry` are deleted.
//! `fork` copies the parent's table through `clone_deep_for_fork` (which bumps
//! the pipe/socket refs); `close` removes the name and releases one
//! reference; `SharedFdTable::close_all` — the same sweep its own `Drop` runs
//! — is the exit teardown.
//!
//! The reference rule is one sentence: **every `PipeRead`/`PipeWrite`/`Socket`
//! entry in a table is backed by one pipe-end/socket reference.** A copy
//! (`dup`, `fork`) bumps through [`clone_refs`]; a removal releases through
//! [`release_desc`]. The reference a pipe's `alloc` starts each end with is
//! consumed by the *first* insert and bumped for every later one.
//!
//! Two divergences are adopted with the flip, both of them
//! `akuma-syscalls-glue`'s existing behaviour — which is the point: this
//! target's file surface is becoming the tree's, and a model the AArch64
//! kernel already self-hosts on is proven rather than speculative. They are
//! pinned at the self-tests that used to assert the opposite:
//!
//! - **`dup` copies the `KernelFile` by value**, so two descriptors onto one
//!   file get *independent cursors*. POSIX shares the open file description;
//!   fixing that honestly is a change to `akuma-exec`'s `FileDescriptor`
//!   (an `Arc` inside the variant), not to this file.
//! - **`nonblocking` is keyed by fd number** (the table's `nonblock` set),
//!   where [`Entry::nonblocking`] used to be keyed by description — so
//!   `dup`ping a non-blocking socket loses the flag.
//!
//! # Descriptors 0/1/2 are real (C2 slice 6)
//!
//! They were "answered by number, above this layer" — not in a row at all,
//! which is why `dup2(fd, 1)` had nowhere to land and `echo x > file` and
//! `cmd | cmd` both failed. `dup2` learnt to land in slice 4; slice 6 finished
//! the job at the other end, where the numbers came *from*: a spawned child
//! now gets fd 0/1/2 as real `PipeRead`/`PipeWrite` entries at birth
//! ([`bind_stdio`]), so the `Spawn`-row router that used to answer "which pipe
//! serves this task's stdout" is gone, along with the three fields it read.
//!
//! Two rules survive the change and are the reason the guards below are
//! spelled `fd < FIRST_FILE_FD && !is_bound(fd)` rather than `fd < 3`:
//!
//! - an **unbound** 0/1/2 is the console, and that is now its only meaning —
//!   init on the serial line, and the boot suite's [`KERNEL_TABLE`];
//! - a **bound** one is whatever it names, so every operation has to ask the
//!   table rather than assume. Getting that wrong is silent: `lseek` on a bound
//!   1 answered `EBADF` for months because its guard did not ask.
//!
//! Allocation still starts at [`FIRST_FILE_FD`], which is the pinned "first
//! free fd is 3" divergence — POSIX's "lowest available" includes a closed
//! 0/1/2. [`bind_stdio`] writes the three slots directly for that reason.

use akuma_exec_core::process::{FileDescriptor, KernelFile};
#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;
use akuma_terminal::TerminalState;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::fs;
use crate::serial;

/// Map an `FsError` from the VFS byte paths onto a syscall errno.
///
/// **`akuma-syscalls-glue`'s `fs_error_to_errno`, arm for arm.** One table, so
/// a folded arm answers what this file answers.
///
/// It used to be three arms and a catch-all, defended by a comment saying
/// "inventing eight more errnos nobody distinguishes is not honesty, it is
/// noise". The argument was sound and the premise was not: half of these are
/// distinguished, by callers this target already runs. `EISDIR` is how a
/// program learns to call `getdents64` instead of `read`; busybox `find` reads
/// `ENOTDIR` to stop descending; `EROFS` is what tells a writer the mount is
/// the problem rather than the disk. `EIO` reads as a *hardware* fault and
/// sends whoever gets it looking at the wrong layer entirely.
///
/// The four `*at` calls below (`mkdirat`, `unlinkat`, `symlinkat`,
/// `utimensat`) each carried their own two- or three-arm subset of this list
/// ending in `EIO`, which is the drift `clone_fd_refs`'s header describes in
/// the other half of the tree: several partial copies of one table, each
/// correct for the cases its author happened to hit.
///
/// One divergence from glue's table, deliberate: `NotSupported` is `ENOSYS`
/// here and falls through to `EIO` there. `utimensat` is the caller that
/// wants it — "this filesystem does not keep times" is not an I/O error, and
/// a build system reading `ENOSYS` stops asking.
fn fs_err_errno(e: akuma_vfs::FsError) -> u64 {
    use akuma_vfs::FsError as E;
    match e {
        E::NotFound => errno::ENOENT,
        E::PermissionDenied => errno::EACCES,
        E::AlreadyExists => errno::EEXIST,
        E::NotADirectory => errno::ENOTDIR,
        E::NotAFile => errno::EISDIR,
        E::DirectoryNotEmpty => errno::ENOTEMPTY,
        E::NoSpace => errno::ENOSPC,
        E::ReadOnly => errno::EROFS,
        E::InvalidPath => errno::EINVAL,
        E::TooManyOpenFiles => errno::EMFILE,
        E::NotSupported => errno::ENOSYS,
        _ => errno::EIO,
    }
}

/// Does the path this descriptor names a directory?
///
/// The replacement for `Entry::is_dir`, which was a bool five constructors had
/// to set correctly and which `akuma-syscalls-glue` does not have — glue
/// refuses a write open of a directory at `open(2)` and otherwise lets the VFS
/// answer, so this target grew the same shape ahead of the fold.
///
/// `/proc` is asked of [`proc_metadata`] and everything else of the VFS, in
/// that order, because a synthetic view has no inode for `fs::metadata` to
/// find: `/proc/<pid>` is a directory that exists only in the process table,
/// and asking the disk about it answers "no such file" rather than "not a
/// directory". `/proc` itself is on the image as a real empty directory, so
/// either source would do for that one; the ordering makes the answer come
/// from the thing that renders it.
///
/// The prefix test is `== "/proc"` or `starts_with("/proc/")` rather than
/// `strip_prefix("/proc")`, which would also claim a file named `/procfoo`.
fn path_is_dir(path: &str) -> bool {
    if path == "/proc" || path.starts_with("/proc/") {
        // [`proc_is_dir`], not [`proc_metadata`]: the latter answers the size
        // too, and it gets it by **rendering the whole file**. Asking it here
        // would re-render `/proc/meminfo` on every `fstat` of it purely to
        // learn that it is not a directory.
        let rest = path.strip_prefix("/proc").unwrap_or("");
        return proc_is_dir(&normalise_proc(rest));
    }
    fs::metadata(path).is_ok_and(|m| m.is_dir)
}

/// The `/proc`-relative rest of a synthetic descriptor's path, for
/// `render_proc_file` — `None` for a real (ext2) path. The discriminator is
/// the path prefix itself (`== "/proc"` or `starts_with("/proc/")`, never
/// `strip_prefix("/proc")`, which would also claim a file named `/procfoo`):
/// `sys_openat` intercepts every `/proc` open, so a descriptor whose path is
/// `/proc`-prefixed was created by this module and has no inode behind it.
fn proc_rest_of(path: &str) -> Option<&str> {
    if path == "/proc" {
        Some("")
    } else {
        path.strip_prefix("/proc/")
    }
}

/// The console's line discipline.
///
/// `akuma-terminal`, not a hand-rolled reader. That crate is the tree's
/// canonical-mode implementation — line buffering, backspace, Ctrl+D as EOF,
/// echo, and `map_cr_to_nl` — and it is `no_std`, dependency-free apart from a
/// spinlock, and already built for `x86_64-unknown-none`.
///
/// `map_cr_to_nl` is the one that would have cost a debugging session: a serial
/// terminal sends **CR** when Enter is pressed, and every line-oriented reader
/// waits for **NL**. A naive byte-at-a-time console read looks correct, echoes
/// what you type, and never returns a line.
///
/// `push_input`'s doc warns that its caller must hold the outer lock with
/// preemption disabled for the duration. That discipline is satisfied here for a
/// reason that will not survive: this target polls the UART from the reading
/// thread itself rather than from an interrupt, so there is no second context to
/// race with. When the 16550 gets an IRQ, this becomes a real obligation.
static CONSOLE: Spinlock<Option<TerminalState>> = Spinlock::new(None);

/// Bring the console line discipline up. Called once, before ring 3 exists.
pub fn init_console() {
    *CONSOLE.lock() = Some(TerminalState::default());
}

/// Linux errno values, negated as the kernel ABI returns them.
pub mod errno {
    pub const EBADF: u64 = (-9i64) as u64;
    pub const ENOTSOCK: u64 = (-88i64) as u64;
    pub const EAFNOSUPPORT: u64 = (-97i64) as u64;
    pub const ENOENT: u64 = (-2i64) as u64;
    pub const EFAULT: u64 = (-14i64) as u64;
    pub const EINVAL: u64 = (-22i64) as u64;
    pub const EMFILE: u64 = (-24i64) as u64;
    /// The machine, not this process, is out of open file descriptions.
    /// Distinct from `EMFILE` on purpose — see `install`.
    pub const ENFILE: u64 = (-23i64) as u64;
    pub const ENOTTY: u64 = (-25i64) as u64;
    pub const ENODEV: u64 = (-19i64) as u64;
    pub const ENOSYS: u64 = (-38i64) as u64;
    pub const ESRCH: u64 = (-3i64) as u64;
    /// No child to wait for. Distinct from [`ESRCH`] on purpose: a shell tests
    /// for exactly this to stop reaping, and `sys_waitpid` returned `ESRCH`
    /// until 2026-09-08.
    pub const ECHILD: u64 = (-10i64) as u64;
    pub const EAGAIN: u64 = (-11i64) as u64;
    pub const ENOMEM: u64 = (-12i64) as u64;
    pub const ENOTDIR: u64 = (-20i64) as u64;
    pub const EISDIR: u64 = (-21i64) as u64;
    pub const EEXIST: u64 = (-17i64) as u64;
    pub const ENOTEMPTY: u64 = (-39i64) as u64;
    pub const EIO: u64 = (-5i64) as u64;
    /// The filesystem is mounted read-only (`MS_RDONLY` → `FsError::ReadOnly`).
    pub const EROFS: u64 = (-30i64) as u64;
    /// The device has no room — a write the VFS could not place.
    pub const ENOSPC: u64 = (-28i64) as u64;
    /// Written to a pipe every reader has closed. New with `akuma-pipes`' end
    /// reference counts — before them this kernel could not tell a dead reader
    /// from a full buffer, and `write_pipe`'s retry loop span forever instead.
    pub const EPIPE: u64 = (-32i64) as u64;
    /// `FUTEX_WAIT` ran out of time. The one errno a futex wait can return
    /// that no other syscall here produces.
    pub const ETIMEDOUT: u64 = (-110i64) as u64;
    /// Interrupted. The one errno this kernel originates rather than reports:
    /// a thread whose group called `exit_group` gets it for the syscall it was
    /// in the middle of, so the call fails rather than acting on a dying
    /// address space.
    pub const EINTR: u64 = (-4i64) as u64;
    /// Not a seekable descriptor. `pread`/`pwrite` on a pipe, socket or the
    /// console — an answer about the *kind* of descriptor, which is why it is
    /// not `EBADF`: musl's `FILE` layer falls back to `read()` on `ESPIPE` and
    /// gives up on `EBADF`.
    pub const ESPIPE: u64 = (-29i64) as u64;
    /// Permission denied. `mmap` on a descriptor that is not a regular file —
    /// Linux's answer for a mapping request the descriptor cannot back.
    pub const EACCES: u64 = (-13i64) as u64;

    /// Does a syscall return value carry an errno? Linux errnos are `1..=4095`,
    /// returned as `(-errno) as u64` — the very top of the range. Anything below
    /// is a real result.
    #[must_use]
    pub const fn is_err(r: u64) -> bool {
        r > u64::MAX - 4096
    }
}

/// The first descriptor number [`bind`] will hand out.
///
/// **Not** "0/1/2 are the console and are never in the table" any more — since
/// C2 slice 6 a spawned child's are pipes in its row from birth. What this
/// constant still means is the pinned allocation divergence: a new `open`
/// starts looking at 3, so `close(1); open(f)` returns 3 here and 1 on Linux.
pub const FIRST_FILE_FD: usize = 3;
/// How many descriptors **one process** may hold.
///
/// A fixed array so the table allocates nothing; the *contents* are heap, but
/// the bookkeeping is not.
///
/// It was 16, then 64, and both were one number for the whole machine. The
/// history is worth keeping because each step was a real failure: 16 came from
/// "more than a shell opens", and `apk update` — a package database, a
/// repository index and a TLS connection per repository — gave up before
/// reaching the network with
///
///     ERROR: Unable to open root: No file descriptors available
///
/// which reads like a permissions or path problem and is neither. 64 bought a
/// package manager with a handful of repositories, and was still a *shared*
/// budget: `apk` installing 14 packages ran it dry on its own, and a second
/// `apk` in a fresh process started from a table the first had filled.
///
/// The budget is now per-process, so the number answers a different question —
/// "how many descriptors may one program hold", not "how many may the machine".
/// 256 is what a `rustc` on a real crate graph needs; `cargo` running four of
/// them concurrently now costs 4 × its own budget rather than sharing one.
pub const MAX_FDS: usize = 256;


/// The descriptor table the boot task (the self-tests, and anything that runs
/// before a process is registered) resolves against.
///
/// It is the [`KERNEL_ROW`] of the deleted `FDS` array under the new
/// authority: [`cur_table`] answers the registered process's table for a user
/// task and this one otherwise, so the suite's `sys_openat`/`sys_read` calls
/// keep working unchanged. Nothing reaps it — the boot task never exits, and
/// the suite closes what it opens — which is exactly the lifetime the kernel
/// row had.
static KERNEL_TABLE: akuma_exec::process::SharedFdTable = akuma_exec::process::SharedFdTable {
    table: Spinlock::new(alloc::collections::BTreeMap::new()),
    cloexec: Spinlock::new(alloc::collections::BTreeSet::new()),
    nonblock: Spinlock::new(alloc::collections::BTreeSet::new()),
};

/// The table a syscall resolves its descriptors against.
///
/// The registered `SharedFdTable` — the one authority since step 4b. `None`
/// from [`crate::usermode::current_process`] (a kernel thread, or the boot
/// task driving the self-tests) falls to [`KERNEL_TABLE`], which is "no
/// process" *and* "a table" in exactly the way the old kernel row was.
#[inline]
fn cur_table() -> &'static akuma_exec::process::SharedFdTable {
    match crate::usermode::current_process() {
        Some(p) => &p.fds,
        None => &KERNEL_TABLE,
    }
}

/// Clone `fd`'s description out of the calling table, or `None` if the fd
/// names nothing — a closed descriptor, one out of range, or a console
/// descriptor.
fn table_get(fd: u64) -> Option<FileDescriptor> {
    table_get_in(cur_table(), fd)
}

fn table_get_in(t: &akuma_exec::process::SharedFdTable, fd: u64) -> Option<FileDescriptor> {
    if fd >= MAX_FDS as u64 {
        return None;
    }
    t.table.lock().get(&(fd as u32)).cloned()
}

/// Borrow `fd`'s description in the calling table, for the duration of `f`.
///
/// The table lock is held only across the closure — no I/O, no user copy,
/// no second lock. The old [`with_file`] could not promise that: it had to
/// resolve through `FDS` and then take `FILES`, two holds that were never
/// nested, and its header had to argue why that was safe. One table, one
/// hold; the argument retired with the structure that needed it.
fn table_with<R>(fd: u64, f: impl FnOnce(&mut FileDescriptor) -> R) -> Option<R> {
    table_with_in(cur_table(), fd, f)
}

fn table_with_in<R>(
    t: &akuma_exec::process::SharedFdTable,
    fd: u64,
    f: impl FnOnce(&mut FileDescriptor) -> R,
) -> Option<R> {
    if fd >= MAX_FDS as u64 {
        return None;
    }
    t.table.lock().get_mut(&(fd as u32)).map(f)
}

/// Does `fd` name an open descriptor in the calling table?
///
/// The question `sys_write` asks before falling back to the console: a bound
/// 1 or 2 has been redirected and must go where the table says, not to the
/// serial port.
#[must_use]
pub fn is_bound(fd: u64) -> bool {
    fd < MAX_FDS as u64 && cur_table().table.lock().contains_key(&(fd as u32))
}

/// Take one more reference to whatever `desc` names — the [`clone_fd_refs`]
/// rule, local to the variants this target interns.
///
/// `PipeRead`/`PipeWrite`/`Socket` are the refcounted families here; a `File`
/// is unrefcounted (its bytes live in the filesystem, its cursor is copied by
/// value — see the module header for what that makes `dup`), and the other
/// variants are never interned by this module. The match stays exhaustive
/// rather than falling through a `_`, so a variant added to `FileDescriptor`
/// is a compile error here and not a silently unreferenced copy — which is
/// the property `akuma_exec::process::clone_fd_refs` exists for on AArch64.
/// This target cannot call that function for its *own* tables without
/// dragging the `ExecRuntime` hook machinery into paths (the boot suite's
/// kernel-table ops) that never needed it.
///
/// [`clone_fd_refs`]: akuma_exec::process::clone_fd_refs
fn clone_refs(desc: &FileDescriptor) {
    match desc {
        FileDescriptor::PipeWrite(id) => crate::pipe::clone_ref(*id as usize, true),
        FileDescriptor::PipeRead(id) => crate::pipe::clone_ref(*id as usize, false),
        FileDescriptor::Socket(s) => akuma_net::socket::socket_clone_ref(*s),
        FileDescriptor::File(_)
        | FileDescriptor::Stdin
        | FileDescriptor::Stdout
        | FileDescriptor::Stderr
        | FileDescriptor::DevTty
        | FileDescriptor::DevNull
        | FileDescriptor::DevZero
        | FileDescriptor::DevDsp
        | FileDescriptor::DevUrandom
        | FileDescriptor::ChildStdout(_)
        | FileDescriptor::UnixSocket { .. }
        | FileDescriptor::EventFd(_)
        | FileDescriptor::EpollFd(_)
        | FileDescriptor::PidFd(_)
        | FileDescriptor::RumpSocket { .. }
        | FileDescriptor::Tap { .. }
        | FileDescriptor::TimerFd(_)
        | FileDescriptor::BlockDev { .. } => {}
    }
}

/// Release the reference `desc` holds, now that its last table entry is gone.
///
/// The [`release`] of the deleted `FILES` table, minus the reference count it
/// used to consult — the count is the set of table entries now, and this runs
/// only when one is removed. A `File` is a no-op: real files are unbuffered
/// (the cache died in C2 slice 5) and a synthetic `/proc` render has no inode
/// behind it to persist to — the old persist arm fired only for cached
/// writes, and it wrote to a path with no inode, so what it produced was a
/// console error line and no bytes.
fn release_desc(desc: &FileDescriptor) {
    match desc {
        FileDescriptor::Socket(s) => crate::sock::close(*s),
        FileDescriptor::PipeWrite(p) => crate::pipe::close_write(*p as usize),
        FileDescriptor::PipeRead(p) => crate::pipe::close_read(*p as usize),
        _ => {}
    }
}

/// Insert `desc` into the calling table under the lowest free descriptor at
/// or above [`FIRST_FILE_FD`], and hand the reference it holds to the table.
///
/// The two exhaustion cases keep their distinct errnos: `EMFILE` is *this
/// process* out of descriptor numbers (checked against [`MAX_FDS`]); the old
/// `ENFILE` — the machine out of open file descriptions — is gone with
/// `FILES`, whose fixed 512 slots were what it counted. A `BTreeMap` is
/// unbounded; the machine-wide ceilings that remain are real resources
/// (`pipe::alloc`'s `MAX_PIPES`, which still answers `ENFILE` at its own
/// call site).
fn install(desc: FileDescriptor) -> u64 {
    let t = cur_table();
    if t.table.lock().len() >= MAX_FDS {
        return errno::EMFILE;
    }
    let fd = t.alloc_fd_from(FIRST_FILE_FD as u32, desc);
    if fd as usize >= MAX_FDS {
        // `alloc_fd_from` found nothing under `u32::MAX`; unwind the entry it
        // inserted so the caller's `EMFILE` does not leak a live descriptor.
        t.table.lock().remove(&fd);
        return errno::EMFILE;
    }
    u64::from(fd)
}

/// Allocate a descriptor for an already-created socket.
///
/// Sockets live in the same table as files, as `FileDescriptor::Socket(idx)` —
/// the same variant the AArch64 kernel uses, carrying the same index into the
/// same `akuma_net::socket` table. Sharing the table is what makes `read` and
/// `write` work on a socket without the caller knowing.
///
/// The socket table entry's initial reference is consumed by this first
/// insert; every later copy bumps through [`clone_refs`].
pub fn alloc_socket_fd(idx: usize) -> Option<u64> {
    let fd = install(FileDescriptor::Socket(idx));
    (!errno::is_err(fd)).then_some(fd)
}

/// The socket index behind `fd`, or `None` if it is not a socket.
#[must_use]
pub fn socket_index(fd: u64) -> Option<usize> {
    table_get(fd).and_then(|d| match d {
        FileDescriptor::Socket(s) => Some(s),
        _ => None,
    })
}

/// Give `pipe_id` a descriptor: `PipeRead` for a reader end, `PipeWrite` for a
/// writer end. Used by `sys_spawn` (the parent's stdout reader) and
/// `sys_openat`'s `/proc/<pid>/fd/0` (the parent's stdin writer).
///
/// **Both callers are always second names.** The pipe `alloc` started one
/// reference per end, and the creating side's inserts consumed those (the
/// child's stdio in [`bind_stdio`], or the `pipe(2)` pair itself) — so this
/// descriptor's reference is a new one, bumped here and handed to `install`.
/// On a failed install the bump is given straight back.
pub fn alloc_pipe_fd(pipe_id: usize, is_write: bool) -> Option<u64> {
    let desc = if is_write {
        FileDescriptor::PipeWrite(pipe_id as u32)
    } else {
        FileDescriptor::PipeRead(pipe_id as u32)
    };
    crate::pipe::clone_ref(pipe_id, is_write);
    let fd = install(desc);
    if errno::is_err(fd) {
        if is_write {
            crate::pipe::close_write(pipe_id);
        } else {
            crate::pipe::close_read(pipe_id);
        }
        return None;
    }
    Some(fd)
}

/// The pipe id behind `fd` if it is a `PipeRead` descriptor.
#[must_use]
pub fn pipe_read_id(fd: u64) -> Option<usize> {
    table_get(fd).and_then(|d| match d {
        FileDescriptor::PipeRead(p) => Some(p as usize),
        _ => None,
    })
}

/// The `/dev` node name behind `fd`, or `None` for anything else.
///
/// The four operations that must answer differently for a device node —
/// [`sys_read`], [`sys_write_file`], [`sys_lseek`], [`sys_fstat`] — ask this
/// rather than carrying a variant, because the descriptor already holds the
/// only thing that identifies the node: its path. `dev_node` returns a
/// `&'static` name out of the table, so there is no allocation and nothing to
/// keep in sync.
///
/// The `starts_with` is the cheap gate: every other descriptor pays one string
/// compare and no lookup.
fn dev_node_of(fd: u64) -> Option<&'static str> {
    table_with(fd, |d| match d {
        FileDescriptor::File(f) if f.path.starts_with("/dev/") => {
            akuma_vfs_glue::dev_node(&f.path).map(|n| n.name)
        }
        _ => None,
    })
    .flatten()
}

/// The pipe id behind `fd` if it is a `PipeWrite` descriptor.
#[must_use]
pub fn pipe_write_id(fd: u64) -> Option<usize> {
    table_get(fd).and_then(|d| match d {
        FileDescriptor::PipeWrite(p) => Some(p as usize),
        _ => None,
    })
}

/// Read from a pipe, honouring `nonblock`. A blocking read **parks** until data
/// or EOF — which is safe on this target only because a pipe reader is never
/// also the pipe's writer (spawn wires them to different tasks).
///
/// Parks rather than spins since 2026-09-07. The loop is the same shape it was;
/// what replaced `yield_now` is the three-step wait `sched::prepare_block`
/// documents — arm, then test-and-register in one step, then park. Every wake
/// this can be waiting for is produced by the pipe table (`write`,
/// `close_write`, `close_read`, `destroy`) and fired by `pipe::fire`, so the
/// wake set is complete by construction rather than by inspection.
pub fn read_pipe(pipe_id: usize, buf: u64, len: usize, nonblock: bool) -> u64 {
    let mut tmp = alloc::vec![0u8; len.min(MAX_IO as usize)];
    loop {
        match crate::pipe::read(pipe_id, &mut tmp) {
            Some(0) => return 0, // EOF
            Some(n) => return copy_to_user(buf, &tmp[..n]),
            None if nonblock => return errno::EAGAIN,
            None => {
                // Arm before asking. A write landing between the question and
                // the park then finds `wake_pending` to set, and the park is a
                // no-op instead of a missed event.
                if !crate::pipe::check_set_reader(pipe_id) {
                    crate::sched::block_current();
                }
            }
        }
    }
}

/// Write to a pipe, honouring `nonblock`. A short write is returned as-is;
/// `sshd`'s bridge carries the residue.
pub fn write_pipe(pipe_id: usize, buf: u64, len: usize, nonblock: bool) -> u64 {
    let Some(data) = copy_in(buf, len as u64) else {
        return errno::EFAULT;
    };
    loop {
        // `None` is a pipe with no readers left. Checking it is not optional:
        // this loop retries a zero-count write forever, and before the pipe
        // table grew end reference counts a dead reader was indistinguishable
        // from a full buffer — so `busybox yes | busybox head -n 1` span here
        // rather than ending. There is no signal machinery on this target, so
        // `EPIPE` is the whole of Linux's answer that applies.
        let Some(n) = crate::pipe::write(pipe_id, &data) else {
            return errno::EPIPE;
        };
        if n > 0 || data.is_empty() {
            return n as u64;
        }
        if nonblock {
            return errno::EAGAIN;
        }
        // Full buffer: park until the reader drains it, or until the last
        // reader goes away — `check_set_writer` reports that second case as
        // "do not block", because a pipe with no readers never gains room and
        // the retry above is what turns it into `EPIPE`.
        if !crate::pipe::check_set_writer(pipe_id) {
            crate::sched::block_current();
        }
    }
}

/// Is `fd` marked `O_NONBLOCK`? `false` for anything not in the table.
///
/// Read from the table's `nonblock` set — keyed by **fd number**, which is
/// where `sys_fcntl`'s `F_SETFL` has always mirrored it. The per-description
/// `Entry::nonblocking` this replaces meant `dup` *kept* the flag across the
/// new name; the set means the dup **loses** it. That is
/// `akuma-syscalls-glue`'s existing behaviour, adopted with the flip and
/// pinned in the module header.
#[must_use]
pub fn is_nonblocking(fd: u64) -> bool {
    fd < MAX_FDS as u64 && cur_table().nonblock.lock().contains(&(fd as u32))
}

/// Name the description `fi` with the lowest free descriptor **at or above**
/// `min` — `fcntl(fd, F_DUPFD, min)`, and `F_DUPFD_CLOEXEC`, which is the same
/// with the new name marked close-on-exec.
///
/// # Why this exists (C2 slice 6)
///
/// It did not, and nothing noticed while descriptors 0/1/2 were unbound: an
/// `fcntl` on one of them resolved to nothing and answered `EBADF`, which is
/// the *one* error `busybox ash`'s `savefd()` forgives —
///
/// ```c
/// newfd = fcntl(from, F_DUPFD_CLOEXEC, 10);
/// err = newfd < 0 ? errno : 0;
/// if (err != EBADF) { if (err) ash_msg_and_raise_perror(...); close(ofd); }
/// ```
///
/// — so `echo x > file` worked by accident: ash asked to save fd 1, was told
/// there was no fd 1, recorded it as closed and carried on to the `open` and
/// the `dup2`. The moment [`bind_stdio`] gave a spawned child a *real* fd 1,
/// the same call resolved, fell through this function's absence to the `_ =>`
/// arm's `EINVAL`, and ash raised — **before** `openredirect` ran, so the
/// redirect exited non-zero and the file was never created at all. Six boot
/// checks, and none of them named `fcntl`.
///
/// The lesson is the general one: **making a descriptor real makes every
/// descriptor operation on it reachable.** `F_DUPFD` is not a slice-6 feature,
/// it is a hole slice 6 stopped hiding.
fn dup_from(fd: u64, min: u64, cloexec: bool) -> u64 {
    let t = cur_table();
    let Some(desc) = table_get(fd) else {
        return errno::EBADF;
    };
    // Linux answers `EINVAL` for a `min` past `RLIMIT_NOFILE`, not `EMFILE`:
    // the argument is out of range, rather than the table being full.
    let Some(min) = u32::try_from(min).ok().filter(|m| (*m as usize) < MAX_FDS) else {
        return errno::EINVAL;
    };
    let newfd = t.alloc_fd_from(min, desc.clone());
    if newfd as usize >= MAX_FDS {
        t.table.lock().remove(&newfd);
        return errno::EMFILE;
    }
    // Bumped only once the new name exists, as [`sys_dup`] does and for the
    // same reason.
    clone_refs(&desc);
    if cloexec {
        t.cloexec.lock().insert(newfd);
    }
    u64::from(newfd)
}

/// `fcntl(fd, cmd, arg)`. The flag commands, plus `F_DUPFD`/`F_DUPFD_CLOEXEC`
/// (see [`dup_from`]). `F_SETFL` only inspects the `O_NONBLOCK` bit — `sshd`
/// was long the sole caller and that is all it sets. `F_GETFL` reports the same
/// bit back and nothing else.
pub fn sys_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    const F_DUPFD: u64 = 0;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const F_SETFD: u64 = 2;
    const F_GETFD: u64 = 1;
    const F_DUPFD_CLOEXEC: u64 = 1030;
    const O_NONBLOCK: u64 = 0x800;
    const FD_CLOEXEC: u64 = 1;

    // Before the resolution below, because duplicating allocates a new name.
    if cmd == F_DUPFD || cmd == F_DUPFD_CLOEXEC {
        return dup_from(fd, arg, cmd == F_DUPFD_CLOEXEC);
    }

    let t = cur_table();
    // EBADF for anything the table does not name — the same resolution the
    // old `with_file` performed against the `FDS` row, console descriptors
    // included.
    if table_get(fd).is_none() {
        return errno::EBADF;
    }
    match cmd {
        // The one authority since the flip: the table's `nonblock` set, keyed
        // by fd number — the same set `is_nonblocking` reads and the one the
        // old `F_SETFL` arm mirrored into. The `Entry.nonblocking` field it
        // used to write first (and mirror from) is gone with `Entry`.
        F_SETFL => {
            if arg & O_NONBLOCK != 0 {
                t.nonblock.lock().insert(fd as u32);
            } else {
                t.nonblock.lock().remove(&(fd as u32));
            }
            0
        }
        F_GETFL => {
            if t.nonblock.lock().contains(&(fd as u32)) {
                O_NONBLOCK
            } else {
                0
            }
        }
        // **`FD_CLOEXEC` is stored in the table's `cloexec` set** and read
        // back by `F_GETFD` — but it still does not *do* anything: this
        // target's `execve` sweeps fds through its own path and does not
        // consult the set yet. That is the same "accepted, not enforced" shape
        // as before the flip. A descriptor marked close-on-exec still survives
        // one; the divergence stays pinned until that sweep folds.
        F_SETFD => {
            if arg & FD_CLOEXEC != 0 {
                t.cloexec.lock().insert(fd as u32);
            } else {
                t.cloexec.lock().remove(&(fd as u32));
            }
            0
        }
        F_GETFD => {
            if t.cloexec.lock().contains(&(fd as u32)) {
                FD_CLOEXEC
            } else {
                0
            }
        }
        _ => errno::EINVAL,
    }
}

/// Copy `len` bytes in from a user pointer. Public for `sock`.
///
/// `None` on a bad range or a fault — the caller returns `EFAULT`. Fault-safe
/// through `crate::uaccess` since 2026-09-05; a bad pointer used to halt here.
#[must_use]
pub fn copy_in(ptr: u64, len: u64) -> Option<Vec<u8>> {
    let mut out = alloc::vec![0u8; len as usize];
    crate::uaccess::read_bytes(ptr, &mut out).then_some(out)
}

/// Copy out to a user pointer. Public for `sock`. Returns the byte count, or
/// `errno::EFAULT` — see [`copy_to_user`].
#[must_use]
pub fn copy_out(ptr: u64, src: &[u8]) -> u64 {
    copy_to_user(ptr, src)
}


/// Give a not-yet-running spawned child its stdio as **real descriptors**:
/// fd 0 = the read end of its stdin pipe, fd 1 **and fd 2** = the write end of
/// its stdout pipe, in the child's own table.
///
/// **C2 slice 6.** This replaces the by-number stdio routing the `Spawn` row
/// used to carry (`Spawn::stdin_pipe`/`stdout_pipe`, consulted at every
/// unbound fd 0/1/2 read and write). Bound descriptors mean every existing
/// mechanism just works: `pipe_read_id`/`pipe_write_id` route the child's
/// I/O, `fork`'s `clone_deep_for_fork` shares the ends with correct
/// refcounts, and the child's exit `close_all` drops its ends — the EOF the
/// parent's reader waits for — with no per-spawn teardown code at all.
///
/// **fd 2 is a second *name*, not a second description**, and that is the
/// whole reason it is bound here rather than left to fall through to the
/// console: the old router answered fd 1 *and* fd 2 from `Spawn::stdout_pipe`,
/// so `prog > file` kept sending stderr to the session. Binding fd 2 to the
/// same write end — one more reference, exactly what `dup2(1, 2)` would build
/// — reproduces that: the `dup2(f, 1)` a shell emits for `>` drops one name
/// and the pipe end stays open under the other. Leaving fd 2 unbound and
/// answering it from fd 1 instead would put the second source of truth this
/// slice exists to delete back in, one indirection further along — and it
/// would break at exactly the redirect it was meant to survive.
///
/// Direct inserts rather than [`install`], deliberately: `install` starts at
/// [`FIRST_FILE_FD`] (the pinned "first free fd is 3" divergence), and stdio
/// is precisely the case that must land on 0/1/2. The reference rule: the
/// inserts at 0 and 1 **consume** the references `pipe::alloc` started each
/// end with; the insert at 2 is a second name for the write end and bumps one
/// more through [`clone_refs`]. Called before the child is published, so no
/// lock ordering question exists.
pub fn bind_stdio(
    table: &akuma_exec::process::SharedFdTable,
    stdin_pipe: usize,
    stdout_pipe: usize,
) -> u64 {
    {
        let mut t = table.table.lock();
        // The child's table is fresh (`SharedFdTable::new`), so 0/1/2 are free;
        // a stale entry here would mean the caller reused a table, and stdio
        // must not silently overwrite it.
        if t.contains_key(&0) || t.contains_key(&1) || t.contains_key(&2) {
            return errno::EINVAL;
        }
        t.insert(0, FileDescriptor::PipeRead(stdin_pipe as u32));
        t.insert(1, FileDescriptor::PipeWrite(stdout_pipe as u32));
        t.insert(2, FileDescriptor::PipeWrite(stdout_pipe as u32));
    }
    // fd 2: one more open description names the write end.
    crate::pipe::clone_ref(stdout_pipe, true);
    0
}

/// Largest single `read`/`write` this kernel will accept.
///
/// A bound rather than trust: the length comes from ring 3, and an unbounded one
/// would walk off the mapped page into whatever follows.
const MAX_IO: u64 = 64 * 1024;

/// Copy `src` to a user pointer.
///
/// Returns `src.len()` — or `errno::EFAULT`, already in syscall-return form, so
/// a caller whose result *is* the count returns this directly and every other
/// caller checks it with `errno::is_err`. Fault-safe through `crate::uaccess`.
#[must_use]
fn copy_to_user(ptr: u64, src: &[u8]) -> u64 {
    if crate::uaccess::write_bytes(ptr, src) {
        src.len() as u64
    } else {
        errno::EFAULT
    }
}

/// Dump a user C string to the serial trace (bounded, stops at NUL). Bring-up
/// aid for the path-taking syscall entry lines; a bad pointer traces as an
/// empty string rather than faulting the tracer. The `user VA` range check is
/// the cheap half of a fault handler — enough for a tracer, not for real
/// `copy_from_user` semantics (see the `akuma-user-access` row in the target
/// README for why that seam does not exist here yet).
pub fn trace_user_cstr(ptr: u64) {
    // A bad pointer traces as nothing: `read_cstr` range-checks and recovers.
    let Some(s) = crate::uaccess::read_cstr(ptr, 256) else {
        return;
    };
    for &b in s.iter().take_while(|b| b.is_ascii()) {
        serial::putb(b);
    }
}

/// Read a NUL-terminated path from user memory.
///
/// Bounded at 256 bytes, which is `PATH_MAX` for every path this kernel can
/// resolve; a longer one is a rejection rather than a truncation, because a
/// truncated path names a *different file* and opening it silently would be
/// worse than failing.
fn path_from_user(ptr: u64) -> Option<alloc::string::String> {
    alloc::string::String::from_utf8(crate::uaccess::read_cstr(ptr, 256)?).ok()
}

/// Resolve an `*at()`-syscall path against its `dirfd`.
///
/// Absolute paths ignore `dirfd`, per POSIX. A relative path resolves against
/// the directory the `dirfd` names — when it names one: a directory descriptor
/// in the table (how `apk` opens each key: `openat(keys_dirfd, name)` after
/// listing the very same directory, whose ignoring cost an afternoon). A
/// directory fd is reached only as a *path* here — this target's descriptors
/// cache no directory handle, so the join is string-level, which is exact for
/// the paths `mkdisk`-built images actually hold. `AT_FDCWD` (`-100`) keeps
/// the pre-`dirfd` behaviour, root-relative: this target has no per-process
/// working directory yet. Everything else that is not a directory descriptor
/// is `ENOTDIR`, per POSIX, rather than a silently different file.
fn resolve_at(dirfd: u64, path: alloc::string::String) -> Result<alloc::string::String, u64> {
    if path.starts_with('/') {
        return Ok(path);
    }
    const AT_FDCWD: u64 = (-100i64) as u64;
    if dirfd == AT_FDCWD {
        let mut p = alloc::string::String::from("/");
        p.push_str(&path);
        return Ok(p);
    }
    // The descriptor's *path*, then one `metadata` to say whether it names a
    // directory. That question used to be a bool on the entry; it is asked of
    // the filesystem now, for the reason the field's removal states — glue has
    // no such field and this is where it would have to come from anyway.
    //
    // Cold path: a `dirfd` open is `apk` walking `/etc/apk/keys`, not a read
    // loop, so one inode read per `openat` with a real `dirfd` is not a cost
    // worth caching a bool for.
    let base = table_with(dirfd, |d| match d {
        FileDescriptor::File(f) => Some(f.path.clone()),
        _ => None,
    })
    .flatten()
    .filter(|p| fs::metadata(p).is_ok_and(|m| m.is_dir));
    let Some(mut joined) = base else {
        return Err(errno::ENOTDIR);
    };
    if !joined.ends_with('/') {
        joined.push('/');
    }
    joined.push_str(&path);
    Ok(joined)
}

/// `openat(dirfd, path, flags, mode)`.
///
/// Absolute paths and `AT_FDCWD` resolve from the root; a relative path
/// resolves against the directory `dirfd` names ([`resolve_at`]). That last
/// case is not a nicety: `apk` loads every signing key with
/// `openat(keys_dirfd, name)` after listing that directory, and while `dirfd`
/// was ignored each such open landed on a root-relative name that does not
/// exist — zero keys loaded, and every fetched index reported `UNTRUSTED
/// signature` no matter how correct the fetch and the keys were.
pub fn sys_openat(dirfd: u64, path: u64, flags_: u64, _mode: u64) -> u64 {
    // **The flag word arrives in the x86_64 encoding and is re-encoded here,
    // once** — the same hop `Syscall::from_x86_64` makes for the syscall
    // number, one argument along. aarch64 Linux keeps the 32-bit ARM fcntl
    // values, so four bits are *permuted* between the two architectures
    // (`O_DIRECTORY`↔`O_DIRECT`, `O_NOFOLLOW`↔`O_LARGEFILE`); every other `O_*`
    // bit is identical, which is exactly why this file used to say the two
    // encodings "happen to share the same numeric encoding" and carry three
    // separate `_X86` constants for the ones that do not.
    //
    // Everything below this line — and everything a folded `akuma-syscalls-glue`
    // arm will read out of `KernelFile::flags` — is therefore asm-generic, the
    // one encoding every shared crate in this tree speaks. See
    // `akuma_syscalls_abi::open_flags`, whose tests pin the trap this closes:
    // an untranslated x86_64 `O_TMPFILE` slips straight through glue's refusal.
    let flags_ = u64::from(akuma_syscalls_abi::open_flags::x86_64_to_aarch64(
        flags_ as u32,
    ));
    let Some(path) = path_from_user(path) else {
        return errno::EFAULT;
    };

    // `/proc/<pid>/fd/0` — `sshd`'s bridge opens this to feed a spawned shell's
    // stdin. It is the only procfs path this target answers; everything else
    // under /proc is ENOENT.
    if let Some(rest) = path.strip_prefix("/proc/") {
        if let Some(pid_str) = rest.strip_suffix("/fd/0") {
            let Ok(pid) = pid_str.parse::<u32>() else {
                return errno::ENOENT;
            };
            let Some(pipe_id) = crate::usermode::stdin_pipe_for_pid(pid) else {
                return errno::ENOENT;
            };
            return alloc_pipe_fd(pipe_id, true).unwrap_or(errno::EMFILE);
        }
        // Everything else under /proc: the live process table and the
        // system-wide virtual files. A path this view does not serve falls
        // through to the mounted `ProcFilesystem` — see `sys_newfstatat`.
        if let Some(r) = open_proc(rest, flags_) {
            return r;
        }
    }
    // `/proc` itself, with no trailing slash — the path `ps` and `top` open to
    // enumerate processes. The `strip_prefix("/proc/")` above cannot match it,
    // and without this it fell through to the real, empty ext2 directory.
    if path == "/proc"
        && let Some(r) = open_proc("", flags_)
    {
        return r;
    }

    let Ok(normalised) = resolve_at(dirfd, path) else {
        return errno::ENOTDIR;
    };
    // Follow symlinks, which `open(2)` does unless `O_NOFOLLOW` says otherwise
    // and which this target could not do at all until `akuma-vfs-glue` arrived
    // (C1 step 4a). `readlink` already worked — `readlinkat` calls
    // `fs::read_symlink` directly — so the gap was one-sided and silent:
    // `ln -s` created a link `readlink` could describe and `cat` reported
    // `ENOENT` for, because `read_file` on the link inode is `NotAFile`.
    //
    // Deliberately here and not in `resolve_at`: that helper also serves
    // `symlinkat`, `readlinkat` and `unlinkat`, and every one of those operates
    // on the link itself. Following there would make `rm` delete the target.
    //
    // `O_NOFOLLOW` is honoured by skipping the walk rather than by failing with
    // `ELOOP` on a link, which is the weaker half of the flag: this target has
    // no `O_PATH` and nothing that opens a link to inspect it. Stated so the
    // divergence is pinned rather than assumed absent.
    let normalised = if flags_ & u64::from(open_flags::O_NOFOLLOW) == 0 {
        fs::resolve_symlinks(&normalised)
    } else {
        normalised
    };
    // `O_TMPFILE` is answered with `EINVAL`, as Linux kernels without tmpfile
    // support do — and it is tested against the **asm-generic** mask, because
    // the word was re-encoded at the top of this function. Read straight off
    // the wire it would not have matched: x86_64 spells the flag `0o20200000`
    // and aarch64 `0o20040000`, they share only `__O_TMPFILE`, and
    // `flags & mask == mask` is therefore false for an x86_64 caller who
    // asked for exactly this. That is the failure
    // `akuma_syscalls_abi::open_flags`'s tests pin, and the reason a folded
    // `sys_openat` could not have inherited this guard for free. This used to be *missing*,
    // which repeated the aarch64 `APK_OTMPFILE_DIR_FD.md` bug bit for bit:
    // apk-tools 3 opens its atomic-write temp file with
    // `openat(dfd, ".", O_RDWR|O_TMPFILE|O_CLOEXEC)`, the open succeeded as a
    // writable descriptor on the *directory*, apk wrote the whole downloaded
    // index into it (the kernel buffered the bytes, `close` skipped the
    // `is_dir` persistence), and the reopen-for-verify then had nothing to
    // verify — surfacing as `UNTRUSTED signature` over a fetch that was fine.
    // Portable callers (apk-tools 3's `__apk_ostream_to_file`) treat any
    // failure here as "no tmpfiles" and fall back to `.tmp.<pid>` +
    // `renameat`, which works.
    let tmpfile = u64::from(open_flags::O_TMPFILE);
    if flags_ & tmpfile == tmpfile {
        return errno::EINVAL;
    }

    // `O_CREAT`: a new (or truncated) file, written back at `close(2)` — see
    // [`sys_close`] and this module's header. Skips `read_file` entirely
    // rather than reading-then-discarding an existing file's bytes: this
    // target always truncates on `O_CREAT` (there is no in-place update path,
    // and tcc — the first real writer here — always asks for
    // `O_WRONLY|O_CREAT|O_TRUNC` together), so starting from an empty buffer
    // is correct for the one case this target's callers actually use.
    // Existing-directory check first: `O_CREAT` on a path that is already a
    // directory must fail, not silently start writing a same-named file.
    let creating = flags_ & u64::from(open_flags::O_CREAT) != 0;
    if creating && fs::metadata(&normalised).is_ok_and(|m| m.is_dir) {
        return errno::EISDIR;
    }    // A directory has no bytes to cache as file contents — `read_file` would
    // fail it as `NotAFile`. Check `metadata` first (one inode read, next to
    // `read_file`'s whole-file copy) so a directory opens successfully instead
    // of falling through to "not found"; `getdents64` lists it straight off
    // the disk on first use. Write-mode opens of a directory are refused at
    // `open(2)` time (Linux `may_open`'s answer) rather than handed out as a
    // descriptor whose every meaningful write is a lie — the second half of
    // the `O_TMPFILE` lesson above, and the same guard the aarch64 kernel
    // shipped for it.
    let is_dir = !creating && fs::metadata(&normalised).is_ok_and(|m| m.is_dir);
    if is_dir && flags_ & u64::from(open_flags::O_ACCMODE) != 0 {
        return errno::EISDIR;
    }
    // **C2 SLICE 5: THE CACHE DIES HERE.** This path only ever reaches a real
    // ext2 file (synthetic views are answered above it, by `open_proc` and the
    // synthetic installers), so the descriptor is born with an **empty**
    // buffer: `read` goes to `fs::read_at` and `write` to `write_at` at the
    // cursor, in `MAX_IO`-bounded chunks, and nothing holds the file's bytes
    // in the kernel heap for the descriptor's lifetime. That is the whole-file
    // heap bug's root cause, deleted rather than guarded.
    //
    // What replaced "read the bytes, and `ENOENT` if that fails" is a
    // **one-byte** `read_at` probe: it answers existence through the same
    // byte path the descriptor will use, which `metadata` alone cannot. One
    // byte, not zero: a zero-length `read_at` succeeds before the path is ever
    // resolved (a short-circuit that is correct for `read(2)` and fatal for a
    // probe — the first Firecracker boot of this slice opened missing files
    // and gave them fds).
    //
    // **The device arm below runs before it**, and that ordering is the whole
    // reason it is written out here. A `/dev` node has no byte path at all, so
    // the probe answers "absent" for every one of them; behind the probe,
    // `open("/dev/zero")` was `ENOENT` and `open("/dev/null", O_CREAT)` — which
    // skips the probe's guard — went on to try to *create* a node. Two
    // different wrong answers to the same question, from one ordering.

    // **The `/dev` character nodes.** `/dev` on this target is synthetic —
    // `akuma_vfs_glue::dev_node` answers `ls -la /dev`, `stat` and `getdents`
    // for it — but nothing had ever wired a *descriptor* to one, so every
    // `open` under `/dev` fell through to the ext2 path below, where `/dev` is
    // not a directory and the node is not a file.
    //
    // That fall-through was wrong in two different eras, and the second is the
    // one that made this urgent. Measured over ssh against the committed
    // kernel, not inferred:
    //
    // - **before C2 slice 5**, `> /dev/null` opened a descriptor with an empty
    //   buffer, buffered every byte written to it, and dropped them at `close`
    //   when the whole-file persist failed — a bit bucket by accident, with a
    //   `[close] persist failed` console line per use;
    // - **after slice 5** the writes go straight to `write_at`, which cannot
    //   resolve `/dev`, so `echo hi > /dev/null` answers
    //   `sh: write error: No such file or directory` and exits 1. `ls >
    //   /dev/null` still *looks* fine only because busybox `ls` swallows its
    //   write error — which is how a broken `/dev/null` survived a suite and a
    //   ring-3 check that both run `>/dev/null` on every line.
    //
    // Reading has been broken longer than either: `cat /dev/null` is `ENOENT`,
    // because the existence probe below asks for a byte a node has none of.
    //
    // None of these is `/dev/null`. It is the most-used special file in any shell
    // script and this target reaches it on `ls > /dev/null` alone, so it is
    // served here for real: the descriptor carries the node's path, and
    // [`dev_node_of`] is what `read`, `write`, `lseek` and `fstat` ask instead
    // of the VFS. A node this does not serve — the block devices — is
    // `ENODEV` rather than a fall-through, which is the same "say so" answer
    // the aarch64 kernel gives for `open("/dev/vda")`.
    if let Some(node) = akuma_vfs_glue::dev_node(&normalised) {
        if node.is_block {
            return errno::ENODEV;
        }
        let file = KernelFile::new(normalised, flags_ as u32);
        return install(FileDescriptor::File(file));
    }
    let mut probe = [0u8; 1];
    let exists = fs::read_at(&normalised, 0, &mut probe).is_ok();
    if !creating && !is_dir && !exists {
        return errno::ENOENT;
    }
    // `O_DIRECTORY` — the other half of the `is_dir` answer above, and the one
    // this file could not express before the word was re-encoded at the top.
    //
    // The flag was **never read at all**: it had no entry in the local
    // `open_flags` module, and `is_dir` is decided by `fs::metadata`, so
    // `open("/bin/busybox", O_RDONLY|O_DIRECTORY)` handed back a working
    // descriptor on a regular file. It could not simply have been added
    // either — the bit ring 3 sets is `0o200000`, which in the encoding every
    // shared crate here uses is `O_DIRECT`, so reading it with the tree's own
    // constant would have tested the wrong bit, and reading it with a fourth
    // inline `_X86` const would have deepened the split this pass removes.
    //
    // **Below the existence probe, not above it**, and the first draft had it
    // above: `open("/no/such/path", O_DIRECTORY)` must be `ENOENT`, because
    // "there is no such file" outranks "and it would not have been a
    // directory". Placed early it answered `ENOTDIR` for every missing path
    // that carried the flag — which is the sort of thing that sends a caller
    // looking for a directory it never asked about. The check's own negative
    // control is what found it, by returning `ENOENT` from the arm that was
    // supposed to be the broken one.
    if !is_dir && flags_ & u64::from(open_flags::O_DIRECTORY) != 0 {
        return errno::ENOTDIR;
    }
    let truncating = flags_ & u64::from(open_flags::O_TRUNC) != 0;
    let appending = flags_ & u64::from(open_flags::O_APPEND) != 0;
    // `O_CREAT | O_EXCL` on a path that already exists is `EEXIST`, which is
    // the *whole* contract of `O_EXCL`: it is how a caller claims a lock file
    // or an atomic temp name, and succeeding anyway tells two of them they
    // both won. Asked here rather than left out, because the probe above has
    // already paid for the existence answer.
    if creating && flags_ & u64::from(open_flags::O_EXCL) != 0 && (exists || is_dir) {
        return errno::EEXIST;
    }
    // **`O_TRUNC` truncates and `O_CREAT` creates — here, once, through the
    // VFS**, and both used to do neither.
    //
    // The line this replaces was `write_at(path, 0, &[])`, and `write_at`'s
    // very first statement is `if data.is_empty() { return Ok(0) }` — *before*
    // it resolves the path, creates a missing inode or touches a length. So it
    // reported success and did nothing, twice over:
    //
    // - `echo x > f` over a 31-byte `f` left `x\nAAAAAAAA…` — 31 bytes, the
    //   tail of the old contents behind the new head. Silent corruption, and
    //   invisible to the boot suite, whose `redirect` checks only ever write
    //   to a file that did not exist yet.
    // - `: > f`, `2> err` for a command that prints no errors, and any other
    //   zero-length create made **no file at all**: nothing wrote bytes, so
    //   nothing ever reached the filesystem.
    //
    // This is the *same short-circuit* slice 5 already paid for in the other
    // direction — its existence probe was zero-length and `read_at` answers
    // `Ok` for that before resolving anything, so every missing file opened
    // successfully. One byte fixed the read; the write needs an API that does
    // not have the short-circuit at all. `write_file(path, &[])` is that:
    // existing → `truncate_inode` then write nothing, missing → allocate the
    // inode and add the directory entry. It is a whole-file write of **zero
    // bytes**, so it costs the heap nothing and reintroduces no cache.
    //
    // A failure is a real refusal (a read-only mount, a missing parent) and
    // fails the open, which is what Linux answers too.
    if !is_dir
        && (truncating || (creating && !exists))
        && let Err(e) = fs::write_file(&normalised, &[])
    {
        return fs_err_errno(e);
    }
    // `O_APPEND`'s starting cursor is the file's real size — one `metadata`,
    // not "the length of what we happened to read". **After** the truncate
    // above, not before: `O_APPEND | O_TRUNC` together must start at 0, and
    // reading the size first would have started at the old end.
    let start_pos = if appending {
        fs::metadata(&normalised).map_or(0, |m| m.size)
    } else {
        0
    };

    // `KernelFile::new` leaves the inode 0 — "read by path", which is what
    // this target does — and the position 0, which `O_APPEND` overrides.
    //
    // **Divergence, pinned, and now the only one left in append:** real
    // `O_APPEND` re-seeks to the end before *every* write, so two processes
    // appending to one file interleave whole records. Here it only sets the
    // starting cursor. What changed with this slice is the *loss mode*: the
    // old design already lost on concurrent appenders (each descriptor held
    // its private copy and wrote the whole file back at close); now the
    // writes go through the VFS as they happen, so appenders interleave at
    // record granularity when they re-seek, and at cursor granularity when
    // they do not — never the whole-file clobber.
    let mut file = KernelFile::new(normalised, flags_ as u32);
    file.position = start_pos as usize;
    install(FileDescriptor::File(file))
}

/// The `open(2)` flag bits, **asm-generic encoding** — the tree's own table,
/// not a local copy.
///
/// This used to be five constants declared here, under a comment reasoning that
/// pulling them from `akuma_syscalls_linux` would be wrong because "those are
/// the AArch64/`asm-generic` values" and "on x86_64 they happen to share the
/// same numeric encoding". The first half was right and the second was **false
/// for four bits** — aarch64 keeps the 32-bit ARM fcntl values, so
/// `O_DIRECTORY`, `O_NOFOLLOW`, `O_DIRECT` and `O_LARGEFILE` are a permutation
/// between the two architectures rather than a shared encoding. The file knew
/// it in three places, as `O_NOFOLLOW_X86`, `O_TMPFILE_X86` and `O_EXCL_X86`
/// declared inline where they were needed, which is how a coincidence survives
/// as a stated fact: each of those sites was correct on its own and none of
/// them contradicted the comment out loud.
///
/// [`sys_openat`] now re-encodes the whole word once at its own boundary
/// (`akuma_syscalls_abi::open_flags::x86_64_to_aarch64`), so every reader below
/// it — this module, `KernelFile::flags`, and whatever
/// `akuma-syscalls-glue` arm folds next — speaks one encoding.
pub use akuma_syscalls_linux::flags::open as open_flags;

/// `utimensat(dirfd, path, times, flags)` — x86_64 280. `times` is two
/// `struct timespec` (atime, mtime); the `UTIME_NOW`/`UTIME_OMIT` sentinel
/// nanosecond values map to "the wall clock" / "leave alone". A NULL `times`
/// sets both to now. First consumer: `apk`'s post-extract mtime preservation.
pub fn sys_utimensat(dirfd: u64, path: u64, times: u64, _flags: u64) -> u64 {
    const UTIME_NOW: i64 = 0x3fff_ffff;
    const UTIME_OMIT: i64 = 0x3fff_fffe;

    let Some(raw) = path_from_user(path) else {
        return errno::EFAULT;
    };
    let Ok(path) = resolve_at(dirfd, raw) else {
        return errno::ENOTDIR;
    };

    // Wall-clock seconds for UTIME_NOW; None until a boot without SNTP sync
    // would make "now" a lie, which `clock::utc_seconds()`'s own contract
    // already refuses to do.
    let now = crate::clock::utc_seconds();
    let decode = |spec: (i64, i64)| -> Option<Option<u64>> {
        match spec.1 {
            UTIME_OMIT => Some(None),
            UTIME_NOW => Some(Some(now.unwrap_or(0))),
            n if n < 0 => None,
            secs => Some(Some(secs.max(0) as u64)),
        }
    };
    let (atime, mtime) = if times == 0 {
        let n = Some(now.unwrap_or(0));
        (n, n)
    } else {
        // SAFETY: user array of two `struct timespec` { i64, i64 }: atime at
        // +0, mtime at +16.
        let read_ts = |off: u64| -> Option<(i64, i64)> {
            crate::uaccess::read_val::<[i64; 2]>(times + off).map(<(i64, i64)>::from)
        };
        let (Some(raw_a), Some(raw_m)) = (read_ts(0), read_ts(16)) else {
            return errno::EFAULT;
        };
        let Some(atime) = decode(raw_a) else {
            return errno::EINVAL;
        };
        let Some(mtime) = decode(raw_m) else {
            return errno::EINVAL;
        };
        (atime, mtime)
    };

    match fs::set_times(&path, atime, mtime) {
        Ok(()) => 0,
        Err(e) => fs_err_errno(e),
    }
}

/// `dup(fd)` — x86_64 32. A new descriptor for an already-open one.
///
/// The first consumer is `apk`'s signature-verification I/O setup — it dups a
/// just-reopened index fd, then closes the original, exactly as the aarch64
/// bring-up found (`APK_MISSING_SYSCALLS.md` "dup (23)": without the syscall
/// apk gave up and reported `UNTRUSTED signature` over a fetch that was in
/// fact fine). Same bug, different syscall number.
///
/// **The flip re-pins the value-copy divergence.** Between C2 slice 4 and the
/// step 4b flip this was the "real thing": one more name for one shared
/// description, a shared cursor, a release only when the last name went. The
/// description's state now lives in the table's own `KernelFile`, and a copy
/// of a `KernelFile` clones by value — so the new descriptor carries a
/// *snapshot* of the cursor, not a shared one. That is
/// `akuma-syscalls-glue`'s `dup` on AArch64, bit for bit, and the AArch64
/// kernel self-hosts on it; fixing it honestly means an `Arc` inside
/// `FileDescriptor::File` in `akuma-exec-core`, which is a change to the
/// shared type with its own pass behind it. Pinned by the self-test that
/// used to assert the opposite.
pub fn sys_dup(fd: u64) -> u64 {
    let t = cur_table();
    let Some(desc) = table_get(fd) else {
        return errno::EBADF;
    };
    let newfd = t.alloc_fd_from(FIRST_FILE_FD as u32, desc.clone());
    if newfd as usize >= MAX_FDS {
        t.table.lock().remove(&newfd);
        return errno::EMFILE;
    }
    // Bumped only once the new name exists: a reference before a failed
    // `alloc` would strand the pipe/socket at a count no `close` can reach.
    clone_refs(&desc);
    u64::from(newfd)
}

/// `dup2(oldfd, newfd)` — x86_64 33 — and `dup3`, which is the same with a
/// flags word.
///
/// **This is what makes shell redirection work**, and until 2026-09-06 it did
/// not exist. `sh` implements `prog > file` as `open(file) -> 3`,
/// `dup2(3, 1)`, `close(3)`: the whole mechanism is the ability to make fd 1
/// name something else. Descriptors 0/1/2 were not in the table at all — they
/// were routed by number below it — so there was nowhere for the new name to
/// land, and `echo x > file` and `cmd | cmd` both failed with `ENOSYS` in a
/// way that looked like a missing syscall rather than a missing table entry.
///
/// POSIX subtleties, both of which real shells depend on:
/// - `oldfd == newfd` returns `newfd` **without closing it**, and is not an
///   error. `dup3` differs here and returns `EINVAL`, which is the only
///   behavioural difference between the two.
/// - `newfd` is closed first if it was open, and that close is silent — its
///   errors are not reported, because the caller is asking about `oldfd`.
pub fn sys_dup2(oldfd: u64, newfd: u64) -> u64 {
    dup_onto(oldfd, newfd, false)
}

/// `dup3(oldfd, newfd, flags)` — x86_64 292. `O_CLOEXEC` is accepted and
/// ignored, as `fcntl(F_SETFD)` is; see [`sys_fcntl`].
pub fn sys_dup3(oldfd: u64, newfd: u64, _flags: u64) -> u64 {
    dup_onto(oldfd, newfd, true)
}

fn dup_onto(oldfd: u64, newfd: u64, strict_same: bool) -> u64 {
    let Some(desc) = table_get(oldfd) else {
        return errno::EBADF;
    };
    if oldfd == newfd {
        return if strict_same { errno::EINVAL } else { newfd };
    }
    let Some(new_idx) = u32::try_from(newfd).ok().filter(|f| (*f as usize) < MAX_FDS) else {
        return errno::EBADF;
    };

    // Install the new name and take out whatever it displaced, in one hold, so
    // no window exists in which `newfd` names nothing. Then bump, then release
    // the displaced reference outside the lock.
    let displaced = {
        let mut t = cur_table().table.lock();
        t.insert(new_idx, desc.clone())
    };
    clone_refs(&desc);
    if let Some(old) = displaced {
        release_desc(&old);
    }
    newfd
}

/// `pipe2(fds, flags)` — x86_64 293 — and `pipe(fds)`, which is `pipe2` with
/// no flags.
///
/// Writes the read end to `fds[0]` and the write end to `fds[1]`, as two
/// `int`s. Both are ordinary descriptors in the calling process's row, so
/// `fork` inherits them and `dup2` can move them onto 0/1/2 — which together
/// are a shell pipeline.
///
/// The pipe is freed when its **last** end closes — `close_read` and
/// `close_write` each drop one of the two reference counts `pipe::alloc` starts
/// it with, and the second of them destroys it. A shell closes the ends in
/// whichever order its bookkeeping reaches them, so getting that rule wrong
/// frees the buffer under a live writer; it used to be a hand-rolled `ends`
/// counter here and is now `akuma-pipes`' own.
pub fn sys_pipe2(fds: u64, _flags: u64) -> u64 {
    let Some(id) = crate::pipe::alloc() else {
        return errno::ENFILE;
    };
    // The two installs **consume** the references `pipe::alloc` started each
    // end with — the descriptor is the pipe's first name, not a copy of one.
    let read_fd = install(FileDescriptor::PipeRead(id as u32));
    if errno::is_err(read_fd) {
        crate::pipe::free(id);
        return read_fd;
    }
    let write_fd = install(FileDescriptor::PipeWrite(id as u32));
    if errno::is_err(write_fd) {
        sys_close(read_fd);
        crate::pipe::free(id);
        return write_fd;
    }

    // Written last: a partial copy must not leave the caller holding two
    // descriptors it does not know about.
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&(read_fd as u32).to_ne_bytes());
    out[4..].copy_from_slice(&(write_fd as u32).to_ne_bytes());
    if errno::is_err(copy_to_user(fds, &out)) {
        sys_close(read_fd);
        sys_close(write_fd);
        return errno::EFAULT;
    }
    0
}

/// `close(fd)`. Closing a console descriptor succeeds and does nothing — a
/// program that closes stdin should not then find the kernel refusing to print.
pub fn sys_close(fd: u64) -> u64 {
    let t = cur_table();
    // An *unbound* console descriptor: closing succeeds and does nothing, so a
    // program that closes stdin does not then find the kernel refusing to
    // print. A **bound** one has been redirected and is a real descriptor —
    // `sh` does `dup2(f,1); close(f)` and later `close(1)`, and that last close
    // has to reach the file or its buffered contents are never persisted.
    if fd >= MAX_FDS as u64 {
        return errno::EBADF;
    }
    // An *unbound* 0/1/2 is the console: `close` succeeds and does nothing, so
    // a program that closes stdin does not then find the kernel refusing to
    // print. Any other absent fd is a genuine `EBADF` — closing twice is one.
    if fd < FIRST_FILE_FD as u64 && !is_bound(fd) {
        t.nonblock.lock().remove(&(fd as u32));
        t.cloexec.lock().remove(&(fd as u32));
        return 0;
    }
    let Some(desc) = t.table.lock().remove(&(fd as u32)) else {
        return errno::EBADF;
    };
    t.nonblock.lock().remove(&(fd as u32));
    t.cloexec.lock().remove(&(fd as u32));
    // The name is gone; now give back the one reference this table entry held.
    // Releasing reaches into the network stack and the pipe table, so it runs
    // outside the table lock.
    release_desc(&desc);
    0
}

/// Serve a read from the `/dev` character node `node`.
///
/// Each of these is a *rule*, not a file: there is no inode behind the path,
/// which is exactly why the ext2 read path answers `EIO` for all of them.
/// Shared by [`sys_read`] and [`sys_pread64`] — the offset makes no difference
/// to any of the four, which is the other half of why they are not files.
fn dev_read(node: &str, buf: u64, len: u64) -> u64 {
    match node {
        // The bit bucket reads as an empty file — `read` returns 0, i.e. EOF.
        "null" => 0,
        "zero" => {
            let zeros = alloc::vec![0u8; len as usize];
            copy_to_user(buf, &zeros)
        }
        // The same entropy `getrandom(2)` gets (`net::rng_fill_checked`, via
        // `akuma_primitives::rng`). Both `/dev/random` and `/dev/urandom`, and
        // deliberately not distinguished: this target has one source and no
        // entropy accounting to block on, so pretending `random` is the
        // blocking one would be a fiction with a hang in it.
        "random" | "urandom" => {
            let mut bytes = alloc::vec![0u8; len as usize];
            // `None` (no source registered) and `Some(false)` (the source
            // failed) are both `EIO` here, and the distinction the crate keeps
            // is not lost by that: it exists so `getrandom(2)` can fall back to
            // a virtio device, and this target registers its source in
            // `boot.rs` unconditionally. Handing ring 3 the zeroed buffer
            // instead would be an entropy source that is not one.
            if akuma_primitives::rng::fill_bytes(&mut bytes) != Some(true) {
                return errno::EIO;
            }
            copy_to_user(buf, &bytes)
        }
        "tty" => read_console(buf, len as usize),
        // Every node this target opens is one of the above: `sys_openat`
        // refuses the block devices with `ENODEV` and there are no others in
        // the table. A new one arriving there and not here would read as an
        // empty file, so answer `EIO` instead — "this kernel cannot tell you"
        // rather than a confident nothing.
        _ => errno::EIO,
    }
}

/// `read(fd, buf, len)`.
///
/// fd 0 is the console and **blocks**: it spins on the UART until a byte
/// arrives. That is what makes an interactive shell possible on a target with no
/// device interrupts, and it is also why nothing else can run while a prompt
/// waits — the honest cost of polling, and the thing an IOAPIC would fix.
pub fn sys_read(fd: u64, buf: u64, len: u64) -> u64 {
    if len == 0 {
        return 0;
    }
    // Clamped, not refused. `read(2)` on real Linux accepts an oversized
    // count and just does a short read (or reads up to its own internal cap,
    // ~2 GiB) — it is not an error for the caller to ask for more than is
    // available or than this kernel wants to service in one call. Rejecting
    // the whole call with `EINVAL` used to be this function's answer, and it
    // broke `apk`: its I/O layer reads local files through a fixed
    // (128 KiB-class) buffer regardless of the file's real size — completely
    // ordinary POSIX usage — so a 119-byte `/etc/apk/repositories` came back
    // "Invalid argument" before `apk update` ever got as far as opening a
    // socket. The file-read path below already clamps to what is actually
    // available (`total.saturating_sub(pos).min(len)`); this clamp is what
    // lets a request past `MAX_IO` reach that logic instead of being refused
    // outright.
    let len = len.min(MAX_IO);

    // **The row is consulted before the by-number defaults**, so a redirected
    // descriptor wins. `sh -c 'prog < file'` is `open(file); dup2(fd, 0)`, and
    // reading fd 0 must then reach the file rather than the console. For an
    // *unbound* 0/1/2 nothing here matches and the console path below runs,
    // exactly as it did before descriptors 0/1/2 could be bound at all.

    // A socket descriptor routes to the network stack. The lock is dropped
    // first: `socket_recv` blocks, and holding the descriptor table across a
    // blocking wait would stop every other task from opening a file.
    if let Some(sock) = socket_index(fd) {
        return crate::sock::recv(sock, buf, len, is_nonblocking(fd));
    }

    // A pipe descriptor — the parent's read end of a spawned child's stdout,
    // or either end of a `pipe(2)` pair.
    if let Some(pid) = pipe_read_id(fd) {
        return read_pipe(pid, buf, len as usize, is_nonblocking(fd));
    }

    if fd < FIRST_FILE_FD as u64 && !is_bound(fd) {
        if fd == 0 {
            // An unbound fd 0 is the console, full stop. It used to also ask
            // `current_stdin_pipe()` — the `Spawn` row — because a spawned
            // child's stdin was a pipe reached *by number*; since C2 slice 6 it
            // is a `PipeRead` descriptor and the branch above has already
            // routed it. Only a task with no stdin descriptor at all (init on
            // the serial line, and the boot suite's kernel row) reaches here.
            return read_console(buf, len as usize);
        }
        return errno::EBADF;
    }

    // A `/dev` character node.
    if let Some(node) = dev_node_of(fd) {
        return dev_read(node, buf, len);
    }

    // Resolve under the lock; do the I/O outside it. **No directory guard
    // here.** A `read` on a directory descriptor reaches `fs::read_at`, which
    // answers `NotAFile`, which [`fs_err_errno`] maps to `EISDIR` — the same
    // errno by the same route `akuma-syscalls-glue` uses, and the reason the
    // entry no longer carries an `is_dir` bool. The common path pays nothing:
    // the check that went away only ever fired on the error.
    //
    // A **synthetic** `/proc` view is re-rendered per read (step 4b) — the
    // mounted `ProcFilesystem`'s semantics, which is what this target's file
    // surface is converging on; the render-at-open snapshot the old
    // `Entry::data` cache gave was that field's whole reason to exist, and it
    // went with the field.
    let resolved = table_with(fd, |d| {
        let FileDescriptor::File(f) = d else {
            return Err(errno::EBADF);
        };
        Ok((f.path.clone(), f.position))
    });
    let (path, pos) = match resolved {
        Some(Ok(v)) => v,
        Some(Err(e)) => return e,
        None => return errno::EBADF,
    };
    let proc_rest = proc_rest_of(&path);
    if let Some(rest) = proc_rest {
        let Some(data) = render_proc_file(rest) else {
            return 0;
        };
        let n = data.len().saturating_sub(pos).min(len as usize);
        if n == 0 {
            return 0;
        }
        let r = copy_to_user(buf, &data[pos..pos + n]);
        if !errno::is_err(r) {
            table_with(fd, |d| {
                if let FileDescriptor::File(f) = d {
                    f.position = pos + n;
                }
            });
        }
        return r;
    }
    // Real path: one bounded VFS read, then the position moves.
    let mut kbuf = alloc::vec![0u8; len as usize];
    // **The error is mapped, not flattened.** `Err(_) => EIO` stood here, and
    // it is what made `read` on a directory descriptor answer `EIO` the moment
    // the entry stopped carrying an `is_dir` bool to refuse it earlier: ext2
    // says `NotAFile`, [`fs_err_errno`] says `EISDIR`, and this line threw
    // both away. Caught by a ring-3 probe, not by the suite.
    let n = match fs::read_at(&path, pos, &mut kbuf) {
        Ok(n) => n,
        Err(e) => return fs_err_errno(e),
    };
    if n == 0 {
        return 0;
    }
    table_with(fd, |d| {
        if let FileDescriptor::File(f) = d {
            f.position = pos + n;
        }
    });
    copy_to_user(buf, &kbuf[..n])
}

/// `pread64(fd, buf, count, offset)` — x86_64 syscall 17.
///
/// A read from an explicit offset that **does not move the descriptor's
/// cursor**. That is the whole difference from [`sys_read`], and it is the
/// reason the two cannot share a body: `sys_read` mutates `file.position` and
/// every caller of `pread` is relying on it not to.
///
/// # Why this exists
///
/// It was not dispatched at all until 2026-09-07, so every `pread` on this
/// target returned `ENOSYS`. `scripts/mem_suite.py`'s `mmapsum` is what found
/// it — its `read()` reference arm is a `pread` loop and it aborted at offset 0
/// before comparing anything (`docs/archive/AKUMA_AMD64_MEMORY_GAPS.md` §1) —
/// but the probe is only the messenger. `pread` is how every archive reader,
/// every `rustc` metadata load and every threaded reader of a shared
/// description reaches into a file, precisely because it needs no lock around a
/// seek-then-read pair.
///
/// # The errnos, and why `ESPIPE` is not `EBADF`
///
/// A pipe, socket or console descriptor has no offset to read from, and Linux
/// says `ESPIPE` for that — a *seekability* answer, distinct from "no such
/// descriptor". Answering `EBADF` instead would tell a caller its fd was closed
/// when it is open and perfectly readable, which is the wrong-errno failure this
/// document's §2 is about in a second place: musl's `FILE` layer falls back to
/// `read()` on `ESPIPE` and gives up on `EBADF`.
///
/// A negative `offset` is `EINVAL`. It arrives as a `u64` from a ring-3
/// register, so the sign has to be recovered before it is used as an index —
/// `0xFFFF_FFFF_FFFF_FFFF` as a `usize` offset would otherwise sail past every
/// bound check by being larger than any file.
pub fn sys_pread64(fd: u64, buf: u64, len: u64, offset: u64) -> u64 {
    // Signed on the wire. Recovered before anything indexes with it.
    let off = offset.cast_signed();
    if off < 0 {
        return errno::EINVAL;
    }
    if len == 0 {
        return 0;
    }
    // Clamped, not refused — the same rule and the same reason as `sys_read`:
    // an oversized count is ordinary POSIX and a short read is the contract.
    let len = len.min(MAX_IO);

    // Not seekable: a socket, a pipe, or an unbound 0/1/2 (the console).
    // Checked ahead of the table lookup so the answer is about the *kind* of
    // descriptor rather than about whether a file happens to sit behind it.
    if socket_index(fd).is_some() || pipe_read_id(fd).is_some() || pipe_write_id(fd).is_some() {
        return errno::ESPIPE;
    }
    if fd < FIRST_FILE_FD as u64 && !is_bound(fd) {
        return errno::ESPIPE;
    }
    // A `/dev` character node **is** seekable — `pread` on `/dev/zero` is
    // ordinary — and the offset changes nothing for any of the four, so this
    // is the same answer `sys_read` gives.
    if let Some(node) = dev_node_of(fd) {
        return dev_read(node, buf, len);
    }

    let resolved = table_with(fd, |d| {
        let FileDescriptor::File(f) = d else {
            return Err(errno::EBADF);
        };
        // **`position` is deliberately not touched.** That is the entire
        // contract of this call.
        Ok(f.path.clone())
    });
    let path = match resolved {
        Some(Ok(v)) => v,
        Some(Err(e)) => return e,
        None => return errno::EBADF,
    };
    let proc_rest = proc_rest_of(&path);
    if let Some(rest) = proc_rest {
        // A mapped synthetic view: rendered fresh at the offset asked for.
        let Some(data) = render_proc_file(rest) else {
            return 0;
        };
        let n = data.len().saturating_sub(off as usize).min(len as usize);
        return copy_to_user(buf, &data[off as usize..off as usize + n]);
    }
    let mut kbuf = alloc::vec![0u8; len as usize];
    let n = match fs::read_at(&path, off as usize, &mut kbuf) {
        Ok(n) => n,
        Err(e) => return fs_err_errno(e),
    };
    copy_to_user(buf, &kbuf[..n])
}

/// Copy `dst.len()` bytes of `fd`'s cached contents starting at byte `offset`
/// into `dst`, returning how many were actually available.
///
/// The backing for `mmap(MAP_PRIVATE, fd)` — see `mm::sys_mmap`. `None` means
/// `fd` is not a regular file, which is the one case a file mapping must refuse
/// rather than serve.
///
/// Bytes past the end of the file are **not written**, so the caller's buffer
/// keeps whatever it had there. Every caller hands in a freshly zeroed page,
/// which is what makes that the right split: a mapping that extends past EOF
/// reads as zeros, exactly as `mmap(2)` specifies for the partial last page.
///
/// The descriptor's bytes at `offset`, for `mmap(MAP_PRIVATE, fd)` — see
/// `mm::sys_mmap`. `None` means `fd` is not a regular file, which is the one
/// case a file mapping must refuse rather than serve.
///
/// Bytes past the end of the file are **not written**, so the caller's buffer
/// keeps whatever it had there. Every caller hands in a freshly zeroed page,
/// which is what makes that the right split: a mapping that extends past EOF
/// reads as zeros, exactly as `mmap(2)` specifies for the partial last page.
///
/// A synthetic `/proc` view is rendered fresh (step 4b); a real file's page
/// comes off the VFS.
pub fn file_bytes_at(fd: u64, offset: usize, dst: &mut [u8]) -> Option<usize> {
    // A directory needs no guard of its own: `fs::read_at` below refuses one
    // (`NotAFile`), and this function's contract is already `None` for
    // anything a `MAP_PRIVATE` file mapping must not be served from.
    let path = table_with(fd, |d| match d {
        FileDescriptor::File(f) => Some(f.path.clone()),
        _ => None,
    })??;
    let proc_rest = proc_rest_of(&path);
    if let Some(rest) = proc_rest {
        // A mapped synthetic view: rendered fresh at the offset asked for.
        let data = render_proc_file(rest)?;
        let n = data.len().saturating_sub(offset).min(dst.len());
        dst[..n].copy_from_slice(&data[offset..offset + n]);
        return Some(n);
    }
    // Real file: the page comes off the VFS. A read error fills nothing —
    // the caller's freshly zeroed page shows through, which is the same
    // answer "past EOF" gets, and the only kind thing a fault path can do
    // with an I/O error anyway.
    fs::read_at(&path, offset, dst).ok()
}

/// Is `fd` a regular file — something `mmap` can back a mapping with?
///
/// Separate from [`file_bytes_at`] because `mmap` has to refuse a socket, a pipe
/// or a directory **before** it places a region, and a zero-byte answer from the
/// copy is not the same thing as "this cannot be mapped".
#[must_use]
pub fn is_regular_file(fd: u64) -> bool {
    // A `/dev` node is a `KernelFile` here too, and it is **not** mappable:
    // `file_bytes_at` would ask `fs::read_at` for bytes the node has no inode
    // to hold, and a file mapping served as zeros is the one failure
    // `sys_mmap`'s file arm exists to refuse.
    if dev_node_of(fd).is_some() {
        return false;
    }
    // "Not a directory", asked of the path. A synthetic `/proc` *file* stays
    // mappable exactly as it was — [`file_bytes_at`] serves it from a fresh
    // render — and a synthetic `/proc` directory is refused.
    table_with(fd, |d| match d {
        FileDescriptor::File(f) => Some(f.path.clone()),
        _ => None,
    })
    .flatten()
    .is_some_and(|p| !path_is_dir(&p))
}

/// `write(fd, buf, len)` on a real file descriptor — everything `sys_write` in
/// `usermode.rs` does not itself handle (console, pipe, socket).
///
/// Writes through the VFS at the descriptor's cursor, in [`MAX_IO`]-bounded
/// chunks, with the disk I/O outside the table lock (C2 slice 5). A
/// **synthetic** `/proc` view has no inode behind the path to write through;
/// since step 4b there is no cached render to write into either, so the write
/// is accepted and dropped — the same observable answer the old
/// write-the-cache-then-fail-the-persist path produced, minus the console
/// error line. Nothing on this target writes a `/proc` file.
pub fn sys_write_file(fd: u64, buf: u64, len: u64) -> u64 {
    if len == 0 {
        return 0;
    }
    // Copied in per-`MAX_IO` chunk inside the loop: `copy_in` allocates, and
    // refusing the whole call when `len` exceeds one chunk (the old
    // `EINVAL`) was wrong twice over — real Linux accepts any `write(2)`
    // length (short writes are the contract), and `apk` exercises that
    // directly: it buffers a whole downloaded APKINDEX (hundreds of KB) and
    // writes it to its cache file in one call. The EINVAL it got back was
    // reported as `updating and opening ...: Invalid argument` and killed
    // every fetch, 2026-09-04.

    // A `/dev` character node, before any of the file machinery: none of these
    // has an inode to write through, and the ext2 path would answer `EIO` for
    // all of them. `/dev/null` accepting every byte and keeping none is the
    // whole point of it; `zero` and the entropy nodes are sinks too, which is
    // what Linux does. `tty` is the console this target already writes to.
    match dev_node_of(fd) {
        Some("null" | "zero" | "random" | "urandom") => return len,
        Some("tty") => {
            let mut chunk = [0u8; 256];
            let mut done = 0u64;
            while done < len {
                let n = ((len - done) as usize).min(chunk.len());
                if !crate::uaccess::read_bytes(buf + done, &mut chunk[..n]) {
                    return if done == 0 { errno::EFAULT } else { done };
                }
                for &byte in &chunk[..n] {
                    serial::putb(byte);
                }
                done += n as u64;
            }
            return len;
        }
        _ => {}
    }

    let mut written: usize = 0;
    while written < len as usize {
        let chunk_len = ((len as usize) - written).min(MAX_IO as usize);
        // Copied in before the lock: there is no reason to hold the table
        // across a user copy.
        let Some(incoming) = copy_in(buf + written as u64, chunk_len as u64) else {
            return if written == 0 { errno::EFAULT } else { written as u64 };
        };
        // Resolve under the lock; the write itself goes through the VFS
        // outside it (`write_at` at the cursor — the disk never belongs under
        // the table lock), and only the cursor move comes back.
        // The directory guard that stood here was unreachable and is gone: a
        // write-mode open of a directory is refused at `open(2)`
        // (`sys_openat`'s `EISDIR`), so a directory descriptor is always
        // read-only and the `writable` test below is what turns it away.
        let resolved = table_with(fd, |d| {
            let FileDescriptor::File(f) = d else {
                return Err(errno::EBADF); // not a file
            };
            if f.flags & open_flags::O_ACCMODE == 0 {
                return Err(errno::EBADF); // opened read-only
            }
            Ok((f.path.clone(), f.position))
        });
        let (path, pos) = match resolved {
            Some(Ok(v)) => v,
            Some(Err(e)) => return e,
            None => return errno::EBADF,
        };
        let proc_rest = proc_rest_of(&path);
        let step = if proc_rest.is_some() {
            // Synthetic: accepted and dropped — see this function's header.
            Ok(incoming.len())
        } else {
            match akuma_vfs_glue::write_at(&path, pos, &incoming) {
                // Short/partial writes: `write_at` returns what it placed,
                // and the cursor moves by that — the caller retries the rest,
                // which is `write(2)`'s contract.
                Ok(n) => {
                    table_with(fd, |d| {
                        if let FileDescriptor::File(f) = d {
                            f.position = pos + n;
                        }
                    });
                    Ok(n)
                }
                Err(e) => Err(fs_err_errno(e)),
            }
        };
        match step {
            Ok(0) => {
                // A zero-byte placement means the write is not progressing
                // (out of space at this cursor); report what was written so
                // far, or `ENOSPC` if that is nothing.
                return if written == 0 { errno::ENOSPC } else { written as u64 };
            }
            Ok(n) => written += n,
            Err(e) => return if written == 0 { e } else { written as u64 },
        }
    }
    len
}

/// Akuma's own `poll_input_event(buf, len, timeout_us)` — a **raw** keystroke.
///
/// Not the same path as `read(0)`, and the difference is the point.
/// `read(0)` goes through `akuma-terminal`'s canonical mode: the kernel buffers
/// a line, handles backspace, echoes, and returns when Enter is pressed. This
/// returns single bytes as they arrive, unechoed, because its caller wants to do
/// its own line editing — `paws`'s `read_line` handles backspace, Ctrl+D and
/// echo itself, which is exactly what a shell with history and completion has to
/// do.
///
/// Both are legitimate and the terminal crate has `enter_raw_mode` for precisely
/// this split. Serving `poll_input_event` from the canonical path would make a
/// shell wait for a whole line before it could echo the first character.
///
/// Blocks until a byte arrives. `timeout_us` is accepted and ignored: honouring
/// it needs a clock read in the wait loop, and every caller today passes
/// `u64::MAX` (wait forever). Ignoring a finite timeout would be wrong, so it is
/// recorded here rather than silently treated as infinite — the first caller
/// that passes one is the one that has to implement it.
pub fn sys_poll_input_event(buf: u64, len: u64, _timeout_us: u64) -> u64 {
    if len == 0 {
        return 0;
    }
    // A spawned child (an interactive `sshd` shell) reads its keystrokes from
    // its stdin pipe, which `sshd` feeds from the SSH channel — not the UART.
    // Yield while waiting so `sshd` and the netpoll daemon keep running.
    //
    // Asked of **fd 0's descriptor** since C2 slice 6, not of the `Spawn` row:
    // this syscall is `read`-shaped and must follow the same redirection every
    // other read does, so a `paws` whose stdin was replaced reads what it was
    // given rather than what it was spawned with.
    if let Some(pipe_id) = pipe_read_id(0) {
        let mut one = [0u8; 1];
        loop {
            match crate::pipe::read(pipe_id, &mut one) {
                Some(0) => return 0, // EOF — the client closed the channel
                Some(_) => return copy_to_user(buf, &one),
                None => {
                    // A park, not a yield: an interactive shell waiting on a
                    // keystroke is the longest wait in this kernel and used to
                    // be its busiest loop.
                    if !crate::pipe::check_set_reader(pipe_id) {
                        crate::sched::block_current();
                    }
                }
            }
        }
    }
    loop {
        if let Some(b) = crate::input::getb() {
            return copy_to_user(buf, &[b]);
        }
        // A yield, not a bare spin: this task holds the Big Kernel Lock, and a
        // shell waiting for a keypress must not hold every other core's syscalls
        // hostage. `yield_now` drops the lock for a moment when it has nothing
        // to switch to.
        crate::sched::yield_now();
    }
}

/// Read from the console, through the line discipline.
///
/// Polls the UART, feeds each byte to `process_canon_input`, writes back
/// whatever it says to echo, and returns a line once one is ready. Blocking:
/// this target takes no device interrupts, so there is nothing else for the CPU
/// to do while a prompt waits — the honest cost of polling, and what an IOAPIC
/// would fix.
///
/// Ctrl+D on an empty line returns 0, which is EOF. A reader that treated that
/// as an error would never terminate.
fn read_console(buf: u64, len: usize) -> u64 {
    loop {
        // Anything the discipline already has, first: a previous call may have
        // delivered two lines' worth of bytes in one burst.
        {
            let mut guard = CONSOLE.lock();
            let Some(term) = guard.as_mut() else {
                return 0;
            };
            let ready = term.drain_canon_ready(len);
            if !ready.is_empty() {
                return copy_to_user(buf, &ready);
            }
        }

        let Some(byte) = crate::input::getb() else {
            // A yield, not a spin: this task holds the Big Kernel Lock, and a
            // shell waiting for a key must not hold every other core's
            // syscalls hostage (`sched::yield_now` drops the lock briefly).
            crate::sched::yield_now();
            continue;
        };

        let (echo, eof) = {
            let mut guard = CONSOLE.lock();
            let Some(term) = guard.as_mut() else {
                return 0;
            };
            // CR -> NL before the discipline sees it. A serial terminal sends CR
            // for Enter; canonical mode ends a line on NL.
            let mut one = [byte];
            term.map_cr_to_nl(&mut one);
            let processed = term.process_canon_input(&one);
            (processed.echo, processed.eof)
        };
        for b in echo {
            serial::putb(b);
        }
        if eof {
            return 0;
        }
    }
}

/// `lseek(fd, offset, whence)`.
pub fn sys_lseek(fd: u64, offset: u64, whence: u64) -> u64 {
    const SEEK_SET: u64 = 0;
    const SEEK_CUR: u64 = 1;
    const SEEK_END: u64 = 2;

    // An *unbound* 0/1/2 is the console, which has no offset. A **bound** one
    // has been redirected and is whatever it now names — since C2 slice 6 that
    // includes a spawned child's stdio pipes, so the guard has to ask rather
    // than assume, exactly as `sys_read` and `sys_write` do.
    if fd < FIRST_FILE_FD as u64 && !is_bound(fd) {
        return errno::EBADF;
    }
    let Some(desc) = table_get(fd) else {
        return errno::EBADF;
    };
    // A pipe or a socket is not seekable, and Linux says so with `ESPIPE`. It
    // used to be `EBADF` here by accident of the bound-0/1/2 guard above, which
    // tells a caller its descriptor is closed when it is open and perfectly
    // writable — the same wrong-errno failure `sys_pread64`'s header describes,
    // and one musl's `FILE` layer reads as fatal rather than as "unseekable".
    if pipe_read_id(fd).is_some() || pipe_write_id(fd).is_some() || socket_index(fd).is_some() {
        return errno::ESPIPE;
    }
    // A `/dev` character node **is** seekable on Linux and every seek lands at
    // the offset asked for — `/dev/null` and `/dev/zero` are infinite and
    // contentless, so no position is out of range and none of them means
    // anything. Answered as a zero-length file (`SEEK_END` → 0) rather than
    // sent through `metadata`, which has no size to give for a node with no
    // inode and would fail the seek with `EIO`.
    //
    // The cursor stays at 0 for all three whences because nothing moves it:
    // the read and write arms above never touch `file.position` for a node.
    if dev_node_of(fd).is_some() {
        if !matches!(whence, SEEK_SET | SEEK_CUR | SEEK_END) {
            return errno::EINVAL;
        }
        let delta = offset.cast_signed();
        return if delta < 0 { errno::EINVAL } else { delta as u64 };
    }
    let FileDescriptor::File(f) = &desc else {
        return errno::EBADF;
    };
    let (proc_rest, path, position) = (proc_rest_of(&f.path), f.path.clone(), f.position);
    // `SEEK_END` needs the size: synthetic from a fresh render, real from the
    // VFS. One `metadata` outside the lock for the real case — the rule
    // `release_desc` states about disk I/O under the table lock. A directory
    // descriptor takes the real branch rather than being excluded: Linux
    // permits `lseek` on one, and `metadata` answers its size like any other
    // inode. The old exclusion left `total` at 0, so `SEEK_END` on a directory
    // silently meant `SEEK_SET(0)`.
    let total = if let Some(rest) = proc_rest {
        render_proc_file(rest).map_or(0, |d| d.len())
    } else {
        match fs::metadata(&path) {
            Ok(m) => m.size as usize,
            Err(e) => return fs_err_errno(e),
        }
    };
    // `offset` is signed on the wire; a negative seek from SEEK_CUR/SEEK_END is
    // legal and must not be read as an enormous unsigned value.
    let delta = offset.cast_signed();
    let base = match whence {
        SEEK_SET => 0i64,
        SEEK_CUR => i64::try_from(position).unwrap_or(i64::MAX),
        SEEK_END => i64::try_from(total).unwrap_or(i64::MAX),
        _ => return errno::EINVAL,
    };
    let Some(target) = base.checked_add(delta) else {
        return errno::EINVAL;
    };
    if target < 0 {
        return errno::EINVAL;
    }
    // Seeking past the end is legal; reading there returns 0. The table's
    // cursor is the only cursor — the mirror-sync block the flip deleted wrote
    // the same number into a second structure.
    table_with(fd, |d| {
        if let FileDescriptor::File(f) = d {
            f.position = target as usize;
        }
    });
    target as u64
}

/// `getdents64(fd, dirp, count)` — x86_64 217. `ls` and `find` both need this;
/// until now `openat` on a directory returned `ENOENT` (`read_file` fails it as
/// `NotAFile`) and this syscall did not exist at all.
///
/// Reuses `KernelFile::dir_cache` — the same field and the same reason the
/// AArch64 kernel has one: a directory that changes between two calls must not
/// shift the caller's cursor mid-walk, so the listing is snapshotted on first
/// use and `position` (otherwise a byte offset into cached file contents, see
/// [`sys_read`]) is reused as an entry index for a directory descriptor.
///
/// The wire record — offsets, the 8-byte `d_reclen` rounding, the NUL and the
/// pad — is `akuma_syscalls_linux::dirent`, not hand-rolled: that module's own
/// header explains why `size_of::<Header>()` is the wrong offset to reach for.
/// `d_ino`/`d_off` are both 1, matching the AArch64 kernel — this target
/// reports no real inode number through `getdents64` either, and nothing seeks
/// a directory by `d_off`.
///
/// Separate table holds rather than one held across the call: the cache-miss
/// path calls `fs::list_dir`, which takes the *other* lock (`fs::ROOT`), and
/// nothing else in this module nests the two — see [`sys_openat`], which
/// reads the file before ever touching the table.
pub fn sys_getdents64(fd: u64, dirp: u64, count: u64) -> u64 {
    if count == 0 {
        return 0;
    }

    let resolved = table_with(fd, |d| {
        let FileDescriptor::File(f) = d else {
            return None;
        };
        Some((f.path.clone(), f.dir_cache.clone()))
    })
    .flatten();
    let Some((path, cached)) = resolved else {
        return errno::EBADF;
    };

    let entries = if let Some(c) = cached {
        c
    } else {
        // `ENOTDIR` for a non-directory comes from here now, not from a bool
        // on the entry: `list_dir` answers `NotADirectory` and
        // [`fs_err_errno`] maps it. The blanket `ENOENT` this replaces was
        // wrong for that case in the way that misdirects — "no such
        // directory" for a path that plainly exists.
        let dir_entries = match fs::list_dir(&path) {
            Ok(e) => e,
            Err(e) => return fs_err_errno(e),
        };
        let cache: Vec<akuma_exec_core::process::DirCacheEntry> = dir_entries
            .iter()
            .map(|e| akuma_exec_core::process::DirCacheEntry {
                name: e.name.clone(),
                d_type: if e.is_dir {
                    4 // DT_DIR
                } else if e.is_symlink {
                    10 // DT_LNK
                } else {
                    8 // DT_REG
                },
            })
            .collect();
        table_with(fd, |d| {
            if let FileDescriptor::File(f) = d {
                f.dir_cache = Some(cache.clone());
            }
        });
        cache
    };

    let position = table_with(fd, |d| match d {
        FileDescriptor::File(f) => f.position,
        _ => 0,
    })
    .unwrap_or(0);
    if position >= entries.len() {
        return 0;
    }

    let count = (count as usize).min(MAX_IO as usize);
    let mut kernel_buf = alloc::vec![0u8; count];
    let mut written = 0usize;
    let mut consumed = 0usize;
    for e in entries.iter().skip(position) {
        let reclen = akuma_syscalls_linux::dirent::reclen(e.name.len());
        if written + reclen > count {
            break;
        }
        let ok = akuma_syscalls_linux::dirent::encode(
            &mut kernel_buf[written..written + reclen],
            1,
            1,
            e.d_type,
            e.name.as_bytes(),
        );
        debug_assert!(ok, "dirent::reclen and dirent::encode disagree on the record size");
        if !ok {
            break;
        }
        written += reclen;
        consumed += 1;
    }
    table_with(fd, |d| {
        if let FileDescriptor::File(f) = d {
            f.position += consumed;
        }
    });

    if written > 0 {
        let r = copy_to_user(dirp, &kernel_buf[..written]);
        if errno::is_err(r) {
            return r;
        }
    }
    written as u64
}

/// The x86_64 `struct stat` — 144 bytes. **Not** the aarch64 layout: x86_64
/// puts `st_nlink` at 16 (8 bytes) and `st_mode` at 24, where `asm-generic`
/// has `st_mode` at 16 and `st_nlink` at 20. This is why `akuma-syscalls-linux`'s
/// `Stat` cannot be reused here — its field offsets are the other architecture's
/// (proposal item 5 territory).
const STAT_SIZE: usize = 144;

/// `S_IFREG | 0644` — a plain file whose real mode the target does not track for
/// an already-open fd (`KernelFile` here carries no inode).
const S_IFREG_0644: u32 = 0o100_644;
/// `S_IFCHR | 0620`, for the console descriptors.
const S_IFCHR_0620: u32 = 0o020_620;
/// `S_IFDIR | 0755`, for a directory descriptor.
const S_IFDIR_0755: u32 = 0o040_755;
/// `S_IFREG | 0444`, for a `/proc` render — read-only, which is what it is.
///
/// The same mode `sys_newfstatat` reports for the same path, so `stat` and
/// `fstat` agree on a `/proc` file. Before the `is_dir` removal every
/// synthetic view was reported as a zero-length **directory** by `fstat`,
/// including `/proc/meminfo`.
const S_IFREG_0444: u32 = 0o100_444;
/// `S_IFIFO | 0600`, for a pipe descriptor.
///
/// Needed from C2 slice 6, which is when a spawned child first *had* one at
/// fd 1: `entry.file()` is `None` for a pipe, so the size/path arm below fell
/// straight through to `EBADF` — a program that `fstat`s its own stdout (musl's
/// stdio does, and so does every `test -p`) would have been told the descriptor
/// it is holding does not exist.
const S_IFIFO_0600: u32 = 0o010_600;
/// `S_IFSOCK | 0600`, for a socket descriptor. Same reason as [`S_IFIFO_0600`],
/// one variant along.
const S_IFSOCK_0600: u32 = 0o140_600;

/// Serialise a `struct stat` at the x86_64 field offsets. The fields this target
/// can answer are filled; the rest stay zero rather than invented — a caller
/// that reads `st_dev` gets 0, which is wrong but is not a plausible-looking lie.
fn encode_stat(
    mode: u32,
    size: u64,
    ino: u64,
    nlink: u64,
    atime: Option<u64>,
    mtime: Option<u64>,
    ctime: Option<u64>,
) -> [u8; STAT_SIZE] {
    const ST_INO: usize = 8;
    const ST_NLINK: usize = 16;
    const ST_MODE: usize = 24;
    const ST_SIZE: usize = 48;
    const ST_BLKSIZE: usize = 56;
    const ST_BLOCKS: usize = 64;
    const ST_ATIME: usize = 72;
    const ST_MTIME: usize = 88;
    const ST_CTIME: usize = 104;

    let mut st = [0u8; STAT_SIZE];
    st[ST_INO..ST_INO + 8].copy_from_slice(&ino.to_le_bytes());
    st[ST_NLINK..ST_NLINK + 8].copy_from_slice(&nlink.to_le_bytes());
    st[ST_MODE..ST_MODE + 4].copy_from_slice(&mode.to_le_bytes());
    st[ST_SIZE..ST_SIZE + 8].copy_from_slice(&size.to_le_bytes());
    st[ST_BLKSIZE..ST_BLKSIZE + 8].copy_from_slice(&4096u64.to_le_bytes());
    st[ST_BLOCKS..ST_BLOCKS + 8].copy_from_slice(&size.div_ceil(512).to_le_bytes());
    st[ST_ATIME..ST_ATIME + 8].copy_from_slice(&atime.unwrap_or(0).to_le_bytes());
    st[ST_MTIME..ST_MTIME + 8].copy_from_slice(&mtime.unwrap_or(0).to_le_bytes());
    st[ST_CTIME..ST_CTIME + 8].copy_from_slice(&ctime.unwrap_or(0).to_le_bytes());
    st
}

/// `fstat(fd, statbuf)` — `struct stat` for an already-open descriptor.
///
/// The size comes from a fresh render for a synthetic view and from the VFS
/// for a real file; the mode is a fixed `S_IFREG | 0644` because a
/// `KernelFile` on this target carries no inode to read a real one from. A
/// console descriptor reports `S_IFCHR`, a directory descriptor `S_IFDIR` —
/// musl's `fdopendir` fstats the fd and refuses it with `ENOTDIR` unless
/// `S_ISDIR` holds, so `ls`/`find` need this to be right, not just `openat`
/// succeeding.
pub fn sys_fstat(fd: u64, statbuf: u64) -> u64 {
    let (mode, size, nlink) = if fd < FIRST_FILE_FD as u64 && !is_bound(fd) {
        (S_IFCHR_0620, 0u64, 1u64)
    } else if pipe_read_id(fd).is_some() || pipe_write_id(fd).is_some() {
        // C2 slice 6: a spawned child's fd 0/1/2 are pipes, and a pipe has no
        // `KernelFile` for the arm below to take a path out of.
        (S_IFIFO_0600, 0u64, 1u64)
    } else if socket_index(fd).is_some() {
        (S_IFSOCK_0600, 0u64, 1u64)
    } else if dev_node_of(fd).is_some() {
        // A `/dev` character node: `S_IFCHR`, size 0 — the same answer
        // `sys_newfstatat` already gives for the same path, which is what
        // keeps `stat file` and `fstat(open(file))` agreeing.
        (S_IFCHR_0620, 0u64, 1u64)
    } else {
        // Size and shape: synthetic from a fresh render, real from the VFS —
        // one `metadata` outside the lock, the rule `release_desc` states
        // about disk I/O under the table lock.
        //
        // # This arm answered `EBADF` for every real directory descriptor
        //
        // It opened `if entry.is_dir { return None }`, and `None` here is
        // `EBADF`. The `S_IFDIR` arm below it was never reached by an ext2
        // directory at all — its `true` is *synthetic*, not *directory*, so
        // the only descriptors ever reported as directories were `/proc`
        // views, **including `/proc/meminfo`**, which was told it was a
        // zero-length directory.
        //
        // Measured from ring 3 (`/probes/dirprobe`, x86_64 musl) before the
        // fix, which is the only reason it was found — this file's own header
        // says the opposite in prose:
        //
        // ```
        // open(/etc, O_DIRECTORY) = 3 (ok)
        // fstat(dirfd) = -1 errno=9(Bad file descriptor) mode=00 S_ISDIR=0
        // fdopendir(dirfd) = NULL (Bad file descriptor)
        // ```
        //
        // busybox never noticed because it walks with `opendir(path)` and
        // `lstat`, not `fdopendir` — so `ls`, `find` and `ls -R` all work over
        // a path that this call cannot describe. `fdopendir` is what `nftw`
        // and most every `openat`-based directory walker are built on, which
        // is the shape a self-hosting build reaches for.
        //
        // Both halves are answered from the source that knows: a synthetic
        // view asks [`proc_metadata`] (the same answer `newfstatat` gives for
        // the same path, so `stat` and `fstat` agree), and a real file asks
        // the VFS for `is_dir` alongside the size it was already fetching.
        let resolved = table_with(fd, |d| match d {
            FileDescriptor::File(f) => Some(f.path.clone()),
            _ => None,
        })
        .flatten();
        let Some(path) = resolved else {
            return errno::EBADF;
        };
        if path_is_dir(&path) {
            (S_IFDIR_0755, 0u64, 2u64)
        } else if let Some(rest) = proc_rest_of(&path) {
            match proc_metadata(rest) {
                // A `/proc` render: read-only, and its size is the bytes this
                // descriptor will actually serve — the same answer
                // `sys_newfstatat` gives for the same path, which is what
                // keeps `stat` and `fstat` agreeing.
                Some((size, false)) => (S_IFREG_0444, size, 1u64),
                _ => (S_IFREG_0444, 0u64, 1u64),
            }
        } else {
            match fs::metadata(&path) {
                Ok(m) => (S_IFREG_0644, m.size, 1u64),
                // The path resolved at `open` and does not now — an unlinked
                // file, which this target has no inode pin for. Reported as
                // the regular file it was rather than as a bad descriptor:
                // the fd is perfectly valid, and `EBADF` would send a caller
                // looking at its own bookkeeping.
                Err(_) => (S_IFREG_0644, 0u64, 1u64),
            }
        }
    };
    let st = encode_stat(mode, size, 0, nlink, None, None, None);
    if errno::is_err(copy_to_user(statbuf, &st)) {
        return errno::EFAULT;
    }
    0
}

/// `statfs` magic numbers, keyed by `Filesystem::name()` — the same table the
/// AArch64 kernel keeps in `akuma-syscalls-glue`. `df` prints the mount's type
/// from `/proc/mounts`, but anything reading `f_type` (a libc `fstatfs`, a
/// build system checking for tmpfs) wants the real constant.
const fn fs_magic(name_is_ext2: bool) -> i64 {
    if name_is_ext2 { 0xEF53 } else { 0xADF5 }
}

/// Fill a user `struct statfs` from whichever mount serves `path`.
///
/// The layout is `asm-generic`'s, identical on x86_64 and aarch64, so the
/// `Statfs` in `akuma-syscalls-linux` — whose 120-byte size and three field
/// offsets are `const`-asserted there — is shared rather than re-declared. A
/// re-declaration is exactly how the aarch64 side once shipped a 120 nothing
/// could check.
fn statfs_into(path: &str, buf: u64) -> u64 {
    let view = match fs::stats_for_path(path) {
        Ok(v) => v,
        Err(akuma_vfs::FsError::NotFound) => return errno::ENOENT,
        Err(_) => return errno::ENOSYS,
    };
    let (name, stats, flags) = (view.fs_name, view.stats, view.flags);
    let bs = i64::from(stats.block_size);
    // Saturating rather than `as`: `struct statfs` is signed and these are not,
    // and a wrapped block count would make `df` print a negative size instead
    // of an implausible one. No filesystem reaches the clamp; the point is that
    // the conversion says what it does.
    let blocks = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    let st = akuma_syscalls_linux::Statfs {
        f_type: fs_magic(name == "ext2"),
        f_bsize: bs,
        f_blocks: blocks(stats.total_blocks),
        f_bfree: blocks(stats.free_blocks),
        // No reservation for root on this target, so available == free. Saying
        // otherwise would make `df` report a Use% that never reaches 100.
        f_bavail: blocks(stats.free_blocks),
        // `akuma-ext2`'s `FsStats` carries no inode counts. Zero is what Linux
        // reports for a filesystem with no fixed inode table, and `df -i` shows
        // it as such rather than inventing a number.
        f_files: 0,
        f_ffree: 0,
        f_fsid: [0, 0],
        f_namelen: 255,
        f_frsize: bs,
        f_flags: i64::try_from(flags).unwrap_or(0),
        f_spare: [0; 4],
    };
    // `write_val` rather than a hand-rolled byte view: `Statfs` is `repr(C)`
    // plain data, which is exactly the `T` that helper takes, and it already
    // owns the one `unsafe` this needs.
    if crate::uaccess::write_val(buf, st) { 0 } else { errno::EFAULT }
}

/// `statfs(path, buf)` — x86_64 137. `busybox df` calls this for every line it
/// read out of `/proc/mounts`; without it `df` printed its header and stopped.
pub fn sys_statfs(path: u64, buf: u64) -> u64 {
    let Some(path) = path_from_user(path) else {
        return errno::EFAULT;
    };
    // `AT_FDCWD` is `resolve_at`'s own local const; `statfs(2)` takes no dirfd,
    // so pass the value that means "resolve from the root" explicitly.
    const AT_FDCWD: u64 = (-100i64) as u64;
    let Ok(normalised) = resolve_at(AT_FDCWD, path) else {
        return errno::ENOTDIR;
    };
    statfs_into(&normalised, buf)
}

/// `fstatfs(fd, buf)` — x86_64 138. Reports the mount serving the path the fd
/// was opened on; a descriptor with no path (a socket, a pipe, a stdio fd)
/// reports the root mount, which is what Linux does for an fd on a filesystem
/// with no name to resolve.
pub fn sys_fstatfs(fd: u64, buf: u64) -> u64 {
    let path = if fd < FIRST_FILE_FD as u64 && !is_bound(fd) {
        alloc::string::String::from("/")
    } else {
        let Some(path) = table_with(fd, |d| match d {
            FileDescriptor::File(f) => f.path.clone(),
            _ => alloc::string::String::from("/"),
        }) else {
            return errno::EBADF;
        };
        path
    };
    statfs_into(&path, buf)
}

/// `newfstatat(dirfd, path, statbuf, flags)` — and, by the two thin shims in
/// `syscall_dispatch`, the x86-only `stat(2)` and `lstat(2)`.
///
/// `dirfd` is honoured only for `AT_EMPTY_PATH` (stat the fd itself, the
/// `fstat` form busybox uses to size a file it just opened); every other path
/// is resolved from the root, exactly as [`sys_openat`] does it, because this
/// target has no per-process working directory.
///
/// The mode, size, inode and timestamps come straight from `akuma-ext2`'s inode
/// metadata — the `type_perms` field already *is* a Linux `st_mode` (type bits
/// plus permissions), so `S_ISDIR` / `S_ISREG` / the executable bit a shell
/// checks before running a PATH entry all come through unmodified.
///
/// Symlinks: this target's ext2 path walk does not follow them and the rootfs
/// is built with hard links rather than symlinks, so `AT_SYMLINK_NOFOLLOW` is
/// accepted and makes no difference — `metadata` reports the link's own inode
/// either way. When a symlink farm appears, following belongs in `fs.rs` where
/// `openat` would need it too, not here.
pub fn sys_newfstatat(dirfd: u64, path: u64, statbuf: u64, flags: u64) -> u64 {
    /// `fstatat`'s "operate on `dirfd` itself when the path is empty" bit.
    const AT_EMPTY_PATH: u64 = 0x1000;

    let Some(path) = path_from_user(path) else {
        return errno::EFAULT;
    };

    if path.is_empty() {
        if flags & AT_EMPTY_PATH != 0 {
            return sys_fstat(dirfd, statbuf);
        }
        return errno::ENOENT;
    }

    let normalised = if path.starts_with('/') {
        path
    } else {
        let mut p = alloc::string::String::from("/");
        p.push_str(&path);
        p
    };

    // `/proc` paths have no inode. `ps` calls `stat` on `/proc/<pid>` to read
    // the owning uid for its USER column, and `top` `stat`s `/proc` itself
    // before it will start.
    let proc_rest = if normalised == "/proc" {
        Some("")
    } else {
        normalised.strip_prefix("/proc/")
    };
    // A `/proc` path this kernel's own synthetic view does not serve is no
    // longer `ENOENT`: since 5b slice 3 the real `ProcFilesystem` is mounted
    // at `/proc`, and it serves files this one never did (`uptime`,
    // `loadavg`, the system-wide `stat`, per-pid `mounts`, the `fd`
    // directory). Falling through to the normal path walk is what lets the
    // union show through, and it keeps the three answers consistent by
    // construction: open, stat and access all fall through at the same point.
    if let Some(rest) = proc_rest
        && let Some((size, is_dir)) = proc_metadata(rest)
    {
        // 0o40555 / 0o100444: root-owned, world-readable, never writable.
        let mode = if is_dir { 0o040_555 } else { 0o100_444 };
        let st = encode_stat(mode, size, 1, if is_dir { 2 } else { 1 }, None, None, None);
        if errno::is_err(copy_to_user(statbuf, &st)) {
            return errno::EFAULT;
        }
        return 0;
    }

    let Ok(meta) = fs::metadata(&normalised) else {
        return errno::ENOENT;
    };
    let st = encode_stat(
        meta.mode,
        meta.size,
        meta.inode,
        if meta.is_dir { 2 } else { 1 },
        meta.accessed,
        meta.modified,
        meta.created,
    );
    if errno::is_err(copy_to_user(statbuf, &st)) {
        return errno::EFAULT;
    }
    0
}

/// `poll(fds, nfds, timeout_ms)` — x86_64 syscall 7.
///
/// Enough of `poll` for an interactive `busybox sh`: its line editor calls
/// `poll(&stdin, 1, -1)` on every keystroke, and an `ENOSYS` there sent it into
/// a tight retry loop printing `sh: poll: Function not implemented` forever.
///
/// Readiness is real for the fds a shell actually polls — a stdin/stdout pipe
/// (checked non-destructively) and the console — and optimistic (always ready)
/// for a regular file, which POSIX allows. A UDP socket fd is real too (see
/// [`poll_ready`]) — musl's stub DNS resolver polls one waiting for a reply. A
/// TCP socket fd still reports **not** ready: nothing on this target polls a
/// stream socket yet, and a false `POLLIN` would send the caller into a
/// blocking `recv`. `POLLNVAL` is not distinguished from "not ready".
///
/// Timeout: `< 0` waits indefinitely (yield-and-retry, so `sshd` and the
/// netpoll daemon keep running); `0` is one non-blocking pass; `> 0` is
/// approximated by a bounded yield budget — this target has no calibrated
/// clock, and the finite-timeout polls a shell makes are short escape-sequence
/// disambiguations that tolerate the imprecision.
pub fn sys_poll(fds: u64, nfds: u64, timeout_ms: u64) -> u64 {
    const POLLIN: u16 = 0x001;
    const POLLOUT: u16 = 0x004;
    const MAX_NFDS: u64 = 64;

    if nfds == 0 {
        // A bare `poll(NULL, 0, ms)` is a sleep; with no clock, yield once.
        crate::sched::yield_now();
        return 0;
    }
    if fds == 0 || nfds > MAX_NFDS {
        return errno::EINVAL;
    }
    let n = nfds as usize;
    let timeout = timeout_ms as i32;

    // Budget for a finite timeout: bounded so a stuck poll cannot wedge the
    // task. Infinite (`< 0`) loops until something is ready.
    let mut budget: u64 = match timeout {
        0 => 1,
        t if t < 0 => u64::MAX,
        t => (t as u64).saturating_mul(200).min(2_000_000),
    };

    loop {
        let mut ready = 0u64;
        for i in 0..n {
            let base = fds + (i as u64) * 8;
            // A `struct pollfd` (8 bytes): fd at +0, events at +4, revents at +6.
            let (Some(fd), Some(events)) =
                (crate::uaccess::read_val::<i32>(base), crate::uaccess::read_val::<u16>(base + 4))
            else {
                return errno::EFAULT;
            };
            let mut revents = 0u16;
            if fd >= 0 {
                let (r, w) = poll_ready(fd as u64);
                if r && events & POLLIN != 0 {
                    revents |= POLLIN;
                }
                if w && events & POLLOUT != 0 {
                    revents |= POLLOUT;
                }
            }
            if !crate::uaccess::write_val::<u16>(base + 6, revents) {
                return errno::EFAULT;
            }
            if revents != 0 {
                ready += 1;
            }
        }
        if ready != 0 {
            return ready;
        }
        budget -= 1;
        if budget == 0 {
            return 0;
        }
        crate::sched::yield_now();
    }
}

/// `select(nfds, readfds, writefds, exceptfds, timeout)` — x86_64 23.
///
/// The bit arithmetic, the return-value rule (a fd ready in both directions
/// counts **twice**) and the fd-set shape are `akuma-syscalls-poll`'s — the
/// same host-tested module the AArch64 kernel's `pselect6` marshals through.
/// Hand-rolling them here was the tree's known failure mode and bought nothing:
/// this module first shipped `poll` without `select`, and `apk` — which waits
/// for post-connect socket writability through exactly this syscall (the
/// AArch64 side's own `APK_MISSING_SYSCALLS.md` records the pselect6 twin of
/// this bug) — spun `select -> ENOSYS` and wedged its TLS fetch mid-handshake.
///
/// The probes are this target's [`poll_ready`], the same readiness source
/// `sys_poll` uses. `exceptfds` is received, and **overwritten to all-zero on
/// the way out** — this kernel never reports exception conditions, and a set
/// the kernel received but did not write comes back exactly as the caller
/// passed it (the libcurl `CURL_CSELECT_ERR` bug in
/// `docs/runbooks/cargo-cannot-reach-crates-io.md`).
///
/// Timeout is `struct timeval { i64 tv_sec, i64 tv_usec }` (16 bytes); NULL
/// blocks until something is ready. Linux's "returns the remaining time in
/// the struct" behaviour is a divergence this target pins: the struct is left
/// untouched, which nothing in this tree relies on.
pub fn sys_select(nfds: u64, readfds: u64, writefds: u64, exceptfds: u64, timeout: u64) -> u64 {
    use akuma_syscalls_poll::fdset::{bytes, interests, nfds_ok, Interest, MAX_WORDS};

    const EPOLLIN: u32 = 0x001;
    const EPOLLOUT: u32 = 0x004;

    let nfds = nfds as usize;
    if !nfds_ok(nfds) {
        return errno::EINVAL;
    }
    let nb = bytes(nfds);

    // Zeroed MAX_WORDS buffers, filled only up to `nb`: `is_set` reads past
    // `nb` as clear, so the tail needs no copy.
    let mut in_read = [0u64; MAX_WORDS];
    let mut in_write = [0u64; MAX_WORDS];
    // `exceptfds` is part of the ABI even though no probe here can raise it:
    // received (so a bad pointer faults loudly at the boundary, not later),
    // then replaced with zeroes on the way back.
    let mut in_except = [0u64; MAX_WORDS];
    for (dst, src) in [
        (&mut in_read, readfds),
        (&mut in_write, writefds),
        (&mut in_except, exceptfds),
    ] {
        if src != 0 {
            let Some(v) = copy_in(src, nb as u64) else {
                return errno::EFAULT;
            };
            for (dst_w, chunk) in dst[..nb / 8].iter_mut().zip(v.as_chunks::<8>().0) {
                *dst_w = u64::from_le_bytes(*chunk);
            }
        }
    }
    let mut out_read = [0u64; MAX_WORDS];
    let mut out_write = [0u64; MAX_WORDS];

    // The timeout, decoded once. `None` = block forever.
    let deadline_budget: Option<u64> = if timeout == 0 {
        Some(1)
    } else {
        let Some([sec, usec]) = crate::uaccess::read_val::<[i64; 2]>(timeout) else {
            return errno::EFAULT;
        };
        if sec < 0 || !(0..1_000_000).contains(&usec) {
            return errno::EINVAL;
        }
        let ms = (sec as u64).saturating_mul(1000).saturating_add(usec as u64 / 1000);
        Some(ms.saturating_mul(200).clamp(1, 2_000_000))
    };

    let mut budget = deadline_budget.unwrap_or(u64::MAX);
    loop {
        let mut ready = 0u64;
        for i in interests(&in_read, &in_write, nfds) {
            let Interest { fd, in_read: r, in_write: w } = i;
            let (pr, pw) = poll_ready(fd as u64);
            let mut revents = 0u32;
            if r && pr {
                revents |= EPOLLIN;
            }
            if w && pw {
                revents |= EPOLLOUT;
            }
            ready += i.record(revents, &mut out_read, &mut out_write);
        }
        if ready != 0 {
            for (src, ptr) in [
                (&out_read, readfds),
                (&out_write, writefds),
                // `in_except` is zeroed, so `is_set` over it is always false —
                // written back all-zero, which is the overwrite rule.
                (&in_except, exceptfds),
            ] {
                if ptr != 0 {
                    let flat: Vec<u8> = src.iter().flat_map(|w| w.to_le_bytes()).collect();
                    if errno::is_err(copy_to_user(ptr, &flat[..nb])) {
                        return errno::EFAULT;
                    }
                }
            }
            return ready;
        }
        if budget == 1 {
            break;
        }
        budget -= 1;
        crate::sched::yield_now();
    }
    // Timed out: the sets come back zeroed, matching the ready path's shape.
    for ptr in [readfds, writefds, exceptfds] {
        if ptr != 0 {
            let zero = [0u8; 128];
            if errno::is_err(copy_to_user(ptr, &zero[..nb])) {
                return errno::EFAULT;
            }
        }
    }
    0
}

/// `(readable, writable)` for one fd, for [`sys_poll`]. Non-destructive.
fn poll_ready(fd: u64) -> (bool, bool) {
    // Same rule as `sys_read`: an *unbound* 0/1/2 is the console, a bound one
    // has been redirected and is described by what it now names — so the
    // console answers here are guarded, not first. The `Spawn`-row questions
    // this used to ask ("does this task have a stdin/stdout pipe?") are gone
    // with C2 slice 6: a spawned child's 0/1/2 are descriptors and fall through
    // to the pipe arms below.
    if fd < FIRST_FILE_FD as u64 && !is_bound(fd) {
        if fd == 0 {
            return (crate::input::has_byte(), false);
        }
        return (false, true);
    }
    if let Some(p) = pipe_read_id(fd) {
        return (crate::pipe::readable(p), false);
    }
    if let Some(p) = pipe_write_id(fd) {
        return (false, crate::pipe::writable(p));
    }
    if let Some(idx) = socket_index(fd) {
        // UDP: real readiness, via `akuma_net::socket::socket_udp_recv_ready`
        // — needed since musl's stub DNS resolver `sendto`s a query then
        // `poll`s the same socket for the reply, and a socket that always
        // "isn't ready" makes every reply look like a timeout no matter how
        // fast smoltcp actually receives it (`sys_sendto`'s doc has the rest
        // of that bug). TCP: real readiness both ways via `socket_tcp_ready`
        // — since `select(2)` arrived, `apk` polls a stream socket for
        // post-connect writability, and the old hard-coded `(false, false)`
        // turned every such wait into a permanent one (`sys_select`'s doc).
        if akuma_net::socket::is_udp_socket(idx) {
            return (akuma_net::socket::socket_udp_recv_ready(idx), false);
        }
        return akuma_net::socket::socket_tcp_ready(idx);
    }
    // A regular file: always ready, per POSIX.
    let in_table = is_bound(fd);
    (in_table, in_table)
}

/// `access(path)` / `faccessat(.., path, ..)` — does the path resolve?
///
/// `F_OK`/`R_OK`/`X_OK` all collapse to "it exists": one user, and no per-file
/// permission enforcement anywhere else on this target, so answering anything
/// finer would be inventing a result. `0` if it resolves, `-ENOENT` if not.
pub fn sys_access(path: u64) -> u64 {
    let Some(path) = path_from_user(path) else {
        return errno::EFAULT;
    };
    let normalised = if path.starts_with('/') {
        path
    } else {
        let mut p = alloc::string::String::from("/");
        p.push_str(&path);
        p
    };
    // `/proc` first, through the **same** `proc_metadata` `stat` uses.
    //
    // This was missing until 2026-09-07 and it made `access` and `open`
    // disagree about what exists: `/proc/self/status` opened fine, `stat`ed
    // fine, and `access(R_OK)` said `ENOENT`. `render_proc_file`'s own header
    // says one function serves `open` and `stat` "so the two can never disagree
    // about what exists" — and there was a third caller that never asked it.
    // `smapsdirty`'s `proc-self-files` sub-probe reported `stat`, `status` and
    // `cmdline` missing on a target that serves all three, which is what found
    // it (`docs/archive/AKUMA_AMD64_MEMORY_GAPS.md` §3).
    let proc_rest = if normalised == "/proc" {
        Some("")
    } else {
        normalised.strip_prefix("/proc/")
    };
    // Served here, or fall through to the mounted `ProcFilesystem` below —
    // see the matching comment in `sys_newfstatat`.
    if let Some(rest) = proc_rest
        && proc_metadata(rest).is_some()
    {
        return 0;
    }
    if fs::metadata(&normalised).is_ok() {
        0
    } else {
        errno::ENOENT
    }
}

/// `ioctl(fd, request, arg)` — the terminal subset, plus `ENOTTY` for the rest.
///
/// An interactive `busybox sh` probes its stdin with `TCGETS` on startup and, if
/// that fails, decides stdin is **not** a terminal: it prints no prompt, does no
/// line editing, and reads to EOF — which over an SSH channel looks exactly like
/// a hang. So fd 0/1/2 answer `TCGETS`/`TIOCGWINSZ` with a plausible cooked-mode
/// `termios` and an 80x24 `winsize`, and accept the setters as no-ops. There is
/// still no real line discipline on the pipe (`SPAWN_FLAG_PTY` is ignored), so
/// the shell does its own editing on raw bytes — this only stops it giving up.
///
/// Everything else, and any request on a non-console fd, stays `ENOTTY` rather
/// than `ENOSYS`: a libc asking "is this a tty?" treats `ENOTTY` as a clean no,
/// where `ENOSYS` reads as a broken kernel and some runtimes abort on it.
pub fn sys_ioctl(fd: u64, req: u64, arg: u64) -> u64 {
    // x86_64 ioctl request numbers (arch-generic for these).
    const TCGETS: u64 = 0x5401;
    const TCSETS: u64 = 0x5402;
    const TCSETSW: u64 = 0x5403;
    const TCSETSF: u64 = 0x5404;
    const TIOCGWINSZ: u64 = 0x5413;
    const TIOCSWINSZ: u64 = 0x5414;
    const TIOCGPGRP: u64 = 0x540F;
    const TIOCSPGRP: u64 = 0x5410;
    const TIOCSCTTY: u64 = 0x540E;

    // Read-only interface introspection. `busybox ifconfig` issues these on an
    // AF_INET socket fd, so they are handled before the "non-console fd →
    // ENOTTY" gate below. Shared layout with the aarch64 kernel.
    if akuma_syscalls_net::cmd::is_interface_query(req as u32) {
        return siocgif(req as u32, arg);
    }

    let is_console = fd < FIRST_FILE_FD as u64;
    if !is_console {
        return errno::ENOTTY;
    }

    match req {
        TCGETS => {
            if arg == 0 {
                return errno::EFAULT;
            }
            // Kernel `struct termios`: c_iflag/oflag/cflag/lflag (u32 each),
            // c_line (u8), c_cc[19]. 36 bytes; a couple extra do no harm.
            let mut t = [0u8; 44];
            let put = |t: &mut [u8], off: usize, v: u32| {
                t[off..off + 4].copy_from_slice(&v.to_le_bytes());
            };
            put(&mut t, 0, 0x0000_0500); // c_iflag = ICRNL | IXON
            put(&mut t, 4, 0x0000_0005); // c_oflag = OPOST | ONLCR
            put(&mut t, 8, 0x0000_00BF); // c_cflag = B38400 | CS8 | CREAD
            put(&mut t, 12, 0x0000_8A3B); // c_lflag = ISIG|ICANON|ECHO|ECHOE|ECHOK|IEXTEN
            // c_cc, the control characters that matter: VERASE, VKILL, VEOF,
            // VINTR, VQUIT, VSUSP, VMIN, VTIME.
            t[17] = 0x03; // VINTR  = ^C
            t[18] = 0x1C; // VQUIT  = ^\
            t[19] = 0x7F; // VERASE = DEL
            t[20] = 0x15; // VKILL  = ^U
            t[21] = 0x04; // VEOF   = ^D
            t[22] = 0x00; // VTIME
            t[23] = 0x01; // VMIN   = 1
            t[27] = 0x1A; // VSUSP  = ^Z
            if errno::is_err(copy_to_user(arg, &t)) {
                return errno::EFAULT;
            }
            0
        }
        TIOCGWINSZ => {
            if arg == 0 {
                return errno::EFAULT;
            }
            // struct winsize { u16 ws_row, ws_col, ws_xpixel, ws_ypixel }.
            let mut w = [0u8; 8];
            w[0..2].copy_from_slice(&24u16.to_le_bytes()); // ws_row
            w[2..4].copy_from_slice(&80u16.to_le_bytes()); // ws_col
            if errno::is_err(copy_to_user(arg, &w)) {
                return errno::EFAULT;
            }
            0
        }
        // The setters and job-control queries: accept, and answer with the one
        // process group this target has.
        TCSETS | TCSETSW | TCSETSF | TIOCSWINSZ | TIOCSPGRP | TIOCSCTTY => 0,
        TIOCGPGRP => {
            if arg != 0 && errno::is_err(copy_to_user(arg, &1i32.to_le_bytes())) {
                return errno::EFAULT;
            }
            0
        }
        _ => errno::ENOTTY,
    }
}

/// Install a read-only fd for a **synthetic directory**: one that exists only
/// as a listing, with no ext2 inode behind it.
///
/// The listing is pre-seeded into the descriptor's `dir_cache`, which is the
/// field [`sys_getdents64`] already consults before it would call
/// `fs::read_dir`. So a synthetic directory needs no branch in `getdents64` at
/// all — and it inherits the snapshot-on-open semantics for free, which is what
/// a process table being walked while processes come and go requires anyway.
fn install_synthetic_dir(path: &str, names: Vec<(alloc::string::String, u8)>, flags: u64) -> u64 {
    let mut file = KernelFile::new(alloc::string::String::from(path), flags as u32);
    file.dir_cache = Some(
        names
            .into_iter()
            .map(|(name, d_type)| akuma_exec_core::process::DirCacheEntry { name, d_type })
            .collect(),
    );
    install(FileDescriptor::File(file))
}

/// DT_DIR / DT_REG, as `getdents64` spells them.
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;

/// The per-process files this target serves under `/proc/<pid>/` for **any**
/// pid. Rendered by `akuma-procfs` out of the spawn table, which is the only
/// process state reachable by pid here.
const PID_FILES: [&str; 3] = ["cmdline", "stat", "status"];

/// The two more it serves for the **calling** process only.
///
/// `maps` and `statm` describe an *address space*, and the only address space
/// this target can name is the running one: the fd rows are keyed by process slot,
/// the spawn table is keyed by pid, and nothing joins them. Every real reader of
/// these two files reads its own — an allocator sizing its arenas, a sanitiser
/// finding the heap, `ps` reading `statm` for its own RSS — so serving `self`
/// and nothing else is a narrowing rather than a fiction.
///
/// **It must be a narrowing of the listing too.** `open_proc`'s own comment
/// spells out why: a name advertised in `/proc/<pid>` whose `stat` then says
/// `ENOENT` is how `ls /proc/2` prints "No such file or directory" about its own
/// listing. So [`pid_files`] returns these only for the pid that can serve them,
/// and `render_pid_file` refuses them for any other — one rule, asserted in both
/// directions by `proc_check`.
const SELF_ONLY_PID_FILES: [&str; 2] = ["maps", "statm"];

/// The per-process file names `/proc/<pid>` should list, for this pid.
fn pid_files(pid: u32) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = PID_FILES.to_vec();
    if pid == crate::usermode::current_pid() {
        out.extend_from_slice(&SELF_ONLY_PID_FILES);
    }
    out
}

/// Normalise a path under `/proc`: drop trailing slashes, then rewrite a
/// leading `self` to the calling process's own pid.
///
/// # Both halves are load-bearing, and both were found the hard way
///
/// **Trailing slashes.** `busybox ps` stats `"/proc/1/"`, not `"/proc/1"` — it
/// builds the directory prefix once and reuses it with the filename appended,
/// so the bare-directory `stat` carries the separator. Without the trim,
/// `"1/"` split into `("1", Some(""))` and matched no arm, `stat` answered
/// `ENOENT`, and `procps_scan` did what it does for a process that exited
/// between the `readdir` and the `stat`: `continue`. Every pid was skipped and
/// `ps` printed its header and nothing else — with no error anywhere, because
/// from `ps`'s point of view nothing had gone wrong.
///
/// **`self`.** `/proc/self` is a symlink on Linux and essentially every tool
/// reaches `/proc` through it. This target has no symlink machinery for a
/// synthetic path, so the rewrite happens here, on the string. The AArch64
/// kernel has the identical helper for the identical reason
/// (`akuma-vfs-glue`'s `resolve_self`), where its absence was what stopped
/// `redis-server` starting at all.
fn normalise_proc(rest: &str) -> alloc::string::String {
    let rest = rest.trim_end_matches('/');
    let pid = crate::usermode::current_pid();
    let mut out = alloc::string::String::new();
    use core::fmt::Write as _;
    if rest == "self" {
        let _ = write!(out, "{pid}");
    } else if let Some(tail) = rest.strip_prefix("self/") {
        let _ = write!(out, "{pid}/{tail}");
    } else {
        out.push_str(rest);
    }
    out
}

/// Render one `/proc/<pid>/<file>`, or `None` if the pid or the file is not one
/// this target serves.
///
/// The bytes themselves are `akuma-procfs`, shared with the AArch64 kernel and
/// host-tested there — this function is only the lookup and the buffer.
fn render_pid_file(pid: u32, file: &str) -> Option<Vec<u8>> {
    // `stat`, `status` and `cmdline` stay here rather than moving to the
    // mounted `ProcFilesystem`, which serves all three and renders them from
    // the same `akuma-exec` table through the same `akuma-procfs` formats.
    // Moving them was tried during 5b slice 3 and reverted, for a reason worth
    // recording:
    //
    // **The boot suite runs before `run_init`.** Its `proc: open/stat/access
    // agree on /proc/self/{stat,status,cmdline}` checks execute on a task that
    // is registered in no process table, and `current_pid()` answers 1 for it.
    // This file can answer for that window (`proc_by_pid` has an explicit pid-1
    // fallback); the mounted filesystem cannot, because it reads the table and
    // the table is empty. Routing the three there made all three checks fail.
    //
    // Serving them from a table the mount also reads is duplication, but not
    // *divergence*: one source, one format crate, two callers. Closing it means
    // registering pid 1 before the self-tests run, which is its own change with
    // its own hazards (`run_init` registers pid 1 too, and would then be
    // re-registering rather than creating).
    //
    // `meminfo`, `mounts` and `net/dev` stay for a different and stronger
    // reason: they read *this* target's PMM, mount table and interface list,
    // where the crate's same-named files read the AArch64 kernel's. A shared
    // format is not a shared source.
    let entry = crate::usermode::proc_by_pid(pid)?;
    let name_owned = alloc::string::String::from(entry.name());
    let stat = entry.stat(&name_owned);
    match file {
        "stat" => {
            let mut buf = [0u8; akuma_procfs::STAT_LINE_MAX];
            let n = akuma_procfs::render_pid_stat(&stat, &mut buf);
            Some(buf[..n].to_vec())
        }
        "status" => {
            let mut buf = [0u8; akuma_procfs::STATUS_MAX];
            let n = akuma_procfs::render_status(&stat, &mut buf);
            Some(buf[..n].to_vec())
        }
        // Already in the `/proc` wire form (NUL-separated) — `usermode` stores
        // it that way, so this is a copy rather than a render.
        "cmdline" => Some(entry.cmdline.clone()),
        // The two address-space files, for the caller's own pid only — see
        // `SELF_ONLY_PID_FILES` for why, and note the guard is here as well as
        // in `pid_files` so `open`, `stat` and `access` all agree.
        "maps" if pid == crate::usermode::current_pid() => Some(render_self_maps()),
        "statm" if pid == crate::usermode::current_pid() => Some(render_self_statm()),
        _ => None,
    }
}

/// `/proc/self/maps` — the calling process's address space, ascending.
///
/// # Two sources, because neither alone is the address space
///
/// The **region list** holds what `mmap` recorded, including a lazy reservation
/// no page of which exists yet — Linux reports a VMA, not its resident pages, so
/// that is the authoritative extent wherever there is one. But the ELF image and
/// the initial stack are placed by the loader and are deliberately *not*
/// regions (`mm.rs`: the region table is `mmap`'s; frame ownership is the
/// ledger's), so a walk of the regions alone produces an **empty** file for an
/// ordinary program — which is what the first version of this function did.
///
/// That is worse than not serving the file at all, and it is the same mistake as
/// answering `ENOSYS` where Linux answers `EINVAL`: a reader scanning `maps` for
/// the mapping containing an address gets a confident "there is none" instead of
/// "this kernel cannot tell you". So the present **page-table leaves** outside
/// every region are coalesced into runs and reported too — the image, the stack,
/// and anything else the loader mapped.
///
/// # What it still is not
///
/// A run of leaves is not a VMA: two adjacent loader mappings with identical
/// permissions merge into one line, and a lazy region's unfaulted middle is one
/// line rather than a hole. Both are what the hardware says, which is the only
/// record this target keeps for loader pages.
///
/// # Ascending, because that is part of the format
///
/// The region list is not kept in address order — `detach_eager_regions_in_range`
/// pushes survivors onto the end — and a parser that stops at the first line past
/// the address it wants would miss on an unsorted file.
fn render_self_maps() -> Vec<u8> {
    let mut out = Vec::new();
    let mut line = [0u8; akuma_procfs::MAPS_LINE_MAX];
    for (start, end, read, write, exec, private) in self_map_rows() {
        let n = akuma_procfs::render_maps_line(
            start, end, read, write, exec, private, "", &mut line,
        );
        out.extend_from_slice(&line[..n]);
    }
    out
}

/// `(start, end, readable, write, exec, private)` — one mapping.
type MapRow = (usize, usize, bool, bool, bool, bool);

/// The calling process's mappings, ascending. See [`render_self_maps`] for why
/// there are two sources; this is that walk, shared with [`render_self_statm`]
/// so `maps` and `statm` cannot report different address spaces.
fn self_map_rows() -> Vec<MapRow> {
    type Row = MapRow;

    let mut rows: Vec<Row> = crate::usermode::with_current_regions(|regions| {
        regions
            .iter()
            .map(|r| {
                let prot = r.recorded_prot().unwrap_or(akuma_mmap::Prot::RW_NO_EXEC);
                (
                    r.start_va,
                    r.start_va.saturating_add(r.len_bytes()),
                    // A `PROT_NONE` reservation is `---p` on Linux too: a
                    // mapping that exists and grants nothing, which is exactly
                    // what a guard region is and what a reader looks for.
                    !prot.is_none(),
                    prot.is_write(),
                    prot.is_exec(),
                    !r.shared_anon,
                )
            })
            .collect()
    })
    .unwrap_or_default();

    // The region extents, so the leaf walk can skip what a region already
    // describes. Taken as a snapshot rather than re-locking per page.
    let extents: Vec<(usize, usize)> = rows.iter().map(|r| (r.0, r.1)).collect();

    // Present user leaves outside every region, coalesced. `for_each_user_leaf`
    // visits in ascending VA order (it walks each level's indices upward), which
    // is what makes a single-pass coalesce correct rather than a sort-then-merge.
    let mut run: Option<Row> = None;
    let _ = crate::usermode::with_current_address_space(|uas| {
    uas.for_each_user_leaf(|leaf| {
        let (va, prot) = (leaf.va, leaf.prot);
        if !prot.user || extents.iter().any(|&(s, e)| va >= s && va < e) {
            // Flush across a gap the region list already covers, so a mapping
            // either side of it is not merged through it.
            if let Some(r) = run.take() {
                rows.push(r);
            }
            return;
        }
        // Every present leaf is readable — x86 has no read-disable bit, so
        // `r` is not a fact the PTE can carry differently.
        let (w, x) = (prot.write, prot.exec);
        match run {
            Some(ref mut r) if r.1 == va && r.3 == w && r.4 == x => {
                r.1 = va + 4096;
            }
            _ => {
                if let Some(r) = run.take() {
                    rows.push(r);
                }
                run = Some((va, va + 4096, true, w, x, true));
            }
        }
    });
    });
    if let Some(r) = run.take() {
        rows.push(r);
    }

    rows.sort_unstable_by_key(|r| r.0);
    rows
}

/// `/proc/self/statm` — the seven page counts.
///
/// Built from the **same** [`self_map_rows`] walk `maps` renders, which is the
/// point: the first version summed the region list alone and reported `size 0`
/// for an ordinary program whose `maps` plainly listed three mappings. Two
/// files disagreeing about one address space is the shape of bug this whole
/// `/proc` section keeps producing (see `proc_consistency_check`), so they read
/// from one function.
///
/// - `size` — every mapped page, including a lazy region's unfaulted middle.
///   That is what Linux reports and it is much larger than the resident set.
/// - `resident` — the frame ledger's count, the only real number this target
///   has for physical pages held.
/// - `text` — the executable rows. `data` is the rest, which is Linux's
///   "data + stack" and is why the two sum to `size`.
/// - `shared` and `lib` and `dt` are 0: nothing here tracks them per process,
///   and a fabricated breakdown would be read as a real one.
fn render_self_statm() -> Vec<u8> {
    let rows = self_map_rows();
    let pages = |(s, e, _, _, _, _): &MapRow| e.saturating_sub(*s) / 4096;
    let size_pages: usize = rows.iter().map(pages).sum();
    let text_pages: usize = rows.iter().filter(|r| r.4).map(pages).sum();
    let resident = crate::usermode::current_resident_pages() as u64;
    let mut buf = [0u8; akuma_procfs::STATM_MAX];
    let n = akuma_procfs::render_statm(
        size_pages as u64,
        resident,
        0,
        text_pages as u64,
        size_pages.saturating_sub(text_pages) as u64,
        &mut buf,
    );
    buf[..n].to_vec()
}

/// Render any `/proc` file this target serves, system-wide or per-process.
///
/// One function for `open` and for `stat`, so the two can never disagree about
/// what exists — the failure that shape produces is `ls /proc` listing a name
/// whose `stat` then says `No such file or directory`.
fn render_proc_file(rest: &str) -> Option<Vec<u8>> {
    match rest {
        // `busybox ifconfig` with no interface name reads this to enumerate
        // devices before it will print anything. Generated, not stored.
        "net/dev" => {
            let mut text = alloc::string::String::new();
            let _ = akuma_syscalls_net::write_proc_net_dev(&interfaces(), &mut text);
            Some(text.into_bytes())
        }
        // `busybox df` reads this **first** — it enumerates mounts here and
        // then calls `statfs` on each one, so with no `/proc/mounts` it prints
        // a header and nothing else no matter how well `statfs` works.
        // Rendered from the mount table rather than stored.
        "mounts" => {
            // 8 mounts (`MountSet<8>`) x a line that cannot exceed ~120 bytes.
            let mut buf = [0u8; 1024];
            let n = fs::render_mounts(&mut buf);
            Some(buf[..n].to_vec())
        }
        "meminfo" => Some(render_meminfo().into_bytes()),
        _ => {
            let (head, file) = rest.split_once('/')?;
            render_pid_file(head.parse::<u32>().ok()?, file)
        }
    }
}

/// `/proc/meminfo`. `busybox free` / `top` read this.
///
/// Only the three fields `free` actually parses carry real numbers — physical
/// RAM the PMM was handed, what it has free, and the kernel heap folded into
/// `Cached` so the number moves when a file-cache leak (see
/// `net::mem_watch_tick`) is eating it. Every other field is present and zero
/// **on purpose**: `free` looks some up by the old name (`MemShared`) and some
/// by the new (`Shmem`), and a name it does not find can be left as
/// `ULONG_MAX` and underflow the `used = total - free - …` line into the
/// 18-quintillion garbage that first showed up here.
fn render_meminfo() -> alloc::string::String {
    let page = 4096u64;
    let total_kib = akuma_pmm::total_count() as u64 * page / 1024;
    let free_kib = akuma_pmm::free_count() as u64 * page / 1024;
    let heap = akuma_alloc::stats();
    let heap_used_kib = (heap.allocated / 1024) as u64;
    let mut text = alloc::string::String::new();
    use core::fmt::Write as _;
    let _ = write!(
        text,
        "MemTotal:       {total_kib:>10} kB\n\
         MemFree:        {free_kib:>10} kB\n\
         MemAvailable:   {free_kib:>10} kB\n\
         MemShared:      {z:>10} kB\n\
         Buffers:        {z:>10} kB\n\
         Cached:         {heap_used_kib:>10} kB\n\
         SwapCached:     {z:>10} kB\n\
         Active:         {z:>10} kB\n\
         Inactive:       {z:>10} kB\n\
         SwapTotal:      {z:>10} kB\n\
         SwapFree:       {z:>10} kB\n\
         Dirty:          {z:>10} kB\n\
         Writeback:      {z:>10} kB\n\
         AnonPages:      {z:>10} kB\n\
         Mapped:         {z:>10} kB\n\
         Shmem:          {z:>10} kB\n\
         Slab:           {z:>10} kB\n\
         SReclaimable:   {z:>10} kB\n\
         SUnreclaim:     {z:>10} kB\n",
        z = 0,
    );
    text
}

/// Is `rest` a directory this target synthesises under `/proc`?
///
/// `stat` must answer yes for `/proc/<pid>` before `ps` will look inside it.
fn proc_is_dir(rest: &str) -> bool {
    if rest.is_empty() || rest == "net" {
        return true;
    }
    match rest.split_once('/') {
        None => rest.parse::<u32>().is_ok_and(|p| crate::usermode::proc_by_pid(p).is_some()),
        Some((head, "fd")) => {
            head.parse::<u32>().is_ok_and(|p| crate::usermode::proc_by_pid(p).is_some())
        }
        Some(_) => false,
    }
}

/// The size `stat` should report for a `/proc` path, and whether it is a
/// directory. `None` if this target does not serve it.
///
/// Rendering the file just to measure it is deliberate: a `/proc` file's length
/// is a property of the moment, and reporting a stale or guessed size is how a
/// reader that trusts `st_size` (rather than reading to EOF) truncates. The
/// cost is one render per `stat`, on a path nothing calls in a loop.
fn proc_metadata(rest: &str) -> Option<(u64, bool)> {
    let rest = normalise_proc(rest);
    if proc_is_dir(&rest) {
        return Some((0, true));
    }
    render_proc_file(&rest).map(|d| (d.len() as u64, false))
}

/// Answer an `open` under `/proc`, or `None` to fall through to the disk.
///
/// `rest` is the path with the leading `/proc/` (or `/proc`) stripped: the empty
/// string is `/proc` itself.
///
/// # Why `/proc` has to be intercepted at all
///
/// `/proc` is a **real, empty ext2 directory** on this target's image
/// (`mkdisk.sh`, so that `busybox reboot` can find init). So `getdents64` on it
/// succeeded and returned nothing, and `ps` printed its header and stopped —
/// a failure with no error anywhere in it. Everything below replaces that empty
/// listing with the live process table.
fn open_proc(rest: &str, flags: u64) -> Option<u64> {
    let rest = normalise_proc(rest);
    let rest = rest.as_str();

    // `/proc` itself: one directory entry per live process, plus the files
    // already served here. `.`/`..` are omitted — `getdents64` on this target
    // has never emitted them for a real directory either, and `ps` skips
    // non-numeric names regardless.
    //
    // **Only names that also `stat`.** Advertising one that does not is how
    // `ls /proc` ends up printing `No such file or directory` for its own
    // listing, which is why `render_proc_file` serves both this and `stat`.
    if rest.is_empty() {
        let mut names: Vec<(alloc::string::String, u8)> = Vec::new();
        for p in crate::usermode::proc_list() {
            let mut n = alloc::string::String::new();
            use core::fmt::Write as _;
            let _ = write!(n, "{}", p.pid);
            names.push((n, DT_DIR));
        }
        names.push((alloc::string::String::from("self"), DT_DIR));
        names.push((alloc::string::String::from("net"), DT_DIR));
        for f in ["meminfo", "mounts"] {
            names.push((alloc::string::String::from(f), DT_REG));
        }
        return Some(install_synthetic_dir("/proc", names, flags));
    }

    if rest == "net" {
        return Some(install_synthetic_dir(
            "/proc/net",
            alloc::vec![(alloc::string::String::from("dev"), DT_REG)],
            flags,
        ));
    }

    // `/proc/<pid>` — the per-process directory.
    if let Ok(pid) = rest.parse::<u32>() {
        if crate::usermode::proc_by_pid(pid).is_none() {
            return Some(errno::ENOENT);
        }
        let mut names: Vec<(alloc::string::String, u8)> = pid_files(pid)
            .iter()
            .map(|f| (alloc::string::String::from(*f), DT_REG))
            .collect();
        names.push((alloc::string::String::from("fd"), DT_DIR));
        return Some(install_synthetic_dir(&alloc::format!("/proc/{pid}"), names, flags));
    }

    // The render doubles as the existence check; the bytes themselves are
    // produced per `read(2)` now, so they are dropped here. The descriptor's
    // path is the **absolute** one — `proc_rest_of` classifies a synthetic
    // descriptor by its `/proc/` prefix, and it used to be harmless that this
    // stored the bare rest (`1/statm`) only because the old read path served
    // cached bytes and never looked at the path.
    render_proc_file(rest).map(|_| install_synthetic_file(&alloc::format!("/proc/{rest}"), flags))
}

#[cfg(not(feature = "no-tests"))]
/// `open`, `stat` and `access` must agree about every `/proc` path.
///
/// # Why this is a self-test and not a comment
///
/// The three answers came from **two** functions. `sys_openat` and
/// `sys_newfstatat` both went through `proc_metadata`/`render_proc_file` —
/// `render_proc_file`'s header says in as many words that one function serves
/// both "so the two can never disagree about what exists" — and `sys_access`
/// went straight to the disk, so it said `ENOENT` for every `/proc` path this
/// target serves. Nothing failed loudly: `busybox` mostly `open`s, and the one
/// caller that probes with `access` first is a program deciding whether the
/// kernel has a `/proc` at all.
///
/// Found 2026-09-07 by `smapsdirty`'s `proc-self-files` sub-probe, which
/// reported `stat status cmdline` missing on a target that serves all three.
/// The invariant is cheap to state and was expensive to notice, so it is
/// asserted in both directions: a path that exists answers 0 from all three, and
/// a path that does not answers `ENOENT` from all three.
fn proc_consistency_check(t: &mut Suite) {
    #[cfg(not(feature = "no-tests"))]
    /// One path, checked three ways. `want` is whether it should exist.
    fn agree(t: &mut Suite, label: &'static str, path: &[u8], want: bool) {
        let p = path.as_ptr() as u64;
        let mut st = [0u8; 160];
        let acc = sys_access(p);
        let sta = sys_newfstatat((-100i64) as u64, p, st.as_mut_ptr() as u64, 0);
        let fd = sys_openat(0, p, 0, 0);
        let opened = !errno::is_err(fd);
        if opened {
            sys_close(fd);
        }
        t.check(label, (acc == 0) == want && (sta == 0) == want && opened == want);
    }

    // The three served for any pid, reached through `self` — which is a string
    // rewrite here, not a symlink, so it is worth asserting rather than assuming.
    agree(t, "proc: open/stat/access agree on /proc/self/stat", b"/proc/self/stat\0", true);
    agree(t, "proc: open/stat/access agree on /proc/self/status", b"/proc/self/status\0", true);
    agree(t, "proc: open/stat/access agree on /proc/self/cmdline", b"/proc/self/cmdline\0", true);
    // The two served for the calling process only (2026-09-07).
    agree(t, "proc: open/stat/access agree on /proc/self/maps", b"/proc/self/maps\0", true);
    agree(t, "proc: open/stat/access agree on /proc/self/statm", b"/proc/self/statm\0", true);
    // And the negative direction, which is the half that catches an
    // "everything under /proc exists" fix.
    agree(t, "proc: all three refuse a file /proc does not serve", b"/proc/self/smaps\0", false);
    agree(t, "proc: all three refuse a pid that is not running", b"/proc/4242/stat\0", false);

    // `statm` must be seven page counts, and `resident` must not be the byte
    // count: every reader multiplies by its own page size, so bytes here read
    // as a process 4096 times too big.
    let sp = b"/proc/self/statm\0";
    let fd = sys_openat(0, sp.as_ptr() as u64, 0, 0);
    if t.check("proc: /proc/self/statm opens", fd >= FIRST_FILE_FD as u64) {
        let mut d = [0u8; akuma_procfs::STATM_MAX];
        let n = sys_read(fd, d.as_mut_ptr() as u64, d.len() as u64);
        let n = n.min(d.len() as u64) as usize;
        let text = core::str::from_utf8(&d[..n]).unwrap_or("");
        t.check(
            "proc: /proc/self/statm has seven fields",
            text.trim_end().split(' ').count() == 7,
        );
        sys_close(fd);
    }
}

/// Install a read-only fd for a generated `/proc` file (like `/proc/net/dev`).
///
/// Nothing is stored: since step 4b the render is produced per `read(2)` by
/// [`render_proc_file`], and the bytes this function used to cache into the
/// now-deleted `Entry` are gone. The caller still renders once as the
/// **existence check** — a view that cannot render fails the open, so a
/// descriptor from here always has something to re-render.
fn install_synthetic_file(path: &str, flags: u64) -> u64 {
    install(FileDescriptor::File(KernelFile::new(
        alloc::string::String::from(path),
        flags as u32,
    )))
}

/// The two synthetic interfaces `ifconfig` sees: `lo` and the live smoltcp
/// `eth0`. Built fresh each call so a DHCP change is reflected.
fn interfaces() -> [akuma_syscalls_net::Interface; 2] {
    let snap = akuma_net::smoltcp_net::interface_snapshot();
    [
        akuma_syscalls_net::Interface::loopback(),
        akuma_syscalls_net::Interface::ethernet(
            snap.ip,
            snap.prefix_len,
            snap.mac,
            u32::from(snap.mtu),
        ),
    ]
}

/// `SIOCGIFCONF` / `SIOCGIF{FLAGS,ADDR,NETMASK,BRDADDR,MTU,HWADDR}` — the
/// read-only half of `ifconfig`. The `struct ifreq` / `struct ifconf` byte
/// layout is `akuma-syscalls-net`; this does the user copies.
fn siocgif(cmd: u32, arg: u64) -> u64 {
    use akuma_syscalls_linux::net::{IFREQ_UNION_OFFSET, SIZEOF_IFREQ};

    if arg == 0 {
        return errno::EFAULT;
    }
    let ifaces = interfaces();

    if cmd == akuma_syscalls_net::cmd::SIOCGIFCONF {
        // struct ifconf { i32 ifc_len; i32 _pad; u64 ifc_buf; }
        let Some(len) = crate::uaccess::read_val::<i32>(arg) else {
            return errno::EFAULT;
        };
        let Some(buf) = crate::uaccess::read_val::<u64>(arg + 8) else {
            return errno::EFAULT;
        };
        let written = if buf == 0 {
            akuma_syscalls_net::siocgifconf_size(&ifaces)
        } else {
            let cap = usize::try_from(len).unwrap_or(0);
            let fit = akuma_syscalls_net::siocgifconf_capacity(&ifaces, cap);
            for (i, iface) in ifaces.iter().take(fit).enumerate() {
                let rec = akuma_syscalls_net::siocgifconf_record(iface);
                if errno::is_err(copy_to_user(buf + (i * SIZEOF_IFREQ) as u64, &rec)) {
                    return errno::EFAULT;
                }
            }
            fit * SIZEOF_IFREQ
        };
        let n = i32::try_from(written).unwrap_or(i32::MAX);
        if !crate::uaccess::write_val::<i32>(arg, n) {
            return errno::EFAULT;
        }
        return 0;
    }

    // The rest: read the 16-byte ifr_name, marshal the union member, write it
    // back at arg + 16.
    let mut name = [0u8; 16];
    if !crate::uaccess::read_bytes(arg, &mut name) {
        return errno::EFAULT;
    }
    let mut union = [0u8; 24];
    match akuma_syscalls_net::siocgifreq_reply(cmd, &ifaces, &name, &mut union) {
        Ok(n) => {
            if errno::is_err(copy_to_user(arg + IFREQ_UNION_OFFSET as u64, &union[..n])) {
                errno::EFAULT
            } else {
                0
            }
        }
        Err(akuma_syscalls_net::ReplyError::NoDevice) => errno::ENODEV,
        Err(akuma_syscalls_net::ReplyError::NotHandled) => errno::ENOTTY,
    }
}

#[cfg(not(feature = "no-tests"))]
/// Exercise the descriptor path from the kernel side.
///
/// Ring 3 exercises it for real in `usermode`; this checks the parts that are
/// awkward to reach from a guest program — the error cases, and the table
/// filling up.
pub fn smoke_test(t: &mut Suite, have_fs: bool) {
    if !have_fs {
        t.note("fd: no filesystem; skipped", 0);
        return;
    }

    // A kernel-side buffer standing in for a user pointer. The copy helpers do
    // not care which side of the privilege boundary an address is on — they
    // dereference it — so this is a faithful exercise of the same path.
    let mut buf = [0u8; 64];
    let path = b"/probe.txt\0";

    let fd = sys_openat(0, path.as_ptr() as u64, 0, 0);
    if !t.check("fd: open /probe.txt", fd >= FIRST_FILE_FD as u64) {
        return;
    }
    let n = sys_read(fd, buf.as_mut_ptr() as u64, 22);
    t.check_eq("fd: read returns the requested length", n, 22);
    t.check("fd: read returns the file's first bytes", &buf[..22] == b"AKUMA/amd64 ext2 probe");

    // Seek and re-read: the cursor must be a property of the descriptor.
    t.check_eq("fd: lseek to 0", sys_lseek(fd, 0, 0), 0);
    t.check_eq("fd: re-read after seek", sys_read(fd, buf.as_mut_ptr() as u64, 5), 5);
    t.check("fd: the same bytes come back", &buf[..5] == b"AKUMA");

    // SEEK_END then read must return 0 rather than an error: end-of-file is not
    // a failure, and a reader that treats it as one never terminates.
    let end = sys_lseek(fd, 0, 2);
    t.check_eq("fd: SEEK_END reports the file size", end, 6623);
    t.check_eq("fd: reading at EOF returns 0", sys_read(fd, buf.as_mut_ptr() as u64, 16), 0);

    let mut st = [0u8; STAT_SIZE];
    t.check_eq("fd: fstat succeeds", sys_fstat(fd, st.as_mut_ptr() as u64), 0);
    t.check_eq(
        "fd: fstat reports the size",
        u64::from_le_bytes(st[48..56].try_into().unwrap_or([0; 8])),
        6623,
    );

    // The `/dev` character nodes. `ls -la /dev` has listed these since devfs
    // arrived, but nothing ever bound a **descriptor** to one: `open` fell
    // through to the ext2 path, where `/dev` is not a directory, and the two
    // eras of that fall-through failed differently and silently — see
    // `sys_openat`'s device arm. `ls > /dev/null` is what reaches it, on a
    // rootfs with no real `/dev`, which is every rootfs this target has.
    {
        let devnull = b"/dev/null\0";
        // `O_WRONLY | O_CREAT | O_TRUNC` — exactly what a shell's `>` emits,
        // and the combination that used to try to *create* the node.
        let nfd = sys_openat(0, devnull.as_ptr() as u64, 0o1101, 0);
        // `!is_err`, not `>= FIRST_FILE_FD`: an errno is a `u64` at the very
        // top of the range, so the obvious bound check passes for **every**
        // failure. The `/dev/zero` arm below was written that way first and
        // scored a green open on an `ENOENT`.
        if t.check("fd: open /dev/null for writing", !errno::is_err(nfd)) {
            let payload = b"discarded";
            t.check_eq(
                "fd: writing to /dev/null accepts every byte",
                sys_write_file(nfd, payload.as_ptr() as u64, payload.len() as u64),
                payload.len() as u64,
            );
            t.check_eq(
                "fd: reading /dev/null is immediate EOF",
                sys_read(nfd, buf.as_mut_ptr() as u64, 16),
                0,
            );
            let mut nst = [0u8; STAT_SIZE];
            t.check_eq("fd: fstat on /dev/null succeeds", sys_fstat(nfd, nst.as_mut_ptr() as u64), 0);
            t.check_eq(
                "fd: and reports a character device",
                u64::from(u32::from_le_bytes(nst[24..28].try_into().unwrap_or([0; 4])) & 0o170_000),
                0o020_000,
            );
            t.check_eq("fd: close /dev/null", sys_close(nfd), 0);
        }
        // `/dev/zero` reads zeros forever — and it must actually *fill* the
        // buffer, not leave whatever was there. Seeded non-zero first, which
        // is the difference between this check and a no-op.
        let devzero = b"/dev/zero\0";
        let zfd = sys_openat(0, devzero.as_ptr() as u64, 0, 0);
        if t.check("fd: open /dev/zero", !errno::is_err(zfd)) {
            buf.fill(0xAA);
            t.check_eq("fd: /dev/zero reads the length asked for", sys_read(zfd, buf.as_mut_ptr() as u64, 32), 32);
            t.check("fd: and every byte of it is zero", buf[..32].iter().all(|&b| b == 0));
            let _ = sys_close(zfd);
        }
        // A **block** node is refused rather than falling through — `ENODEV`,
        // not the `ENOENT` an absent file would get, because it is present and
        // this target will not serve it.
        //
        // Asked only when the node is actually there. `/dev/vda` exists when
        // `dev_probe` finds a virtio-blk device, which is QEMU and Firecracker
        // and **not** the bare-metal box, whose root is a USB disk — so an
        // unconditional check here is a suite that fails on the one machine
        // that matters most.
        if akuma_vfs_glue::dev_node("/dev/vda").is_some() {
            let devvda = b"/dev/vda\0";
            t.check_eq(
                "fd: open /dev/vda is ENODEV, not a fall-through",
                sys_openat(0, devvda.as_ptr() as u64, 0, 0),
                errno::ENODEV,
            );
        }
    }

    // Path-based stat: `newfstatat(AT_FDCWD, "/probe.txt", &st, 0)`, the form
    // `stat(2)` decodes to. Size, regular-file type bit and link count must all
    // come back — busybox `sh` reads exactly these off a PATH entry.
    const AT_FDCWD: u64 = (-100i64) as u64;
    st = [0u8; STAT_SIZE];
    let probe = b"/probe.txt\0";
    t.check_eq(
        "fd: newfstatat /probe.txt succeeds",
        sys_newfstatat(AT_FDCWD, probe.as_ptr() as u64, st.as_mut_ptr() as u64, 0),
        0,
    );
    t.check_eq(
        "fd: newfstatat reports the size",
        u64::from_le_bytes(st[48..56].try_into().unwrap_or([0; 8])),
        6623,
    );
    let mode = u32::from_le_bytes(st[24..28].try_into().unwrap_or([0; 4]));
    t.check("fd: newfstatat reports S_IFREG", mode & 0o170_000 == 0o100_000);
    t.check_eq(
        "fd: newfstatat reports st_nlink",
        u64::from_le_bytes(st[16..24].try_into().unwrap_or([0; 8])),
        1,
    );

    // A directory: the type bit must switch to S_IFDIR and nlink to 2.
    st = [0u8; STAT_SIZE];
    let bindir = b"/bin\0";
    t.check_eq(
        "fd: newfstatat /bin succeeds",
        sys_newfstatat(AT_FDCWD, bindir.as_ptr() as u64, st.as_mut_ptr() as u64, 0),
        0,
    );
    let dmode = u32::from_le_bytes(st[24..28].try_into().unwrap_or([0; 4]));
    t.check("fd: newfstatat reports S_IFDIR for /bin", dmode & 0o170_000 == 0o040_000);

    // A missing path is ENOENT, not ENOSYS — the whole point of the stage.
    let gone = b"/no/such/path\0";
    t.check_eq(
        "fd: newfstatat on a missing path is ENOENT",
        sys_newfstatat(AT_FDCWD, gone.as_ptr() as u64, st.as_mut_ptr() as u64, 0),
        errno::ENOENT,
    );

    // `AT_EMPTY_PATH` on an open fd falls through to `fstat`.
    st = [0u8; STAT_SIZE];
    let empty = b"\0";
    t.check_eq(
        "fd: newfstatat AT_EMPTY_PATH stats the fd",
        sys_newfstatat(fd, empty.as_ptr() as u64, st.as_mut_ptr() as u64, 0x1000),
        0,
    );
    t.check_eq(
        "fd: newfstatat AT_EMPTY_PATH reports the fd's size",
        u64::from_le_bytes(st[48..56].try_into().unwrap_or([0; 8])),
        6623,
    );

    // `access`: a resolvable path is 0, a missing one ENOENT.
    let probe_c = b"/probe.txt\0";
    t.check_eq("fd: access(/probe.txt) is 0", sys_access(probe_c.as_ptr() as u64), 0);
    t.check_eq(
        "fd: access on a missing path is ENOENT",
        sys_access(gone.as_ptr() as u64),
        errno::ENOENT,
    );

    // `ioctl(TCGETS)` on the console answers rather than failing — this is what
    // stops an interactive busybox deciding stdin is not a terminal.
    let mut term = [0u8; 44];
    t.check_eq(
        "fd: ioctl(0, TCGETS) succeeds",
        sys_ioctl(0, 0x5401, term.as_mut_ptr() as u64),
        0,
    );
    t.check(
        "fd: TCGETS reports a cooked-mode c_lflag (ICANON|ECHO)",
        u32::from_le_bytes(term[12..16].try_into().unwrap_or([0; 4])) & 0x0A == 0x0A,
    );
    let mut ws = [0u8; 8];
    t.check_eq(
        "fd: ioctl(0, TIOCGWINSZ) succeeds",
        sys_ioctl(0, 0x5413, ws.as_mut_ptr() as u64),
        0,
    );
    t.check_eq(
        "fd: TIOCGWINSZ reports 80 columns",
        u64::from(u16::from_le_bytes(ws[2..4].try_into().unwrap_or([0; 2]))),
        80,
    );
    t.check_eq(
        "fd: ioctl(TCGETS) on a file is ENOTTY",
        sys_ioctl(FIRST_FILE_FD as u64, 0x5401, term.as_mut_ptr() as u64),
        errno::ENOTTY,
    );

    // `ifconfig`'s read-only ioctls, on a kernel-stack `struct ifreq` (the
    // self-tests run inside the user-pointer bypass). `SIOCGIF*` are answered
    // regardless of the fd, so a not-open fd is fine here.
    const SIOCGIFADDR: u64 = 0x8915;
    const SIOCGIFFLAGS: u64 = 0x8913;
    let mut ifr = [0u8; 40];
    ifr[..2].copy_from_slice(b"lo");
    t.check_eq(
        "fd: SIOCGIFADDR(lo) succeeds",
        sys_ioctl(3, SIOCGIFADDR, ifr.as_mut_ptr() as u64),
        0,
    );
    t.check("fd: SIOCGIFADDR(lo) returns 127.0.0.1", ifr[20..24] == [127, 0, 0, 1]);
    ifr = [0u8; 40];
    ifr[..4].copy_from_slice(b"eth0");
    t.check_eq(
        "fd: SIOCGIFFLAGS(eth0) succeeds",
        sys_ioctl(3, SIOCGIFFLAGS, ifr.as_mut_ptr() as u64),
        0,
    );
    t.check(
        "fd: eth0 is UP|BROADCAST|RUNNING|MULTICAST",
        i16::from_le_bytes([ifr[16], ifr[17]]) == akuma_syscalls_net::iff::ETHERNET,
    );
    ifr = [0u8; 40];
    ifr[..3].copy_from_slice(b"zz9");
    t.check_eq(
        "fd: SIOCGIFADDR on an unknown interface is ENODEV",
        sys_ioctl(3, SIOCGIFADDR, ifr.as_mut_ptr() as u64),
        errno::ENODEV,
    );
    if have_fs {
        let devp = b"/proc/net/dev\0";
        let devfd = sys_openat(0, devp.as_ptr() as u64, 0, 0);
        t.check("fd: /proc/net/dev opens", devfd >= FIRST_FILE_FD as u64);
        if devfd >= FIRST_FILE_FD as u64 {
            // Both interface rows sit past the ~195-byte two-line header, so
            // the buffer has to be generous — `busybox ifconfig` reads the
            // whole file.
            let mut d = [0u8; 512];
            let n = sys_read(devfd, d.as_mut_ptr() as u64, d.len() as u64);
            let text = &d[..n.min(d.len() as u64) as usize];
            t.check(
                "fd: /proc/net/dev lists lo and eth0",
                n > 0
                    && text.windows(3).any(|w| w == b"lo:")
                    && text.windows(5).any(|w| w == b"eth0:"),
            );
            sys_close(devfd);
        }

        proc_consistency_check(t);

        // `/proc/mounts` + `statfs`, the pair `busybox df` needs. `df` reads
        // the file to learn what to ask about, then calls `statfs` once per
        // line; either one missing and it prints a header and stops, which is
        // what it did on this target before the mount table was wired in.
        let mp = b"/proc/mounts\0";
        let mfd = sys_openat(0, mp.as_ptr() as u64, 0, 0);
        t.check("fd: /proc/mounts opens", mfd >= FIRST_FILE_FD as u64);
        if mfd >= FIRST_FILE_FD as u64 {
            let mut d = [0u8; 512];
            let n = sys_read(mfd, d.as_mut_ptr() as u64, d.len() as u64);
            let text = &d[..n.min(d.len() as u64) as usize];
            t.check(
                "fd: /proc/mounts describes the root mount",
                n > 0 && text.windows(7).any(|w| w == b" / ext2"),
            );
            sys_close(mfd);
        }

        // `statfs("/")`. The buffer is checked for a plausible ext2 rather than
        // exact numbers: the magic pins the field offsets (a layout slip puts
        // `f_bsize` where `f_type` should be), and a non-zero block count is
        // what stops `df` reporting a 0-byte disk.
        let mut sfs = [0u8; core::mem::size_of::<akuma_syscalls_linux::Statfs>()];
        let rootp = b"/\0";
        t.check_eq(
            "fd: statfs(/) succeeds",
            sys_statfs(rootp.as_ptr() as u64, sfs.as_mut_ptr() as u64),
            0,
        );
        t.check_eq(
            "fd: statfs(/) reports ext2 magic",
            u64::from_le_bytes(sfs[0..8].try_into().unwrap_or_default()),
            0xEF53,
        );
        let f_bsize = u64::from_le_bytes(sfs[8..16].try_into().unwrap_or_default());
        let f_blocks = u64::from_le_bytes(sfs[16..24].try_into().unwrap_or_default());
        t.check("fd: statfs(/) block size is sane", (512..=65536).contains(&f_bsize));
        t.check("fd: statfs(/) reports a non-empty filesystem", f_blocks > 0);
        // A path with no filesystem behind it must not report the root's
        // numbers. `fstatfs` on stdin has no path at all, which is the case
        // that falls back to the root mount on purpose.
        t.check_eq(
            "fd: fstatfs(stdin) falls back to the root mount",
            sys_fstatfs(0, sfs.as_mut_ptr() as u64),
            0,
        );
        t.check_eq(
            "fd: statfs into a bad pointer is EFAULT",
            sys_statfs(rootp.as_ptr() as u64, 0),
            errno::EFAULT,
        );
    }

    // `poll`: a regular file is always ready; a zero-length set with a timeout
    // is a sleep that returns 0; an oversized set is EINVAL.
    let mut pfd = [0u8; 8];
    pfd[0..4].copy_from_slice(&(fd as i32).to_le_bytes());
    pfd[4..6].copy_from_slice(&0x001u16.to_le_bytes()); // POLLIN
    t.check_eq(
        "fd: poll reports a regular file ready",
        sys_poll(pfd.as_mut_ptr() as u64, 1, 0),
        1,
    );
    t.check_eq(
        "fd: poll(revents) has POLLIN set",
        u64::from(u16::from_le_bytes(pfd[6..8].try_into().unwrap_or([0; 2]))) & 0x001,
        0x001,
    );
    t.check_eq("fd: poll(NULL, 0, 0) returns 0", sys_poll(0, 0, 0), 0);
    t.check_eq(
        "fd: poll with too many fds is EINVAL",
        sys_poll(pfd.as_mut_ptr() as u64, 999, 0),
        errno::EINVAL,
    );

    t.check_eq("fd: close", sys_close(fd), 0);
    t.check_eq("fd: closing twice is EBADF", sys_close(fd), errno::EBADF);
    t.check_eq("fd: reading a closed fd is EBADF", sys_read(fd, buf.as_mut_ptr() as u64, 4), errno::EBADF);

    // ── the open(2) flag encoding, as ring 3 actually spells it ──────────
    //
    // **Every constant in this block is the x86_64 value**, deliberately, and
    // that is the whole point of the block: these are the bits a musl-linked
    // guest binary sets, and aarch64 Linux keeps the 32-bit ARM fcntl values,
    // so four of them are a *permutation* rather than a shared encoding
    // (`akuma_syscalls_abi::open_flags`). `sys_openat` re-encodes the word
    // once at its boundary; these checks are what says the hop happened.
    //
    // Each has a negative control that is a **different errno**, not a
    // different success — an assertion that merely wanted "an error" would
    // pass against the untranslated kernel for all three.
    const O_DIRECTORY_X86: u64 = 0o200_000;
    const O_TMPFILE_X86: u64 = 0o20_200_000;
    const O_RDWR: u64 = 0o2;
    let etc = b"/etc\0";
    let missing_dir = b"/no-such-dir\0";
    let reg = b"/bin/busybox\0";
    let dfd = sys_openat(0, etc.as_ptr() as u64, O_DIRECTORY_X86, 0);
    // Untranslated, `0o200000` reads as `O_DIRECT` — a cache hint nothing here
    // implements — so this open succeeds either way. It is here to prove the
    // *next* check is not passing because `/etc` is unopenable.
    if t.check("fd: O_DIRECTORY on a directory opens", dfd >= FIRST_FILE_FD as u64) {
        t.check_eq("fd: ... and closes", sys_close(dfd), 0);
    }
    // The real assertion. Untranslated this is `O_DIRECT` on a regular file,
    // which this kernel ignores, and the open **succeeds** — the answer before
    // this pass, for every caller that ever passed the flag.
    // `/bin/busybox`, not `/etc/passwd`: the image has no passwd file, and the
    // first draft of this check used one. It passed — for the wrong reason,
    // against an `O_DIRECTORY` refusal that ran *before* the existence probe
    // and so answered `ENOTDIR` for a path that was simply absent. The
    // negative control is what said so, by returning `ENOENT` from the arm
    // that was meant to be broken.
    t.check_eq(
        "fd: O_DIRECTORY on a regular file is ENOTDIR",
        sys_openat(0, reg.as_ptr() as u64, O_DIRECTORY_X86, 0),
        errno::ENOTDIR,
    );
    // And a missing path is `ENOENT` even with the flag set — the ordering the
    // draft got wrong.
    t.check_eq(
        "fd: O_DIRECTORY on a missing path is ENOENT",
        sys_openat(0, missing_dir.as_ptr() as u64, O_DIRECTORY_X86, 0),
        errno::ENOENT,
    );
    // `O_TMPFILE` is the compound case — `__O_TMPFILE | O_DIRECTORY`, so it
    // inherits the permutation. Untranslated, the mask test is false and the
    // open falls through to the write-mode-on-a-directory guard, which answers
    // `EISDIR`: an error, and the wrong one, from a check that never ran.
    // (That fall-through is why this was never a *live* defect on this target
    // — apk's probe was refused by the neighbouring guard. It becomes one the
    // moment a folded `akuma-syscalls-glue` arm owns the refusal.)
    t.check_eq(
        "fd: O_TMPFILE is EINVAL, not EISDIR",
        sys_openat(0, etc.as_ptr() as u64, O_RDWR | O_TMPFILE_X86, 0),
        errno::EINVAL,
    );

    // ── a directory descriptor describes itself ──────────────────────────
    //
    // `fstat` on one answered **`EBADF`** until the `is_dir` field came off
    // the entry, so `fdopendir` — which musl builds on exactly this call —
    // could not open a directory at all. The `S_IFDIR` arm existed and was
    // unreachable: its discriminator was *synthetic*, not *directory*, so the
    // only descriptors ever reported as directories were `/proc` views, and
    // `/proc/meminfo` was reported as a zero-length one.
    //
    // busybox never noticed, which is why a 538-check suite and a ring-3
    // harness both missed it: it walks with `opendir(path)` and `lstat`.
    // Measured with an x86_64 musl probe (`fdopendir(dirfd) = NULL (Bad file
    // descriptor)`) before the fix.
    let dstat = sys_openat(0, etc.as_ptr() as u64, 0, 0);
    if t.check("fd: a directory opens", dstat >= FIRST_FILE_FD as u64) {
        let mut st = [0u8; STAT_SIZE];
        t.check_eq("fd: fstat(dirfd) succeeds", sys_fstat(dstat, st.as_mut_ptr() as u64), 0);
        // `st_mode` is at offset 24 in the x86_64 `struct stat`.
        let mode = u32::from_le_bytes(st[24..28].try_into().unwrap_or([0; 4]));
        t.check_eq(
            "fd: fstat(dirfd) reports S_IFDIR",
            u64::from(mode & 0o170_000),
            0o040_000,
        );
        // And the byte path refuses it *by errno*, not by accident: ext2 says
        // `NotAFile`, `fs_err_errno` says `EISDIR`. The `Err(_) => EIO` this
        // replaces is what the field's removal briefly turned this into.
        let mut b = [0u8; 4];
        t.check_eq(
            "fd: read(dirfd) is EISDIR",
            sys_read(dstat, b.as_mut_ptr() as u64, 4),
            errno::EISDIR,
        );
        t.check_eq("fd: closing the dirfd", sys_close(dstat), 0);
    }
    // The mirror image, from `fs::list_dir` rather than from a bool: a
    // `getdents64` on a regular file is `ENOTDIR`. The blanket `ENOENT` that
    // used to come out of this path was wrong in the way that misdirects —
    // "no such directory" for a path that plainly exists.
    let rfd = sys_openat(0, reg.as_ptr() as u64, 0, 0);
    if t.check("fd: a regular file opens", rfd >= FIRST_FILE_FD as u64) {
        let mut d = [0u8; 64];
        t.check_eq(
            "fd: getdents64 on a regular file is ENOTDIR",
            sys_getdents64(rfd, d.as_mut_ptr() as u64, 64),
            errno::ENOTDIR,
        );
        t.check_eq("fd: closing it", sys_close(rfd), 0);
    }

    // A missing file, and a path that is not a path.
    let missing = b"/does-not-exist\0";
    t.check_eq("fd: opening a missing file is ENOENT",
               sys_openat(0, missing.as_ptr() as u64, 0, 0), errno::ENOENT);
    t.check_eq("fd: a null path is EFAULT", sys_openat(0, 0, 0, 0), errno::EFAULT);

    // Write round-trip (2026-09-04): create a file, write to it, close (the
    // one point this target ever persists a write — see `fs`'s module
    // header), reopen read-only, and read the same bytes back.
    // `/write_probe.txt` is a name `mkdisk.sh` never creates, so this cannot
    // collide with a real fixture on the image.
    const O_WRONLY: u64 = 0o1;
    const O_CREAT: u64 = 0o100;
    const O_TRUNC: u64 = 0o1000;
    let wpath = b"/write_probe.txt\0";
    let wfd = sys_openat(0, wpath.as_ptr() as u64, O_WRONLY | O_CREAT | O_TRUNC, 0o644);
    if t.check("fd: O_CREAT open succeeds", wfd >= FIRST_FILE_FD as u64) {
        let msg = b"hello from amd64 write()\n";
        t.check_eq(
            "fd: write returns the byte count",
            sys_write_file(wfd, msg.as_ptr() as u64, msg.len() as u64),
            msg.len() as u64,
        );
        t.check_eq("fd: close after write persists it", sys_close(wfd), 0);

        let rfd = sys_openat(0, wpath.as_ptr() as u64, 0, 0);
        if t.check("fd: reopen the written file", rfd >= FIRST_FILE_FD as u64) {
            let mut rbuf = [0u8; 64];
            let n = sys_read(rfd, rbuf.as_mut_ptr() as u64, rbuf.len() as u64);
            t.check_eq("fd: read back the written length", n, msg.len() as u64);
            t.check("fd: read back the written bytes", &rbuf[..msg.len()] == msg);

            // `pread64` (2026-09-07). The descriptor is at EOF after the read
            // above, which is what makes the first two checks worth anything:
            // a `pread` implemented as seek-read-seek would return 0 here, and
            // one implemented as a plain read would return 0 *and* leave the
            // cursor somewhere else.
            let mut pbuf = [0u8; 64];
            t.check_eq(
                "fd: pread reads from its own offset, not the cursor",
                sys_pread64(rfd, pbuf.as_mut_ptr() as u64, 5, 6),
                5,
            );
            t.check("fd: pread returns the bytes at that offset", pbuf[..5] == msg[6..11]);
            t.check_eq(
                "fd: pread leaves the cursor where it was",
                sys_lseek(rfd, 0, 1),
                msg.len() as u64,
            );
            t.check_eq(
                "fd: pread past the end returns 0",
                sys_pread64(rfd, pbuf.as_mut_ptr() as u64, 8, 1_000_000),
                0,
            );
            t.check_eq(
                "fd: pread at a negative offset is EINVAL",
                sys_pread64(rfd, pbuf.as_mut_ptr() as u64, 8, u64::MAX),
                errno::EINVAL,
            );
            t.check_eq(
                "fd: pread on the console is ESPIPE, not EBADF",
                sys_pread64(1, pbuf.as_mut_ptr() as u64, 8, 0),
                errno::ESPIPE,
            );
            sys_close(rfd);
            t.check_eq(
                "fd: pread on a closed fd is EBADF",
                sys_pread64(rfd, pbuf.as_mut_ptr() as u64, 8, 0),
                errno::EBADF,
            );
        }
    }

    // A write to a read-only fd is refused, not silently accepted.
    let rofd = sys_openat(0, path.as_ptr() as u64, 0, 0);
    t.check_eq(
        "fd: writing a read-only fd is EBADF",
        sys_write_file(rofd, path.as_ptr() as u64, 4),
        errno::EBADF,
    );
    sys_close(rofd);

    // `dup` is a second name backed by its own reference: closing one of two
    // dups must leave the survivor open, and the underlying socket/pipe must
    // survive until the last name goes.
    //
    // **The cursor is NOT shared, and that is pinned here on purpose.** The
    // step 4b flip adopted `akuma-syscalls-glue`'s `dup`, which copies the
    // `KernelFile` by value — independent cursors, where POSIX (and this
    // target's deleted `FILES` table) shared the open file description. See
    // `sys_dup`'s header; this check fails if the divergence ever quietly
    // becomes something else.
    {
        let a = sys_openat(0, path.as_ptr() as u64, 0, 0);
        let b = sys_dup(a);
        t.check("fd: dup returns a new descriptor", b != a && !errno::is_err(b));
        let mut one = [0u8; 16];
        t.check_eq(
            "fd: reading through a dup advances that descriptor's cursor",
            sys_read(a, one.as_mut_ptr() as u64, 8),
            8,
        );
        // The divergence, asserted: `b`'s cursor is a copy taken at `dup`
        // time — 0 — not a window onto `a`'s.
        t.check_eq(
            "fd: a dup's cursor is independent (glue divergence, pinned)",
            sys_lseek(b, 0, 1),
            0,
        );
        sys_close(a);
        // The description's backing reference went with `b`, so it stays open.
        t.check_eq(
            "fd: closing one dup leaves the other open",
            sys_read(b, one.as_mut_ptr() as u64, 8),
            8,
        );
        sys_close(b);
        t.check_eq(
            "fd: closing the last dup releases it",
            sys_read(b, one.as_mut_ptr() as u64, 8),
            errno::EBADF,
        );
    }

    // `nonblock` is keyed by fd number now (the table's set, where `F_SETFL`
    // always mirrored it): a dup of a non-blocking descriptor **loses** the
    // flag. Also pinned — it is the other half of glue's `dup` model, see
    // `is_nonblocking`'s header.
    {
        let a = sys_openat(0, path.as_ptr() as u64, 0, 0);
        t.check_eq("fd: F_SETFL nonblock succeeds", sys_fcntl(a, 4, 0x800), 0);
        t.check_eq(
            "fd: F_GETFL reads the flag back",
            sys_fcntl(a, 3, 0),
            0x800,
        );
        let b = sys_dup(a);
        t.check_eq(
            "fd: a dup loses nonblock (glue divergence, pinned)",
            sys_fcntl(b, 3, 0),
            0,
        );
        t.check_eq("fd: the original keeps it", sys_fcntl(a, 3, 0), 0x800);
        sys_close(a);
        sys_close(b);
    }

    // Fill this row's descriptors, then check the next open is refused rather
    // than overwriting a live one. The budget is per-process now, so the
    // ceiling is this row's descriptor count — not, as it was until
    // 2026-09-06, one number shared by every task on the machine.
    const ROW_CAPACITY: usize = MAX_FDS - FIRST_FILE_FD;
    let mut opened = Vec::new();
    loop {
        let fd = sys_openat(0, path.as_ptr() as u64, 0, 0);
        if fd >= FIRST_FILE_FD as u64 && fd < MAX_FDS as u64 {
            opened.push(fd);
        } else {
            t.check_eq("fd: a full table is EMFILE", fd, errno::EMFILE);
            break;
        }
        if opened.len() > ROW_CAPACITY {
            t.check("fd: the table has a bound", false);
            break;
        }
    }
    t.check_eq(
        "fd: the row held exactly its capacity",
        opened.len() as u64,
        ROW_CAPACITY as u64,
    );
    for fd in opened {
        sys_close(fd);
    }

    // And it is empty again afterwards: a table that leaked would hand out a
    // descriptor above `FIRST_FILE_FD` here, which is exactly the shape of the
    // `apk`-twice-in-a-row failure the exit sweep was written for.
    let reopened = sys_openat(0, path.as_ptr() as u64, 0, 0);
    t.check_eq(
        "fd: closing every descriptor frees the whole row",
        reopened,
        FIRST_FILE_FD as u64,
    );
    sys_close(reopened);

    // A request past `MAX_IO` is clamped, not refused — real `read(2)`
    // semantics (see `sys_read`'s own comment): the byte count actually
    // returned is bounded by what the file has, not by what was asked for.
    // Uses its own appropriately-sized buffer rather than the 64-byte `buf`
    // above — a clamped read here legitimately delivers more than 64 bytes
    // (`/probe.txt` is 6623), and writing that many into a 64-byte
    // destination would be a real overflow. That is the caller's mistake to
    // avoid, not this kernel's to prevent: `len` is the caller's own
    // assertion about its buffer's size, exactly as on real Linux.
    let fd = sys_openat(0, path.as_ptr() as u64, 0, 0);
    let mut big = alloc::vec![0u8; 8192];
    t.check_eq(
        "fd: a request past MAX_IO is clamped to what the file has",
        sys_read(fd, big.as_mut_ptr() as u64, MAX_IO + 1),
        6623,
    );
    sys_close(fd);
}
