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
//! as "no inode: read by path". That was exactly this target's situation while
//! this module built its own descriptors; since 4b batch 2d it builds none —
//! `akuma-syscalls-glue`'s `openat` does, and it pins `(mount_id, inode)`
//! through `open_file_ids` where the mount can name them. The read and write
//! paths here still address the file by `f.path`, so the pin is carried and not
//! yet used; what it buys on the day they do is "unlinked but still open".
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
//! **`/proc` is the mounted `ProcFilesystem`, not a view of this module's**
//! (4b batch 2c). This kernel rendered that namespace itself — from its own
//! spawn table, intercepting ahead of the VFS in eight syscalls — while the
//! shared filesystem was mounted at the same path and served whatever the local
//! view declined. ~500 lines went with the deletion; one path outlived it —
//! `/proc/<pid>/fd/0`, whose semantics here were genuinely not the shared one's
//! — and since 2026-09-11 **nothing is left**: the shared stdin sink learned to
//! find a target whose real stdin is a pipe, which is what that path was
//! working around (see [`sys_openat`]). A `/proc` file is rendered by its
//! filesystem per
//! `read(2)`, so two reads can see two renders — Linux `seq_file`'s snapshot
//! semantics are the thing neither side implements.
//!
//! # Which arms are `akuma-syscalls-glue`'s (C1 step 4b)
//!
//! Seven so far, and the list is the progress report: `mkdirat`, `unlinkat`,
//! `renameat`, `symlinkat` and `readlinkat` (batch 1, path-only), `close`
//! (batch 2b) and **`openat` (batch 2d)** — the one that matters, because every
//! other file arm reads a descriptor `openat` produced. What is left here for
//! it is a preamble, and [`sys_openat`] says what each part of that preamble is
//! for. The arms still implemented in this file — `read`, `pread64`, `write`,
//! `lseek`, `fstat`, `getdents64`, `fcntl`, `statfs`, `newfstatat`, `access`,
//! `dup`, `dup3`, `pipe2`, `poll`, `select`, `ioctl`, `utimensat` — are what
//! the next batches take.
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

use akuma_exec_core::process::FileDescriptor;
// Named by this module's doc links and by nothing else since 4b batch 2d took
// the last `KernelFile::new` with it — every descriptor this module hands out
// for a file is now built by `akuma-syscalls-glue`. An intra-doc link resolves
// through an import like any other path, so the import stays and says why.
#[allow(unused_imports)]
use akuma_exec_core::process::KernelFile;
#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::fs;
use crate::serial;

// `fs_err_errno` — the local `FsError` -> errno table — left with the last of
// the arms that used it (`utimensat`, 4b batch 3c). `akuma-syscalls-glue`'s
// `fs_error_to_errno` is the one table now.

/// Does the path this descriptor names a directory?
///
/// The replacement for `Entry::is_dir`, which was a bool five constructors had
/// to set correctly and which `akuma-syscalls-glue` does not have — glue
/// refuses a write open of a directory at `open(2)` and otherwise lets the VFS
/// answer, so this target grew the same shape ahead of the fold.
///
/// `/proc` used to be asked of a synthetic view here, ahead of the VFS, because
/// this target rendered that namespace itself. It does not any more: the
/// mounted `ProcFilesystem` answers `metadata` for `/proc` and everything under
/// it, so one question suffices.
fn path_is_dir(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_dir)
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
    /// "The image was rejected by the loader" — `exec_load_errno`'s default
    /// for a rejection string it does not recognise.
    pub const ENOEXEC: u64 = (-8i64) as u64;
    /// The argument or environment list is longer than the initial stack
    /// builder will place ([`crate::loader::MAX_ARGV`] / `MAX_ENVP`). Linux
    /// answers this when `argv` exceeds `ARG_MAX`; this target answers it for
    /// the same reason, and answering it **at all** is the point — truncating
    /// instead handed `ld` a command line with its inputs missing and cost a
    /// session of looking at `collect2` (`docs/archive/RUST_TOOLCHAIN_AMD64.md`).
    pub const E2BIG: u64 = (-7i64) as u64;
    pub const ENOTDIR: u64 = (-20i64) as u64;
    pub const EISDIR: u64 = (-21i64) as u64;
    pub const EEXIST: u64 = (-17i64) as u64;
    // `ENOTEMPTY`/`EIO`/`EROFS`/`ENOSPC` left with `fs_err_errno` (4b batch 3c):
    // the folded arms map `FsError` through `akuma-syscalls-glue`'s table now.
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
    /// Too many symbolic links. The one errno here that is not a report of a
    /// failure but a *refusal to follow*: `open(link, O_NOFOLLOW)`. Named for
    /// the loop it usually means, and used by Linux for the flag as well.
    pub const ELOOP: u64 = (-40i64) as u64;

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
pub fn table_get(fd: u64) -> Option<FileDescriptor> {
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

/// Which end of the console `fd` names, if it names one.
///
/// **Two spellings reach the same device, and both have to work.** The
/// *by-number* one is this target's own: an **unbound** 0/1/2 is the console,
/// which is what a task with no stdio descriptors at all has — the boot
/// suite's kernel row. The *descriptor* one is the tree's:
/// `SharedFdTable::with_stdio`, the table every registered process starts
/// with, puts `Stdin`/`Stdout`/`Stderr` at 0/1/2, and
/// `akuma-syscalls-glue`'s `openat` hands out the same variants for
/// `/dev/tty`.
///
/// Only the first spelling existed here, asked for by every arm as
/// `fd < FIRST_FILE_FD && !is_bound(fd)` — a test that is **false for a
/// registered process**, because its 0/1/2 are bound to exactly those
/// variants. So `init`'s `write(1)` skipped the console path, fell through to
/// the file path, and answered `EBADF`: booted with `INIT=/bin/hello`, this
/// kernel printed `-- running /bin/hello --`, then `-- init exited --`, and
/// **not one byte of the program's output**. It survived because every other
/// process here gets its stdio from [`bind_stdio`]'s pipes and init happens to
/// be `sshd`, which writes to a socket.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum ConsoleEnd {
    /// The keyboard side: `read(2)` reaches `akuma-syscalls-glue`'s
    /// `Stdin`/`DevTty` arm, through the channel `crate::console` fills.
    Read,
    /// The screen side: `write(2)` reaches the serial port.
    Write,
}

/// See [`ConsoleEnd`]. A descriptor that names something else answers `None`,
/// which is what keeps the by-number rule below from claiming a redirected
/// 0/1/2 — the property `is_bound` was introduced for.
#[must_use]
pub fn console_end(fd: u64) -> Option<ConsoleEnd> {
    match table_get(fd) {
        Some(FileDescriptor::Stdin) => return Some(ConsoleEnd::Read),
        Some(FileDescriptor::Stdout | FileDescriptor::Stderr) => return Some(ConsoleEnd::Write),
        // A bound descriptor naming anything else: a pipe, a socket, a file.
        Some(_) => return None,
        None => {}
    }
    match fd {
        0 => Some(ConsoleEnd::Read),
        1 | 2 => Some(ConsoleEnd::Write),
        _ => None,
    }
}

// `clone_refs`/`release_desc` — the local refcount helpers `dup`/`dup2`/`dup3`
// used — left with those arms in 4b batch 3c. `akuma-syscalls-glue`'s
// `clone_fd_refs` (which `sys_dup`/`sys_dup3` call) is the one list now, and
// it has the same exhaustive-match property.

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

/// Give `sys_spawn`'s caller a handle on its child's output: a
/// `FileDescriptor::ChildStdout(pid)`, which glue's read arm resolves through
/// `get_child_channel(pid)`.
///
/// This was `alloc_pipe_fd(stdout_pipe, false, true)` until 2026-09-11 — a
/// `PipeRead` on the other end of the child's stdout pipe, with a stated
/// adopt-don't-clone rule because `bind_stdio` left that pipe's initial *reader*
/// unowned and cloning a second one stranded it (one leaked pipe per spawn
/// against `MAX_PIPES`, `proposals/AMD64_SPAWN_PIPE_LEAK.md`). None of that
/// applies now: the channel is reference-counted by `Arc` and the registration
/// is dropped by glue's own `close` arm, so there is no hand-managed end to
/// account for.
pub fn install_child_stdout(pid: u32) -> u64 {
    install(FileDescriptor::ChildStdout(pid))
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
        // The **other** spelling of the same node, and the one every folded
        // `akuma-syscalls-glue` arm produces: glue's `openat` answers
        // `/dev/null` and friends with a dedicated variant rather than with a
        // `File` carrying the path. Mapping them back to the node name here is
        // what lets `read`, `write`, `lseek` and `fstat` keep asking one
        // question — see [`dev_read`], which is the rule for both spellings.
        FileDescriptor::DevNull => Some("null"),
        FileDescriptor::DevZero => Some("zero"),
        FileDescriptor::DevUrandom => Some("urandom"),
        FileDescriptor::DevTty => Some("tty"),
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

/// `fcntl(fd, cmd, arg)` — **glue's arm** (4b batch 3c).
///
/// A forward, kept as a named function because ~5 sites in `usermode.rs` (the
/// per-spawn `F_SETFL` that makes a child's stdout non-blocking) and two boot
/// checks call it directly. `cmd` and `arg` need no translation: `F_DUPFD`=0,
/// `F_GETFD`=1 … `F_SETFL`=4 and `F_DUPFD_CLOEXEC`=1030 are identical on
/// x86_64 and asm-generic, and the only `arg` bit either side reads
/// (`O_NONBLOCK`) is not one of the four `open(2)` flags that permute.
///
/// Glue's arm is a superset of the one it replaces: it adds `F_GETLK`/
/// `F_SETLK`/`F_SETLKW` and `F_SETOWN`/`F_GETOWN` as accepted no-ops (nginx's
/// `ngx_spawn_process` treats a failing `F_SETOWN` as fatal *before* it forks).
/// `F_DUPFD` now allocates the lowest free fd from **0**, not from
/// [`FIRST_FILE_FD`] — Linux's "lowest available" — which is the same
/// convergence `openat` made in batch 2d.
pub fn sys_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    akuma_syscalls_glue::fs::sys_fcntl(fd as u32, cmd as u32, arg)
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

// `resolve_at` — the local `*at` dirfd ladder — left with the arms that used
// it (`openat` in batch 2d, `newfstatat` in 3b, `utimensat`/`access` in 3c).
// `akuma_syscalls_glue::fs::resolve_path_at` is the one ladder now, and it
// reads `Process::cwd` where this one hard-coded root.


/// `openat(dirfd, path, flags, mode)` — **glue's arm, behind a preamble**
/// (4b batch 2d).
///
/// The ~280 lines that used to be here are
/// `akuma_syscalls_glue::fs::openat_path`: the `AT_FDCWD` ladder, symlink
/// resolution, the `/dev` nodes, the existence and parent-directory probes,
/// `O_CREAT`/`O_TRUNC` through `write_file`, the inode pin, and the descriptor
/// allocation. What is left is four things that are **this target's**, and each
/// is here because it cannot be anywhere else.
///
/// **1. The flag word arrives in the x86_64 encoding and is re-encoded here,
/// once** — the same hop `Syscall::from_x86_64` makes for the syscall number,
/// one argument along. aarch64 Linux keeps the 32-bit ARM fcntl values, so four
/// bits are *permuted* between the two architectures (`O_DIRECTORY`↔`O_DIRECT`,
/// `O_NOFOLLOW`↔`O_LARGEFILE`); every other `O_*` bit is identical, which is
/// exactly why this file used to say the two encodings "happen to share the
/// same numeric encoding" and carry three separate `_X86` constants for the
/// ones that do not. Everything below this line — and everything glue reads out
/// of `KernelFile::flags` — is asm-generic, the one encoding every shared crate
/// in this tree speaks. See `akuma_syscalls_abi::open_flags`, whose tests pin
/// the trap: an untranslated x86_64 `O_TMPFILE` slips straight through glue's
/// refusal.
///
/// **2. `/proc/<pid>/fd/0` was the last path this kernel answered for itself,
/// and no longer is** (2026-09-11). It returned the **write end of the child's
/// stdin pipe**, which is not what the name means anywhere else: on Linux, and
/// in the shared `ProcFilesystem`, writing to a process's fd 0 delivers into
/// *its* stdin. The note here said closing it meant "teaching the shared stdin
/// sink to find the target's real stdin (its own `get_fd(0)`)", and that is
/// exactly what happened — `akuma_exec::process::write_to_process_stdin` now
/// routes to a `PipeRead` at the target's fd 0 through the `pipe_write` runtime
/// hook, *after* its ISIG filtering. So `sshd`'s bridge opens the real procfs
/// symlink through glue like everything else under `/proc`, and an `ssh`
/// session's `^C` becomes a `SIGINT` instead of a `0x03` byte.
///
/// **3. The refusals glue does not make.** `O_DIRECTORY`, `O_EXCL`,
/// `O_NOFOLLOW` and `O_CREAT`-on-a-directory are all enforced here and by
/// nothing in `akuma-syscalls-glue`, so folding them away would have deleted
/// four working checks — three of which this target grew *because* a real
/// program tripped over their absence. They are stated as a preamble rather
/// than pushed into glue because that arm is the AArch64 kernel's too, and
/// adding a refusal there is a behaviour change on a kernel this pass has no
/// loop to verify against. `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH2D.md` § "The
/// three flags" records what moving them would cost and buy.
///
/// **4. A block node is `ENODEV`, not a raw descriptor.** Glue serves
/// `/dev/vdX` as a `BlockDev` descriptor (`proposals/RAW_BLOCK_DEVICE_FD.md`);
/// nothing on this target reads one, so a folded `open("/dev/vda")` would hand
/// back a descriptor whose first `read` answers `EBADF` — a failure at the
/// wrong syscall, which is the `O_TMPFILE` lesson exactly. Refused here, at
/// `open`, which is the answer this target has always given.
///
/// The `dirfd` case is not a nicety: `apk` loads every signing key with
/// `openat(keys_dirfd, name)` after listing that directory, and while `dirfd`
/// was ignored each such open landed on a root-relative name that does not
/// exist — zero keys loaded, and every fetched index reported `UNTRUSTED
/// signature` no matter how correct the fetch and the keys were. It is
/// `akuma_syscalls_glue::fs::resolve_path_at` that answers it now, for both
/// kernels, and [`resolve_at`] — this module's own ladder, still used by the
/// arms that have not folded — is no longer in the `open` path at all. Two
/// differences come with that swap and both are gains: a bogus negative
/// `dirfd` is `EBADF` rather than `ENOTDIR`, and `AT_FDCWD` resolves against
/// the process's `cwd` rather than against `/` — `/` for every process until a
/// `chdir` moves it, which the target has served since 2026-09-12.
pub fn sys_openat(dirfd: u64, path: u64, flags_: u64, mode: u64) -> u64 {
    let flags = akuma_syscalls_abi::open_flags::x86_64_to_aarch64(flags_ as u32);

    // 1024, matching glue's own `copy_from_user_str` bound, so the preamble
    // cannot refuse a path the arm behind it would have accepted. It was 256
    // ([`path_from_user`]), which is what every unfolded arm still uses.
    let Some(raw) = crate::uaccess::read_cstr(path, 1024)
        .and_then(|b| alloc::string::String::from_utf8(b).ok())
    else {
        return errno::EFAULT;
    };

    // Resolved with **glue's** ladder, not this module's, so the refusals below
    // are asked of exactly the path `openat_path` will open. Handing the
    // resolved path back to it costs a `canonicalize` and no second lookup:
    // it is absolute, so glue's `dirfd_base` returns before it touches the
    // table.
    let Ok(resolved) = akuma_syscalls_glue::fs::resolve_path_at(dirfd as i32, &raw) else {
        return errno::EBADF;
    };

    // A **block** node is refused rather than served — see the header. A
    // table lookup, no I/O, so it is asked unconditionally.
    if let Some(node) = akuma_vfs_glue::dev_node(&resolved)
        && node.is_block
    {
        return errno::ENODEV;
    }

    // `O_NOFOLLOW`: Linux's `ELOOP` on a final component that is a symlink.
    //
    // This used to be spelled as *skipping* the symlink walk, which is the
    // weaker half of the flag and answered `ENOENT` — the walk skipped, the
    // existence probe then run against the link inode, which ext2 reports as
    // `NotAFile`. Glue resolves unconditionally, so the choice here was between
    // Linux's answer and silently following a link a caller asked not to
    // follow. `is_symlink` is the exact predicate: `resolve_symlinks` reads the
    // link off the *whole* path and never walks intermediate components, so
    // "the path glue would rewrite" and "the final component is a link" are the
    // same question in this tree.
    if flags & open_flags::O_NOFOLLOW != 0 && akuma_vfs_glue::is_symlink(&resolved) {
        return errno::ELOOP;
    }

    // The three that need to know what is already there. One `metadata` for all
    // of them, taken only when a flag that reads it is set — an `open` with
    // none of them pays nothing.
    let creating = flags & open_flags::O_CREAT != 0;
    let wants_dir = flags & open_flags::O_DIRECTORY != 0;
    let excl = creating && flags & open_flags::O_EXCL != 0;
    if creating || wants_dir {
        let md = fs::metadata(&resolved);
        let is_dir = md.as_ref().is_ok_and(|m| m.is_dir);
        let present = md.is_ok();
        // `O_CREAT` on a path that is already a directory must fail, not
        // silently start writing a same-named file. Glue refuses a *write*
        // open of a directory (`may_open`'s answer) and this is the other
        // half: `open("/etc", O_CREAT)` with no access mode gets past that one.
        if creating && is_dir {
            return errno::EISDIR;
        }
        // `O_DIRECTORY` on something that is not one. **Below the existence
        // question, not above it**, and the first draft of this check had it
        // above: `open("/no/such/path", O_DIRECTORY)` must be `ENOENT`, because
        // "there is no such file" outranks "and it would not have been a
        // directory". Placed early it answered `ENOTDIR` for every missing path
        // that carried the flag — which is the sort of thing that sends a
        // caller looking for a directory it never asked about. Glue answers the
        // `ENOENT`; this arm only refuses what is there.
        //
        // The flag was **never read at all** before the word was re-encoded at
        // the top: the bit ring 3 sets is `0o200000`, which in the encoding
        // every shared crate here uses is `O_DIRECT`, so
        // `open("/bin/busybox", O_RDONLY|O_DIRECTORY)` handed back a working
        // descriptor on a regular file.
        if wants_dir && present && !is_dir {
            return errno::ENOTDIR;
        }
        // `O_CREAT | O_EXCL` on a path that already exists is `EEXIST`, which
        // is the *whole* contract of `O_EXCL`: it is how a caller claims a lock
        // file or an atomic temp name, and succeeding anyway tells two of them
        // they both won.
        if excl && present {
            return errno::EEXIST;
        }
    }

    // **The row's descriptor ceiling.** Glue's `alloc_fd` has none — a
    // `BTreeMap` grows — and here that is not merely a policy difference:
    // [`MAX_FDS`] is also this module's *lookup* bound ([`table_get_in`],
    // [`is_bound`] and [`is_nonblocking`] all refuse a number at or above it),
    // so a descriptor glue handed out past the ceiling would be a successful
    // `open` that every later syscall answers `EBADF` for. Asked here, one
    // lock, where [`install`] asks it for every descriptor this module still
    // allocates itself.
    if cur_table().table.lock().len() >= MAX_FDS {
        return errno::EMFILE;
    }

    // `mode` reaches the filesystem for the first time here: this arm ignored
    // it (`_mode`) and every file this target created came out with whatever
    // `write_file` picked. Glue `chmod`s a created file to `mode & 0o7777`,
    // which is what makes a `tcc`-built binary executable without a `chmod +x`.
    akuma_syscalls_glue::flat(akuma_syscalls_glue::fs::openat_path(
        dirfd as i32,
        &resolved,
        flags,
        mode as u32,
    ))
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

/// `dup(fd)` — x86_64 32. **Glue's arm** (4b batch 3c).
///
/// A named forward: two boot checks call it directly. The value-copy
/// divergence stays pinned — glue copies the `KernelFile` by value, so two
/// descriptors onto one file get *independent cursors* (POSIX shares the open
/// file description; fixing that is an `Arc` inside `FileDescriptor::File`).
/// One convergence: glue allocates the lowest free fd from **0**, where this
/// arm started at [`FIRST_FILE_FD`] — the same "lowest available includes a
/// closed 0/1/2" move `openat` made in batch 2d.
pub fn sys_dup(fd: u64) -> u64 {
    akuma_syscalls_glue::fs::sys_dup(fd as u32)
}

/// `dup2(oldfd, newfd)` — x86_64 33, the legacy spelling asm-generic dropped.
/// A preamble over glue's `sys_dup3`.
///
/// Two things glue's `dup3` arm does not do that `dup2` must:
/// - **`oldfd == newfd` returns `newfd` unchanged, without closing it**, and
///   is not an error — where `dup3` answers `EINVAL`. This is the whole
///   behavioural difference between the two calls.
/// - **`newfd` past [`MAX_FDS`] is `EBADF`.** Glue's table is a `BTreeMap` with
///   no ceiling; this target's [`table_get_in`] refuses a number at or above
///   `MAX_FDS`, so a descriptor glue placed there would be a successful `dup2`
///   that every later syscall answered `EBADF` for — the trap batch 2d found
///   for `openat`.
pub fn sys_dup2(oldfd: u64, newfd: u64) -> u64 {
    if newfd >= MAX_FDS as u64 {
        return errno::EBADF;
    }
    if oldfd == newfd {
        return if table_get(oldfd).is_some() { newfd } else { errno::EBADF };
    }
    akuma_syscalls_glue::fs::sys_dup3(oldfd as u32, newfd as u32, 0)
}

/// `dup3(oldfd, newfd, flags)` — x86_64 292. **Glue's arm** (4b batch 3c)
/// behind the `MAX_FDS` ceiling `sys_dup2` explains. `O_CLOEXEC` in `flags` is
/// honoured by glue; the bit is not one of the four `open(2)` flags that
/// permute, so `flags` passes through raw.
pub fn sys_dup3(oldfd: u64, newfd: u64, flags: u64) -> u64 {
    if newfd >= MAX_FDS as u64 {
        return errno::EBADF;
    }
    akuma_syscalls_glue::fs::sys_dup3(oldfd as u32, newfd as u32, flags as u32)
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
pub fn sys_pipe2(fds: u64, flags: u64) -> u64 {
    // **The `MAX_PIPES` preamble.** Glue's `pipe_create` is a `BTreeMap` with
    // no ceiling; this target keeps one because each pipe is up to 64 KiB of
    // kernel buffer on a userspace request (`crate::pipe::MAX_PIPES`). The
    // `sys_spawn` path gates through `crate::pipe::alloc`; this is the same
    // gate for the `pipe2(2)` path.
    if crate::pipe::at_capacity() {
        return errno::ENFILE;
    }
    akuma_syscalls_glue::pipe::sys_pipe2(fds, flags as u32)
}

/// `close(fd)`. Closing a console descriptor succeeds and does nothing — a
/// `close(fd)` — **`akuma-syscalls-glue`'s arm**, by the name this module's
/// callers already use.
///
/// A forward, not a second implementation: the dispatcher hands `close`
/// straight to glue (`usermode.rs`), and this exists because ~40 kernel-side
/// call sites — every self-test that opens something — spell it this way. It
/// takes a `u64` because they do.
///
/// **The one divergence the fold adopted, stated where it changed:** an
/// *unbound* 0/1/2 used to answer 0 here and do nothing, so "a program that
/// closes stdin does not then find the kernel refusing to print". Glue answers
/// `EBADF` for an fd its table does not hold, console number or not — which is
/// Linux's answer, and is now reachable only by a task whose stdio is unbound
/// (the boot row; every registered process has the triple, per
/// `SharedFdTable::with_stdio`). The console itself is unaffected: it is
/// answered by number in `read`/`write` through [`console_end`], not by a
/// descriptor this could close.
pub fn sys_close(fd: u64) -> u64 {
    akuma_syscalls_glue::fs::sys_close(fd as u32)
}

/// `read(fd, buf, len)` — **`akuma-syscalls-glue`'s arm** (4b batch 3a), with
/// no preamble at all since the console channel landed.
///
/// # The console was the arm that stayed
///
/// It is worth saying what used to be here, because the reason it had to be is
/// the shape this series keeps meeting. `read_console` served fd 0 by
/// **polling the UART from the reading thread itself**, and the preamble that
/// routed both console spellings to it could not simply be deleted: glue's
/// `Stdin`/`DevTty` arm reads a `ProcessChannel`, no process on this target
/// had one, and its `current_channel().is_none()` fallback returns
/// `Process::read_stdin`'s zero — an EOF on the console a shell is reading
/// from. `INIT=/bin/sh` on the serial line would have exited at its first
/// prompt.
///
/// **That claim went stale before it was removed**, and the staleness was the
/// worse state of the two: the exit channel adopted 2026-09-10 registers
/// through `register_channel(task_slot, ..)`, which is exactly where
/// `current_channel()` falls back to, so every registered process here had a
/// channel — an *exit* channel, whose stdin FIFO nothing fills and whose
/// `is_terminal()` is `true`. A read reaching glue's arm would not have
/// reported EOF; it would have **parked forever**.
///
/// What closes it is a producer: `crate::console` bridges
/// `input::getb` into a real `ProcessChannel` and back out again, and
/// `usermode::register_exec_process` hands that channel to any process whose
/// fd 0 is a `FileDescriptor::Stdin`. Glue's arm then serves the console with
/// its own `EAGAIN`, its `EINTR`, its raw/cooked branch and its `EPOLLET` edge
/// re-arm — four things `read_console` either lacked or had to grow a copy of.
///
/// `/dev/tty` needs no arm of its own either, and now for the right reason:
/// glue's `openat` gates it on `current_channel().is_some_and(|c|
/// c.is_terminal())`, which a console process satisfies, so the descriptor
/// exists and glue's own `Stdin | DevTty` arm serves it — the same channel,
/// the same line discipline.
///
/// # And the clamp
///
/// `len` is bounded to [`MAX_IO`] before the call, not after. Glue's `File`,
/// `BlockDev` and `Socket` arms clamp themselves, but its pipe, stdin and
/// `/dev/zero` arms allocate `count` kernel bytes for whatever ring 3 asked
/// for — `validate_user_ptr` bounds that to a *mapped* range, which a process
/// holding a large mapping can make large. This arm has always clamped and a
/// fold must not drop a bound. Clamped rather than refused, for the reason
/// `apk` taught: an oversized count is ordinary POSIX and a short read is the
/// contract (a 119-byte `/etc/apk/repositories` read through a 128 KiB buffer
/// came back `EINVAL` before that was understood, and `apk update` never
/// reached the network).
///
/// # What the fold gained
///
/// - **Reads go by inode.** Glue asks `read_at_open_file`, so a descriptor
///   resolves its bytes through the `(mount id, inode)` `open(2)` pinned
///   instead of walking the path again on every call — and an
///   unlinked-but-open fd keeps reading.
/// - **A `read` on a real file runs BKL-free** (`VfsBklGuard` scoped to that
///   arm) rather than under the lock.
/// - **`EPOLLET` edges are re-armed** after a pipe or socket read, which this
///   arm never did.
pub fn sys_read(fd: u64, buf: u64, len: u64) -> u64 {
    if len == 0 {
        return 0;
    }
    akuma_syscalls_glue::fs::sys_read(fd, buf, len.min(MAX_IO) as usize)
}

/// `pread64(fd, buf, count, offset)` — x86_64 syscall 17. **Glue's arm** (4b
/// batch 3a) behind the errno this target refuses to give up.
///
/// A read from an explicit offset that **does not move the descriptor's
/// cursor**. That is the whole difference from [`sys_read`], and it is why the
/// two are separate arms in glue as well.
///
/// # `ESPIPE` is the preamble
///
/// A pipe, socket or console descriptor has no offset to read from, and Linux
/// says `ESPIPE` for that — a *seekability* answer, distinct from "no such
/// descriptor". Glue's arm answers `_ => EBADF` for all three, and the
/// difference is not cosmetic: **musl's `FILE` layer falls back to `read()` on
/// `ESPIPE` and gives up on `EBADF`**, so the wrong one turns a working
/// unseekable stream into a closed file. Moving this into glue is a behaviour
/// change on the AArch64 kernel, whose verification loop does not run on this
/// machine, so it stays here and is stated rather than assumed absent.
///
/// Asked ahead of the table lookup, so the answer is about the *kind* of
/// descriptor rather than about whether a file happens to sit behind it.
///
/// # Why this exists at all
///
/// It was not dispatched until 2026-09-07, so every `pread` on this target
/// returned `ENOSYS`. `scripts/mem_suite.py`'s `mmapsum` is what found it —
/// its `read()` reference arm is a `pread` loop and it aborted at offset 0
/// before comparing anything — but the probe is only the messenger. `pread` is
/// how every archive reader, every `rustc` metadata load and every threaded
/// reader of a shared description reaches into a file, precisely because it
/// needs no lock around a seek-then-read pair.
///
/// A negative `offset` is `EINVAL` — glue's first line, and it must stay a
/// refusal: it arrives as a `u64` from a ring-3 register, so
/// `0xFFFF_FFFF_FFFF_FFFF` used as a `usize` index would sail past every bound
/// check by being larger than any file.
pub fn sys_pread64(fd: u64, buf: u64, len: u64, offset: u64) -> u64 {
    if len == 0 {
        return 0;
    }
    // See the header. A `/dev` character node **is** seekable — `pread` on
    // `/dev/zero` is ordinary — so the nodes are deliberately not in this
    // guard; glue serves all three, and the offset changes nothing for any of
    // them.
    if socket_index(fd).is_some()
        || pipe_read_id(fd).is_some()
        || pipe_write_id(fd).is_some()
        || console_end(fd).is_some()
    {
        return errno::ESPIPE;
    }
    akuma_syscalls_glue::fs::sys_pread64(
        fd as u32,
        buf,
        // Clamped for [`sys_read`]'s reason: glue's `/dev/zero` and
        // `/dev/urandom` arms allocate whatever ring 3 asked for.
        len.min(MAX_IO) as usize,
        offset.cast_signed(),
    )
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
/// A `/proc` file is rendered by its filesystem per read; a real file's page
/// comes off the VFS.
pub fn file_bytes_at(fd: u64, offset: usize, dst: &mut [u8]) -> Option<usize> {
    // A directory needs no guard of its own: `fs::read_at` below refuses one
    // (`NotAFile`), and this function's contract is already `None` for
    // anything a `MAP_PRIVATE` file mapping must not be served from.
    let path = table_with(fd, |d| match d {
        FileDescriptor::File(f) => Some(f.path.clone()),
        _ => None,
    })??;
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
    // "Not a directory", asked of the path. A `/proc` *file* stays
    // mappable exactly as it was — [`file_bytes_at`] serves it from a fresh
    // render — and a synthetic `/proc` directory is refused.
    table_with(fd, |d| match d {
        FileDescriptor::File(f) => Some(f.path.clone()),
        _ => None,
    })
    .flatten()
    .is_some_and(|p| !path_is_dir(&p))
}

/// Is a `write(2)` on `fd` refused by the descriptor's **access mode**?
///
/// `Some(EBADF)` when `fd` names a regular file opened `O_RDONLY`, `None`
/// otherwise — including for every descriptor kind that is not a file, which
/// glue answers for itself.
///
/// # This is a gap in the shared arm, not a divergence this target wanted
///
/// `akuma_syscalls_glue::fs::sys_write`'s `File` arm does not look at
/// `KernelFile::flags` at all, so on the AArch64 kernel a descriptor obtained
/// with `open(path, O_RDONLY)` is a **write capability**: the bytes reach the
/// filesystem and `write(2)` reports success. `O_ACCMODE` is checked at
/// `open(2)` for whether the *file* may be written (`may_open`) and then never
/// again for whether this *description* may.
///
/// This target has always refused it (the arm this replaces opened with the
/// test), and 4b batch 3a's fold would have imported the gap silently — which
/// is the one thing the fold rules forbid. So the refusal is asked here, ahead
/// of the delegation.
///
/// **It is not moved into glue** for the reason batch 2d gives for `openat`'s
/// three flags: it is a behaviour change on the other kernel and the AArch64
/// verification loop does not run on this machine. Recorded as an open issue in
/// `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH3.md` rather than fixed blind — a
/// program that has been writing through a read-only descriptor would start
/// getting `EBADF`, and that program deserves a boot behind the change.
///
/// `O_ACCMODE == 0` is `O_RDONLY`; the word is asm-generic by the time it is
/// stored (`sys_openat` re-encodes it), so [`open_flags`] is the right table.
#[must_use]
pub fn write_mode_refusal(fd: u64) -> Option<u64> {
    table_with(fd, |d| match d {
        FileDescriptor::File(f) => (f.flags & open_flags::O_ACCMODE == 0).then_some(errno::EBADF),
        _ => None,
    })
    .flatten()
}

/// `write(fd, buf, len)` on a **file** descriptor — **glue's arm** (4b batch
/// 3a), by the name this module's self-tests already use.
///
/// A forward, not a second implementation, and the same shape as
/// [`sys_close`]: the dispatcher's `write` arm is `usermode::sys_write`, which
/// keeps the serial console and hands everything else here. The
/// [`write_mode_refusal`] is asked on this path too, so a kernel-side check
/// gets the answer ring 3 gets.
///
/// Named `_file` because that is what its callers write to; it will serve a
/// pipe, a socket or a `/dev` node just as well, since glue's arm dispatches on
/// the descriptor and not on the name of this function.
pub fn sys_write_file(fd: u64, buf: u64, len: u64) -> u64 {
    if let Some(e) = write_mode_refusal(fd) {
        return e;
    }
    akuma_syscalls_glue::fs::sys_write(fd, buf, len as usize)
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

/// Is there console input waiting? [`console_take_byte`]'s non-destructive
/// twin, and it has to consult the same source for the same reason: once the
/// pump runs, a typed line lives in the channel and the UART reads empty.
fn console_has_input() -> bool {
    if crate::console::pump_running() {
        crate::console::has_input()
    } else {
        crate::input::has_byte()
    }
}


/// `lseek(fd, offset, whence)` — **served by `akuma-syscalls-glue`** (4b batch
/// 3a), behind one arm this kernel keeps.
///
/// # The one arm that stays
///
/// A `/dev` character node. Linux seeks one to exactly the offset asked for —
/// `/dev/null` and `/dev/zero` are contentless and infinite, so no position is
/// out of range and none of them means anything — and glue answers `0` for
/// `DevNull`/`DevZero` and `ESPIPE` for `DevUrandom`/`DevTty`, which has no
/// arm at all. Handing this over would trade a Linux answer for a matching
/// one; fixing it in glue is a behaviour change on the AArch64 kernel, whose
/// verification loop does not run on this machine (batch 2d records the same
/// call for `openat`'s three flags). So it is a stated divergence, asked with
/// one table lookup, ahead of the delegation.
///
/// The cursor stays at 0 for all three whences because nothing moves it: the
/// read and write arms never touch a node's `position`.
///
/// # What the fold gained
///
/// - **`SEEK_END` by inode.** Glue asks `metadata_open_file`, so an
///   unlinked-but-open descriptor still knows how big it is instead of being
///   silently treated as a zero-length file.
/// - **`rewinddir` works.** Glue clears `KernelFile::dir_cache` on a seek to
///   0; this arm left the first snapshot in place for the life of the fd, so a
///   directory re-read after `rewinddir` replayed the old listing forever.
/// - **A console descriptor is `ESPIPE`, not `EBADF`.** The kind answer rather
///   than the closed-descriptor one, which is the distinction
///   [`sys_pread64`]'s header is about. An fd that names *nothing* is still
///   `EBADF` — glue separates the two, where this arm's console guard could
///   not.
pub fn sys_lseek(fd: u64, offset: u64, whence: u64) -> u64 {
    // See the header. Ahead of the delegation, and one table lookup with no I/O
    // behind it.
    //
    // **`/dev/tty` is excluded**, and it is the reason this test is no longer
    // `is_some()`. A terminal is not seekable and Linux answers `ESPIPE` for
    // one; glue's arm already does exactly that (its `_ =>` after the
    // `DevNull`/`DevZero` and `File`/`BlockDev` arms). Before the console
    // channel landed the exclusion was unnecessary because a `DevTty`
    // descriptor could not exist here — glue's `openat` refused `/dev/tty` with
    // `ENODEV` for want of a terminal channel. It can exist now, and without
    // this it would seek to whatever offset it was handed.
    if dev_node_of(fd).is_some_and(|n| n != "tty") {
        const SEEK_SET: u64 = 0;
        const SEEK_CUR: u64 = 1;
        const SEEK_END: u64 = 2;
        if !matches!(whence, SEEK_SET | SEEK_CUR | SEEK_END) {
            return errno::EINVAL;
        }
        let delta = offset.cast_signed();
        return if delta < 0 { errno::EINVAL } else { delta as u64 };
    }
    akuma_syscalls_glue::fs::sys_lseek(
        // Truncating rather than refusing a number past `u32`: glue's own
        // dispatch spells `args[0] as u32`, so a folded arm must decode the
        // register the same way the AArch64 kernel does or the two disagree
        // about which fd `lseek(0x1_0000_0003, …)` names. It resolves to
        // nothing in either case — `EBADF`.
        fd as u32,
        // Signed on the wire. A negative seek from `SEEK_CUR`/`SEEK_END` is
        // legal and must not read as an enormous unsigned offset.
        offset.cast_signed(),
        whence as i32,
    )
}

/// `getdents64(fd, dirp, count)` — **`akuma-syscalls-glue`'s arm** (4b batch
/// 3a), by the name this module's self-tests already use.
///
/// A forward, not a second implementation: the dispatcher hands the syscall
/// straight to glue (`usermode.rs`), exactly as it does `close`. This kernel
/// keeps **no** preamble for it — the record layout was already shared
/// (`akuma_syscalls_linux::dirent`) and the snapshot field was already
/// `KernelFile::dir_cache`, so the two arms differed only in what they got
/// wrong.
///
/// # What the fold gained
///
/// - **A `/dev` node lists as a device.** `DirEntry` carries only
///   `is_dir`/`is_symlink`, so this arm reported every character node as
///   `DT_REG`; glue asks `dev_node_named` for the real `d_type` (one string
///   compare for every *other* directory, hoisted out of the per-entry map).
/// - **The user buffer is validated up front** rather than only at the copy,
///   so a partly-unmapped `dirp` is `EFAULT` before a directory is read off
///   the disk.
///
/// The 64 KiB clamp this arm carried was **moved into glue rather than
/// dropped**: `count` is a ring-3 number and it is that function's kernel
/// allocation. Folding must not lose a bound.
pub fn sys_getdents64(fd: u64, dirp: u64, count: u64) -> u64 {
    akuma_syscalls_glue::fs::sys_getdents64(fd as u32, dirp, count as usize)
}

/// The size of the x86_64 `struct stat` this target writes — 144 bytes, pinned
/// by `offset_of!` assertions in `akuma_syscalls_abi::stat`. Only the boot
/// suite still names it, to size the buffers it reads fields back out of.
#[cfg(not(feature = "no-tests"))]
const STAT_SIZE: usize = core::mem::size_of::<akuma_syscalls_abi::stat::X8664>();

/// `fstat(fd, statbuf)` — **glue's `fstat_fill`** (4b batch 3b) behind two
/// answers this target keeps, then the x86_64 layout conversion.
///
/// `struct stat` is the **third architecture vocabulary**, after the syscall
/// numbers (C1 step 1) and `open(2)`'s flag word (4b prerequisites): x86_64 is
/// 144 bytes with `st_nlink` 8-wide at offset 16 and `st_mode` at 24, where
/// `asm-generic` is 128 with `st_nlink` 4-wide at 20 and `st_mode` at 16. Glue
/// fills the asm-generic `Stat`; `akuma_syscalls_abi::stat::to_x86_64` re-lays
/// it. This used to be a hand-rolled `encode_stat` writing literal offsets into
/// a `[u8; 144]`, under a comment calling it "proposal item 5 territory" — that
/// item is `akuma_syscalls_abi::stat`, with `offset_of!` assertions on every
/// one of those literals.
///
/// # The preamble: the by-number console
///
/// An **unbound** 0/1/2 is the serial console (a kernel thread, the boot row
/// before its stdio was wired). Glue resolves `current_process_shared().get_fd`
/// and would answer `EBADF` for a descriptor that is not in a table at all;
/// this target has always reported `S_IFCHR` for it, which is what `isatty(3)`
/// on those numbers needs. A *bound* Stdin/Stdout/Stderr goes to glue, which
/// gives the richer answer (`st_rdev` = `makedev(136, 0)`, the pts major).
pub fn sys_fstat(fd: u64, statbuf: u64) -> u64 {
    if console_end(fd).is_some() && !is_bound(fd) {
        // `S_IFCHR | 0620`, size 0 — the answer the old `encode_stat` gave, in
        // the x86_64 layout the converter also produces.
        let g = akuma_syscalls_linux::Stat {
            st_mode: 0o020_620,
            st_nlink: 1,
            st_blksize: 4096,
            ..Default::default()
        };
        let x = akuma_syscalls_abi::stat::to_x86_64(&g);
        return if crate::uaccess::write_val(statbuf, x) { 0 } else { errno::EFAULT };
    }
    match akuma_syscalls_glue::fs::fstat_fill(fd as u32) {
        Ok(g) => {
            let x = akuma_syscalls_abi::stat::to_x86_64(&g);
            if crate::uaccess::write_val(statbuf, x) { 0 } else { errno::EFAULT }
        }
        Err(e) => e,
    }
}

/// `statfs(path, buf)` — x86_64 137, **glue's arm** (4b batch 3b).
///
/// `struct statfs` is `asm-generic`'s on both architectures — 120 bytes, three
/// offsets `const`-asserted in `akuma-syscalls-linux` — so this is a straight
/// forward, no layout hop. Glue's `fs_magic` table is richer than this
/// target's was (`proc`, `tmpfs`, `overlay`), which `df` prints as the mount's
/// type. `busybox df` calls it once per line it read from `/proc/mounts`.
pub fn sys_statfs(path: u64, buf: u64) -> u64 {
    akuma_syscalls_glue::flat(akuma_syscalls_glue::fs::sys_statfs(path, buf))
}

/// `fstatfs(fd, buf)` — x86_64 138, **glue's arm** (4b batch 3b). A descriptor
/// with no path (socket, pipe, stdio) reports the root mount, which is Linux's
/// answer for an fd on a filesystem with no name to resolve.
pub fn sys_fstatfs(fd: u64, buf: u64) -> u64 {
    akuma_syscalls_glue::fs::sys_fstatfs(fd as u32, buf)
}

/// `newfstatat(dirfd, path, statbuf, flags)` — and, via two thin shims in the
/// dispatcher, the x86-only `stat(2)` and `lstat(2)`. **Glue's
/// `newfstatat_fill`** (4b batch 3b) plus the x86_64 `struct stat` conversion
/// (see [`sys_fstat`]).
///
/// `AT_EMPTY_PATH` (stat the fd itself) redirects to [`sys_fstat`] here rather
/// than in glue, because that is where the fd-vs-path branch has always been.
/// Every other path resolves through `akuma_syscalls_glue::fs::resolve_path_at`
/// — `Process::cwd`-relative; `/` until a `chdir` moves it (served since
/// 2026-09-12).
pub fn sys_newfstatat(dirfd: u64, path: u64, statbuf: u64, flags: u64) -> u64 {
    const AT_EMPTY_PATH: u64 = 0x1000;
    let Some(raw) = path_from_user(path) else {
        return errno::EFAULT;
    };
    if raw.is_empty() {
        if flags & AT_EMPTY_PATH != 0 {
            return sys_fstat(dirfd, statbuf);
        }
        return errno::ENOENT;
    }
    match akuma_syscalls_glue::fs::newfstatat_fill(dirfd as i32, &raw, flags as u32) {
        Ok(g) => {
            let x = akuma_syscalls_abi::stat::to_x86_64(&g);
            if crate::uaccess::write_val(statbuf, x) { 0 } else { errno::EFAULT }
        }
        Err(e) => e,
    }
}

/// `statx(dirfd, path, flags, mask, buf)` — x86_64 332. **Glue's arm** (4b
/// batch 3b), and this one needs no preamble at all: `struct statx` is
/// arch-neutral (256 bytes, same offsets on both), so there is nothing to
/// convert. This target had no `statx` arm before — every call returned
/// `ENOSYS` — which a modern coreutils `stat` and Rust's `std::fs` both reach
/// for before falling back to `newfstatat`.
pub fn sys_statx(dirfd: u64, path: u64, flags: u64, mask: u64, buf: u64) -> u64 {
    akuma_syscalls_glue::flat(akuma_syscalls_glue::fs::sys_statx(
        dirfd as i32,
        path,
        flags as u32,
        mask as u32,
        buf,
    ))
}

/// `poll(fds, nfds, timeout_ms)` — x86_64 syscall 7. **Glue's `ppoll` arm**
/// (4b batch 4b) behind a millisecond-to-`Option<u64>` conversion.
///
/// `poll(2)` and `ppoll(2)` differ in exactly one thing — how they spell the
/// timeout — so this arm converts and [`sys_ppoll`] does not. `int timeout` in
/// milliseconds arrives sign-extended in a 64-bit register: `< 0` is "wait
/// forever" (`None`), `0` is one non-blocking pass, and anything else scales to
/// microseconds. `as i32` is what recovers the sign, and it is why `-1` does not
/// read as an 18-quintillion-millisecond wait.
///
/// # What the fold changed, and it is all of the interesting part
///
/// The loop this replaced was a **yield budget**: it re-scanned every fd, called
/// `crate::sched::yield_now()`, and counted laps, approximating a finite timeout
/// at "200 laps per millisecond, capped at two million". Glue's loop is
/// `akuma_net_yarn::WaitMachine` under `WaitPolicy::epoll` — it registers the
/// calling thread as a **waker** on each polled resource, parks (capped at the
/// 10 ms blocking-poll interval so a resource with no waker is still re-checked),
/// compares against a real deadline, and honours
/// `should_interrupt_blocking_syscall` for `EINTR`. Four consequences, each of
/// them a fix rather than a wash:
///
/// - **A finite timeout is now a timeout.** The budget approximated one with a
///   lap count on a target whose lap cost is not fixed; `clock::uptime_us` is
///   real here and the machine reads it.
/// - **`nfds == 0` blocks.** It used to yield once and return 0. musl's
///   `pause()` is exactly `ppoll(NULL, 0, NULL, …)`, so `alarm()` + `pause()`
///   was a no-op that returned instantly. Glue's arm carries the fix and its
///   comment.
/// - **A signal interrupts the wait.** The budget loop had no interrupt check at
///   all, so a `poll(-1)` slept through its own `SIGALRM` — nothing returned to
///   the syscall-return path that delivers it.
/// - **The wait stops spinning.** A blocked `poll` was a `yield_now` loop, so it
///   consumed a full share of every scheduling round; it parks now.
///
/// The readiness answers change too, in the direction of more of them being
/// real — see [`poll_console_state`], which is the one thing on this target
/// glue's map cannot know.
pub fn sys_poll(fds: u64, nfds: u64, timeout_ms: u64) -> u64 {
    let timeout = match timeout_ms as i32 {
        ms if ms < 0 => None,
        ms => Some((ms as u64).saturating_mul(1_000)),
    };
    akuma_syscalls_glue::flat(akuma_syscalls_glue::poll::ppoll_timeout_us(
        fds,
        nfds as usize,
        timeout,
    ))
}

/// `ppoll(fds, nfds, *timespec, sigmask, sigsetsize)` — x86_64 271. **Glue's
/// arm**, unconverted: the `struct timespec` read is glue's own
/// (`akuma_syscalls_time::read_timeout_us`), which is the one place in the tree
/// that decodes a poll timeout.
///
/// This used to fold the `timespec` down to *milliseconds* here and call
/// [`sys_poll`], which threw away every sub-millisecond timeout — a `ppoll` of
/// 500 µs became 0 and returned immediately. Nanoseconds survive now.
///
/// The `sigmask` is still ignored, on both kernels: glue takes it as `_sigmask`.
/// A `ppoll` that atomically swaps the signal mask for the duration of the wait
/// is the whole reason the call exists over `poll`, and neither kernel does it.
/// Nothing in this tree passes a non-NULL mask, so it is a pinned divergence
/// rather than a gap being closed here.
pub fn sys_ppoll(fds: u64, nfds: u64, timeout: u64, sigmask: u64) -> u64 {
    akuma_syscalls_glue::flat(akuma_syscalls_glue::poll::sys_ppoll(
        fds,
        nfds as usize,
        timeout,
        sigmask,
    ))
}

/// `select(nfds, readfds, writefds, exceptfds, timeout)` — x86_64 23. **Glue's
/// `pselect6` arm** (4b batch 4b) behind a `struct timeval` conversion.
///
/// x86_64 is the architecture that has *both* spellings, and musl uses this one:
/// `src/select/select.c` compiles its `#ifdef SYS_select` branch here and its
/// `pselect6` branch on aarch64, which is why the two kernels needed two arms
/// for one call. The difference is the timeout struct — `{ tv_sec, tv_usec }`
/// against `{ tv_sec, tv_nsec }` — and the conversion below is the whole of it.
///
/// A NULL pointer blocks forever. `tv_usec` outside `0..1_000_000` is `EINVAL`,
/// which Linux also enforces and which is what keeps a garbage `timeval` from
/// becoming a very long wait.
///
/// # The divergences, carried
///
/// - **The struct is not updated on return.** Linux writes the remaining time
///   back into it. Glue's `pselect6` cannot (its ABI has no such rule) and this
///   arm never did; nothing in this tree reads it.
/// - **`exceptfds` comes back all-zero**, and that is glue's arm doing it, not
///   this one. It is load-bearing: a set the kernel *received* but did not write
///   comes back exactly as the caller passed it, and cargo's vendored libcurl
///   compiles the `select()` branch of `Curl_poll()` and reads `POLLPRI` out of
///   the stale set — `docs/runbooks/cargo-cannot-reach-crates-io.md`. Both
///   implementations had the rule; only one of them has the regression test.
/// - **`nfds == 0` returns 0 immediately** instead of sleeping out the timeout.
///   Glue's divergence, unchanged by the fold, and the opposite of `ppoll`'s
///   (which was fixed) — stated here because the pair now sits in one place.
///
/// # What the fold buys
///
/// A **listening** TCP socket is readable when a connection is waiting. This
/// arm's own readiness probe asked `akuma_net::socket::socket_tcp_ready`, which
/// resolves a single smoltcp handle and answers `(false, false)` for a listener
/// — a pool of `MAX_BACKLOG` handles, not one — so `select` on a listening fd
/// could never report a pending connection, and an event-driven server that
/// waits before calling `accept` waited forever. Glue's probe asks
/// `listener_ready`, which also reaps backlog handles that died unaccepted.
pub fn sys_select(nfds: u64, readfds: u64, writefds: u64, exceptfds: u64, timeout: u64) -> u64 {
    let deadline = if timeout == 0 {
        None
    } else {
        // `struct timeval { i64 tv_sec, i64 tv_usec }` — 16 bytes.
        let Some([sec, usec]) = crate::uaccess::read_val::<[i64; 2]>(timeout) else {
            return errno::EFAULT;
        };
        if sec < 0 || !(0..1_000_000).contains(&usec) {
            return errno::EINVAL;
        }
        Some((sec as u64).saturating_mul(1_000_000).saturating_add(usec as u64))
    };
    akuma_syscalls_glue::flat(akuma_syscalls_glue::poll::pselect6_timeout_us(
        nfds as usize,
        readfds,
        writefds,
        exceptfds,
        deadline,
    ))
}

/// **This target's *unbound* console, for the poll family's readiness map** —
/// `akuma_syscalls_glue::SyscallHooks::poll_console_state`, registered in
/// `boot::install_shared_sinks`.
///
/// # What is left of it
///
/// A descriptor **in** the fd table is glue's now. Its readiness arm resolves a
/// `Stdin`/`DevTty` through `current_channel()`, reports
/// `has_data: ch.has_stdin_data()` and — the part this hook cannot do —
/// registers the caller as a poller on that channel, so `write_stdin`'s
/// `wake_pollers` releases a parked `poll` on the keystroke rather than at the
/// next 10 ms blocking-poll tick. Since `crate::console` gives a console
/// process a real channel, that path is simply better than this one, and
/// [`is_bound`] is the test that hands it over.
///
/// What glue still cannot answer is an **unbound** 0/1/2 — the boot task, which
/// holds no descriptors at all. It is not in the fd table, so the map reports
/// `FdState::Missing`, which is `EPOLLHUP | EPOLLERR`: not "nothing yet" but
/// "this fd is finished", on a console that is merely idle.
///
/// Returning an [`FdState`](akuma_syscalls_poll::readiness::FdState) rather than
/// event bits is deliberate: the console is then mapped by the same host-tested
/// table as every other resource instead of beside it.
///
/// **There is no waker here.** [`console_has_input`] asks the channel (or, before
/// the pump exists, the UART), and nothing on this target rings a bell when a
/// byte lands. The wait loop's park is capped at the 10 ms blocking-poll
/// interval for exactly this case — which is the cost the bound path no longer
/// pays.
pub fn poll_console_state(fd: u32) -> Option<akuma_syscalls_poll::readiness::FdState> {
    use akuma_syscalls_poll::readiness::FdState;
    // **A descriptor in the table is glue's**, and answering for it here would
    // be worse than not answering: glue's own `Stdin` arm calls
    // `ch.add_poller(tid)` on the console channel, so `write_stdin`'s
    // `wake_pollers` releases a parked `poll` on the keystroke itself. This
    // hook has no waker (its header says so) and a poll through it waits out
    // the 10 ms blocking-poll interval instead. So the hook now covers only the
    // spelling glue cannot see — an **unbound** 0/1/2, which is not in the fd
    // table at all and would otherwise map to `FdState::Missing`, i.e.
    // `EPOLLHUP | EPOLLERR` on a console that is merely idle.
    if is_bound(u64::from(fd)) {
        return None;
    }
    match console_end(u64::from(fd))? {
        ConsoleEnd::Read => Some(FdState::Stdin { has_data: console_has_input() }),
        // Always writable: the serial port never blocks.
        ConsoleEnd::Write => Some(FdState::Sink),
    }
}

/// `access(path)` / `faccessat(.., path, ..)` — does the path resolve?
///
/// `F_OK`/`R_OK`/`X_OK` all collapse to "it exists": one user, and no per-file
/// permission enforcement anywhere else on this target, so answering anything
/// finer would be inventing a result. `0` if it resolves, `-ENOENT` if not.
pub fn sys_access(path: u64) -> u64 {
    // **Glue's `faccessat2` arm** (4b batch 3c), from the root. The dispatcher
    // routes the real `faccessat(2)` through `to_glue` so its `dirfd` is
    // honoured; this spelling is `access(path)` and the kernel-side callers
    // (`proc_consistency_check`) that want "does this path resolve".
    akuma_syscalls_glue::flat(akuma_syscalls_glue::fs::sys_faccessat2(
        (-100i64) as i32,
        path,
        0,
        0,
    ))
}

/// `ioctl(fd, request, arg)` — this target's fake tty, then **glue's arm**
/// (4b batch 4b).
///
/// # The divergence the preamble exists for
///
/// This is the one place in the whole 4b fold where the two kernels do not
/// disagree by accident. They implement **two different interactive-shell
/// architectures** and each answers `TCGETS` on a pipe the way its own
/// architecture requires:
///
/// - **Here**, a spawned child's stdin *is* a pipe — `bind_stdio` gives it one
///   and there is no `ProcessChannel` and no PTY — so the pipe has to *be* the
///   terminal, faked at `ioctl`. An interactive `busybox sh` probes stdin with
///   `TCGETS` on startup and, if it fails, decides stdin is not a terminal: no
///   prompt, no line editing, read to EOF. Over an ssh channel that is
///   indistinguishable from a hang.
/// - **Glue** checks the fd *table entry* and answers `ENOTTY` for a
///   `PipeRead`, deliberately — "so shells like busybox run non-interactively
///   over the SSH-into-box bridge instead of launching a line editor that hangs
///   on an `ESC[6n` cursor query" (`TTY_SHENANIGANS.md` round 3). On that
///   kernel the exec bridge hands the child a `ProcessChannel` that reports
///   `is_terminal()` for a real PTY session, *separately* from its stdio pipes,
///   so the table entry is free to be the ground truth.
///
/// Neither answer is wrong for its own kernel and neither can be adopted by the
/// other, so this arm keeps its own answers for the requests that decide the
/// question and delegates the rest. The preamble is `fd < FIRST_FILE_FD ||
/// console_end(fd).is_some() || dev_node_of(fd) == Some("tty")` — the first
/// term is the load-bearing one and is what claims a spawned child's pipe at
/// fd 0; the second adds the descriptor spelling (a registered process's own
/// `Stdin`/`Stdout`/`Stderr`); the third is the fd a pager opens on `/dev/tty`
/// to read keys from, which is never 0/1/2.
///
/// **Two thirds of that has happened and the term above is the third.** The
/// *console* has a terminal-capable `ProcessChannel` (`crate::console`), so
/// `poll`'s `Stdin` arm works on its own for a console process and
/// [`poll_console_state`] has shrunk to the unbound spelling. An **sshd
/// session's child** has one too since 2026-09-11 — `sys_spawn` builds it for
/// `SPAWN_FLAG_PTY` — which is what gives an `ssh` session INTR→SIGINT: the
/// bridge's keystrokes reach `akuma_exec::process::write_to_process_stdin`
/// through the real procfs `/proc/<pid>/fd/0`, its ISIG branch reads that
/// channel's `is_terminal()`, and `^C` raises `SIGINT` on the session's
/// foreground group instead of arriving as a `0x03` byte.
///
/// **What has not happened is fd 0.** The session child's stdin is still a
/// `PipeRead` from [`bind_stdio`] — the channel carries the terminal's identity,
/// not its data — so glue's table test still answers `ENOTTY` for it and the
/// `fd < FIRST_FILE_FD` fake tty is still what lets `busybox sh` run
/// interactively over the bridge.
///
/// What is in the way is **not** a missing mechanism, and the first note here
/// said it was. It is this target's *second implementation of session stdio*.
/// An AArch64 spawned child has one `ProcessChannel` serving its I/O, its exit
/// status and the parent's `ChildStdout(pid)` descriptor, with
/// `SharedFdTable::with_stdio()` for fd 0/1/2 and **no pipes**; the line
/// discipline's echo and the program's own output land in one FIFO and `sshd`
/// reads that FIFO. This target builds the same session on `crate::pipe`
/// because `sys_spawn` predates all of it, so moving fd 0 is a deletion:
/// [`bind_stdio`] and both pipes out, `sys_spawn` returning a `ChildStdout`,
/// and two preambles moving with it — `sys_write`'s, which claims **every**
/// `Stdout`/`Stderr` descriptor for the serial line, and this one.
///
/// (`delegate_pid` used to be named here as part of the same deferred work. It
/// is not: `delegate_pid` is `sys_reattach`'s — `box grab` — which moves a
/// *target's* channel to a *grabber*, and has nothing to do with how `sshd`
/// reaches its own child. The phrase was a leftover.)
///
/// # What glue adds
///
/// Everything this arm never had, and each of them has a caller in the tree:
/// `FIONBIO` (the non-blocking flag, which `fcntl` already tracked and `ioctl`
/// could not set), `FIONREAD` (bytes available — answered per fd kind, so a
/// pipe and a socket give real counts), `FIOCLEX`/`FIONCLEX` (the close-on-exec
/// flag), `FIOASYNC` (a no-op success, without which nginx's
/// `ngx_spawn_process` refuses to fork at all), and the `SIOCGIF*` block.
///
/// # Two carried behaviour changes
///
/// - **`SIOCGIF*` now requires a socket fd.** This arm answered them on *any*
///   descriptor; glue gates them on `FileDescriptor::Socket(_)` and otherwise
///   says `ENOTTY`. That is Linux (they are socket ioctls) and `busybox
///   ifconfig` always holds an `AF_INET` socket, so the tightening costs
///   nothing real — but the boot suite's checks were passing an unopened fd 3
///   and moved to `sock::smoke_test`, which has a real one.
/// - **A request on a non-console fd with no registered process is `ESRCH`,
///   not `ENOTTY`.** Glue's first line is `current_process_shared()`. Only the
///   boot task can be in that state, and `boot_row_register` is the answer the
///   suite already uses for it (4b batch 2b).
pub fn sys_ioctl(fd: u64, req: u64, arg: u64) -> u64 {
    // The by-number/by-descriptor console, plus a `/dev/tty` fd. See the header:
    // the `fd < FIRST_FILE_FD` term is the fake tty and is the reason this
    // preamble exists at all.
    let is_console = console_end(fd).is_some() || dev_node_of(fd) == Some("tty");
    if is_console
        && let Some(r) = console_ioctl(req, arg)
    {
        return r;
    }
    // `TIOCSWINSZ` on a spawned child's stdout needed a shim here while the
    // parent held that stdout as a plain `PipeRead`: glue's arm reads the child
    // pid straight out of a `FileDescriptor::ChildStdout(pid)`, which this
    // target had none of, so the size landed in `sshd`'s own `TerminalState`
    // and nothing ever read it (`AMD64_SSH_TERM_SIZE_NOT_PASSED.md` break 4).
    // `sys_spawn` hands back a real `ChildStdout` now, so glue's arm answers it.
    // Everything else — including any request on a console fd that the terminal
    // subset above does not claim, which glue also answers `ENOTTY`.
    akuma_syscalls_glue::term::sys_ioctl(fd as u32, req as u32, arg)
}

/// The terminal subset this target answers itself, for a console fd.
///
/// `None` means "not one of mine" and sends the caller on to glue's arm.
///
/// **`TCGETS`/`TCSETS` read and write a real `TerminalState` now.** They were
/// compiled-in literals and a no-op respectively, for as long as this target
/// had no line discipline to describe — which is also why the console was
/// always cooked: a program could ask for raw mode and nothing recorded the
/// request. `crate::console::default_terminal_state` seeds every process's
/// state with exactly the values the literals reported, so a program that reads
/// without setting sees what it always saw, and `stty -a` over ssh still says
/// `speed 38400 baud` and `susp = ^Z`.
///
/// They stay here rather than delegating for a reason that has nothing to do
/// with the fold: glue's `term::sys_ioctl` writes `c_cc` at byte **16** instead
/// of 17 (byte 16 is `c_line`), so every control character it reports is one
/// off. See batch 4b's § "found, not fixed".
///
/// The rest are still fixed answers, and the reason is unchanged: a spawned
/// child's stdin is a pipe with no channel behind it, so what these report is
/// what a cooked terminal *would* look like — enough that `isatty` says yes and
/// a shell does not give up.
fn console_ioctl(req: u64, arg: u64) -> Option<u64> {
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
    /// `c_cc`'s length in the kernel `struct termios`. `TerminalState::cc` is
    /// 20 bytes and Linux's array is 19; the copy is bounded by this so neither
    /// side reads past its own.
    const NCCS: usize = 19;

    Some(match req {
        TCGETS => {
            if arg == 0 {
                return Some(errno::EFAULT);
            }
            // **Read out of the line discipline, not out of literals.**
            //
            // These four words were compiled-in constants for as long as this
            // target had no `TerminalState` to describe, which is also why
            // `TCSETS` below could only be a no-op: there was nowhere to put
            // what a caller set. `crate::console::default_terminal_state`
            // starts every process's state at exactly the values that used to
            // be spelled here, so what a program reads is unchanged until it
            // sets something — and now a program that sets raw mode gets raw
            // mode, because glue's `Stdin` arm branches on `is_canonical()`
            // reading this same cell.
            //
            // Kernel `struct termios`: c_iflag/oflag/cflag/lflag (u32 each),
            // c_line (u8), c_cc[19]. 36 bytes; a couple extra do no harm.
            let mut t = [0u8; 44];
            let put = |t: &mut [u8], off: usize, v: u32| {
                t[off..off + 4].copy_from_slice(&v.to_le_bytes());
            };
            // A thread with no registered state at all — the boot task before
            // `boot_row_register` — reads the same defaults it always did
            // rather than zeroes, and without building a `TerminalState` to do
            // it.
            let (iflag, oflag, cflag, lflag, cc) =
                if let Some(lock) = akuma_exec::process::current_terminal_state() {
                    let ts = lock.lock();
                    (ts.iflag, ts.oflag, ts.cflag, ts.lflag, ts.cc)
                } else {
                    let (iflag, oflag, cflag, lflag) = crate::console::DEFAULT_FLAGS;
                    (iflag, oflag, cflag, lflag, crate::console::default_cc())
                };
            put(&mut t, 0, iflag);
            put(&mut t, 4, oflag);
            put(&mut t, 8, cflag);
            put(&mut t, 12, lflag);
            // `c_cc[0]` is at byte **17** — byte 16 is `c_line`, which is not
            // part of the array. Glue's `term::sys_ioctl` writes its `cc` at 16
            // and is one byte off for every control character; see batch 4b's
            // § "found, not fixed". That bug is the reason this request is
            // still answered here rather than delegated.
            t[17..17 + NCCS].copy_from_slice(&cc[..NCCS]);
            if errno::is_err(copy_to_user(arg, &t)) {
                return Some(errno::EFAULT);
            }
            0
        }
        // **The setters that used to be no-ops.**
        //
        // `TCSETS`/`TCSETSW`/`TCSETSF` differ only in when they take effect
        // relative to queued output (`stty` uses all three); this target has no
        // output queue to drain, so all three are immediate. Landing them in the
        // process's `TerminalState` is the whole of raw mode: `stty raw` and
        // every `cfmakeraw` caller clear `ICANON|ECHO` here, and glue's `Stdin`
        // arm reads that cell on its next pass.
        //
        // A caller with no registered terminal state is accepted and dropped,
        // which is what all three did for everyone before this.
        TCSETS | TCSETSW | TCSETSF => {
            if arg == 0 {
                return Some(errno::EFAULT);
            }
            let Some(bytes) = copy_in(arg, 36) else {
                return Some(errno::EFAULT);
            };
            if let Some(lock) = akuma_exec::process::current_terminal_state() {
                let get = |off: usize| {
                    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
                };
                let mut ts = lock.lock();
                ts.iflag = get(0);
                ts.oflag = get(4);
                ts.cflag = get(8);
                ts.lflag = get(12);
                // `c_cc` from byte 17, the same offset `TCGETS` writes it at.
                ts.cc[..NCCS].copy_from_slice(&bytes[17..17 + NCCS]);
            }
            0
        }
        TIOCGWINSZ => {
            if arg == 0 {
                return Some(errno::EFAULT);
            }
            // **The one request here that is not a constant.** The window size
            // is the only part of a terminal this target really tracks: an
            // `sshd` session's client knows its own size, sends it in
            // `pty-req`, and `sshd` writes it into this process's
            // `TerminalState` through `TIOCSWINSZ` (see [`sys_ioctl`]'s arm for
            // how it gets here from the *parent's* descriptor). Answering the
            // compiled-in 24x80 instead is what made every full-screen program
            // over ssh believe the window was 80 columns whatever it really was
            // — `AMD64_SSH_TERM_SIZE_NOT_PASSED.md` break 5.
            //
            // The fallback stays 24x80, for a process with no registered
            // terminal state at all (the boot task); `TerminalState::default()`
            // is the same size, so a process that was never told anything reads
            // exactly what it read before.
            let (rows, cols) = akuma_exec::process::current_terminal_state()
                .map_or((24u16, 80u16), |ts| {
                    let ts = ts.lock();
                    (ts.term_height, ts.term_width)
                });
            // struct winsize { u16 ws_row, ws_col, ws_xpixel, ws_ypixel }.
            let mut w = [0u8; 8];
            w[0..2].copy_from_slice(&rows.to_le_bytes()); // ws_row
            w[2..4].copy_from_slice(&cols.to_le_bytes()); // ws_col
            if errno::is_err(copy_to_user(arg, &w)) {
                return Some(errno::EFAULT);
            }
            0
        }
        // The job-control queries: accept, and answer with the one process
        // group this target has.
        //
        // `TIOCSWINSZ` is accepted-and-dropped on purpose: a process setting
        // the size of its *own* console is describing a terminal this target
        // does not own, and the size that matters comes from `sshd` on the
        // parent's descriptor, which never reaches this arm ([`sys_ioctl`]).
        TIOCSWINSZ | TIOCSPGRP | TIOCSCTTY => 0,
        TIOCGPGRP => {
            if arg != 0 && errno::is_err(copy_to_user(arg, &1i32.to_le_bytes())) {
                return Some(errno::EFAULT);
            }
            0
        }
        _ => return None,
    })
}

/// `(start, end, readable, write, exec, private)` — one mapping.
type MapRow = (usize, usize, bool, bool, bool, bool);

/// **The `/proc/<pid>/maps` and `/proc/<pid>/statm` walk, for any pid** — this
/// target's answer to `akuma_vfs_glue::VfsGlueHooks::pid_map_rows`.
///
/// Registered in `fs::init_vfs`, so the *shared* `ProcFilesystem` renders both
/// files (through the same `akuma-procfs` formats this module used to call
/// directly) and this kernel stops serving them from a view of its own. The
/// hook exists because the leaf walk below is x86-only: `for_each_user_leaf`
/// is `x86_walk_leaves` underneath, and the AArch64 kernel registers a
/// function returning `None`.
///
/// `None` means "no such process", which procfs turns into an absent file.
pub fn pid_map_rows(pid: u32) -> Option<Vec<MapRow>> {
    let regions = akuma_exec::process::with_process(pid, |p| {
        p.mmap_regions
            .lock()
            .iter()
            .map(|r| {
                let prot = r.recorded_prot().unwrap_or(akuma_mmap::Prot::RW_NO_EXEC);
                (
                    r.start_va,
                    r.start_va.saturating_add(r.len_bytes()),
                    !prot.is_none(),
                    prot.is_write(),
                    prot.is_exec(),
                    !r.shared_anon,
                )
            })
            .collect::<Vec<MapRow>>()
    })?;
    let mut rows = regions;
    let extents: Vec<(usize, usize)> = rows.iter().map(|r| (r.0, r.1)).collect();
    let leaves = akuma_exec::process::with_process(pid, |p| {
        collect_leaf_runs(&p.address_space.lock(), &extents)
    })?;
    rows.extend(leaves);
    rows.sort_unstable_by_key(|r| r.0);
    Some(rows)
}

/// Present user leaves outside every extent in `skip`, coalesced into runs.
///
/// `for_each_user_leaf` visits in ascending VA order (it walks each level's
/// indices upward), which is what makes a single-pass coalesce correct rather
/// than a sort-then-merge.
///
/// A run is not a VMA: two adjacent loader mappings with identical permissions
/// merge into one line. That is what the hardware says, which is the only
/// record this target keeps for pages the loader placed.
fn collect_leaf_runs(uas: &akuma_mmu::UserAddressSpace, skip: &[(usize, usize)]) -> Vec<MapRow> {
    let mut rows: Vec<MapRow> = Vec::new();
    let mut run: Option<MapRow> = None;
    uas.for_each_user_leaf(|leaf| {
        let (va, prot) = (leaf.va, leaf.prot);
        if !prot.user || skip.iter().any(|&(s, e)| va >= s && va < e) {
            // Flush across a gap a region already covers, so a mapping either
            // side of it is not merged through it.
            if let Some(r) = run.take() {
                rows.push(r);
            }
            return;
        }
        // Every present leaf is readable — x86 has no read-disable bit, so `r`
        // is not a fact the PTE can carry differently.
        let (w, x) = (prot.write, prot.exec);
        match run {
            Some(ref mut r) if r.1 == va && r.3 == w && r.4 == x => r.1 = va + 4096,
            _ => {
                if let Some(r) = run.take() {
                    rows.push(r);
                }
                run = Some((va, va + 4096, true, w, x, true));
            }
        }
    });
    if let Some(r) = run.take() {
        rows.push(r);
    }
    rows
}

#[cfg(not(feature = "no-tests"))]
/// `open`, `stat` and `access` must agree about every `/proc` path.
///
/// # Why this is a self-test and not a comment
///
/// The three answers came from **two** functions when this kernel rendered
/// `/proc` itself: `sys_openat` and `sys_newfstatat` shared a renderer "so the
/// two can never disagree about what exists", and `sys_access` went straight to
/// the disk, so it said `ENOENT` for every `/proc` path the target served.
/// Nothing failed loudly: `busybox` mostly `open`s, and the one caller that
/// probes with `access` first is a program deciding whether the kernel has a
/// `/proc` at all.
///
/// Since 4b batch 2c there is one implementation — the mounted
/// `ProcFilesystem` — and this check is what says so: it is the same section it
/// always was, asserting the same paths, against the survivor.
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

// `interfaces()` and `siocgif()` — the `struct ifreq`/`struct ifconf`
// marshalling behind `busybox ifconfig` — left with the `ioctl` fold (4b batch
// 4b). They were a **byte-for-byte** duplicate of `akuma_syscalls_glue::net`'s
// `net_ifaces` + `sys_ioctl_siocgifconf`/`sys_ioctl_siocgifreq`, right down to
// building the two-interface array fresh on every call so a DHCP change shows
// up; both sides already marshalled through the same `akuma-syscalls-net`, so
// there was one layout and two copies of the user-copy loop around it. What the
// delegation adds is the socket-fd gate — see `sys_ioctl`'s header.


#[cfg(not(feature = "no-tests"))]
/// Give the boot row a process identity for the duration of a self-test, and
/// return the thread id [`boot_row_release`] needs.
///
/// **Why any test that touches a descriptor needs this.** Every
/// `akuma-syscalls-glue` arm that hands out or frees one ends in
/// `if let Some(proc) = current_process_shared() { … } else { Err(ESRCH) }`,
/// and this suite runs on the boot task, which is registered nowhere. Batch
/// 1's five folded arms slipped past it only because they are path-based;
/// `close` — folded in batch 2b — does not, and `sock::smoke_test` closing a
/// socket was the check that said so (`ESRCH`, not 0).
///
/// **pid 1, not a spare number.** `usermode::current_pid` already answers 1 for
/// an unmapped thread — "the self-tests run before `run_init` registers
/// anything, and they are pid 1's work" — and `/proc/self` resolves through it,
/// so any other pid would point `/proc/self` at a process the synthetic view
/// has never heard of.
///
/// **The row starts with stdio, which `make_test_process` does not give it.**
/// A prerequisite for the `openat` fold (4b batch 2d), and the same one every
/// registered process met in batch 2a: glue allocates with `alloc_fd`, which is
/// `alloc_fd_from(0)`, so with 0/1/2 absent the suite's first `open` returns
/// **fd 0** — and every check that reads `fd >= FIRST_FILE_FD` as "the open
/// succeeded" reads a successful open as a failure. `make_test_process` builds
/// `SharedFdTable::new()` and is `akuma-exec`'s, shared with 25 AArch64 call
/// sites, so the triple is written here rather than there.
///
/// The AArch64 kernel has had this since long before: `register_at_syscall_process`,
/// used 25 times in `src/process_tests.rs`.
pub fn boot_row_register() -> usize {
    let tid = akuma_exec::threading::current_thread_id();
    akuma_exec::process::register_process(1, akuma_exec::process::make_test_process(1));
    akuma_exec::process::register_thread_pid(tid, 1);
    if let Some(proc) = akuma_exec::process::current_process_shared() {
        proc.set_fd(0, FileDescriptor::Stdin);
        proc.set_fd(1, FileDescriptor::Stdout);
        proc.set_fd(2, FileDescriptor::Stderr);
    }
    // **And the console itself.** The boot task sits on the serial line exactly
    // as `init` does, so a `Stdin` in its table has to name the same channel a
    // ring-3 console process's does — otherwise glue's `Stdin` arm takes its
    // `current_channel().is_none()` fallback and answers `0`, i.e. EOF, which is
    // what `console_nonblock_test` saw the moment `read_console` was deleted.
    //
    // Through the **per-thread** registry rather than `Process::channel`:
    // `make_test_process` builds the row and its fields are not reachable
    // afterwards, and `current_channel` consults `get_channel(tid)` precisely
    // for a thread whose identity was assembled this way.
    // [`boot_row_release`] takes it back.
    //
    // **The line discipline is deliberately NOT registered alongside it.**
    // `register_terminal_state(tid, ..)` would shadow `Process::terminal_state`
    // for this thread, and the boot row's own state is what `winsize_test`
    // writes through `with_process(caller_pid, ..)` and reads back through
    // `TIOCGWINSZ` — the Break 5 check. The one thing the row therefore does
    // not get is the pump's `input_waker`: a *blocking* console read from the
    // boot task would park on a cell `console::pump_once` never fires and wait
    // out the 1 Hz untimed-park backstop instead. No check does that (the suite
    // runs against an idle line and reads non-blocking), and the pump is not
    // even spawned until `run_init`; this note is for the one that tries.
    if let Some(ch) = crate::console::channel() {
        akuma_exec::process::channel::register_channel(tid, ch);
    }
    tid
}

#[cfg(not(feature = "no-tests"))]
/// Hand pid 1 back, and return how many retired slots the drain reclaimed.
///
/// `unregister_process` **retires** the slot rather than dropping it — the
/// deferred reclamation Phase 7e introduced — so the `Process`, and the address
/// space `make_test_process` built for it, are still held when it returns. Left
/// that way the identity costs a permanent page and `identity: probe teardown
/// leaks nothing` reports it, correctly. Draining is what makes the
/// registration a loan rather than a leak.
pub fn boot_row_release(tid: usize) -> usize {
    // The console channel [`boot_row_register`] took out. Left behind, the next
    // boot row to land on this slot inherits it — and so would any unrelated
    // thread the slot is recycled to.
    akuma_exec::process::channel::remove_channel(tid);
    akuma_exec::process::unregister_thread_pid(tid);
    akuma_exec::process::unregister_process(1);
    akuma_exec::process::reclaim::drain_retired()
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

    // The boot row's identity, for every check below that opens or closes
    // something — see [`boot_row_register`].
    let boot_tid = boot_row_register();
    t.check(
        "fd: the boot row has a process identity (every glue fd arm needs one)",
        akuma_exec::process::current_process_shared().is_some(),
    );

    // A kernel-side buffer standing in for a user pointer. The copy helpers do
    // not care which side of the privilege boundary an address is on — they
    // dereference it — so this is a faithful exercise of the same path.
    let mut buf = [0u8; 64];
    let path = b"/probe.txt\0";

    // **The console, by descriptor** — the spelling a registered process uses
    // (`SharedFdTable::with_stdio`) and the one every folded
    // `akuma-syscalls-glue` arm will produce. The suite's own row has 0/1/2
    // *unbound*, so it exercises the by-number rule on every other line;
    // these six lines are the only place the variant arms are asked anything.
    //
    // Negative control, run 2026-09-09 rather than reasoned about: with
    // `console_end`'s two variant arms returning `None`, `INIT=/bin/hello`
    // printed `-- running /bin/hello --` and then nothing at all — its
    // `write(1)` reached `sys_write_file`, which has no arm for a `Stdout`
    // descriptor and answers `EBADF`. That is the failure this block pins.
    // **Two descriptors appending to one file must interleave, not clobber.**
    // Until 2026-09-09 `O_APPEND` was only a *starting* cursor set at `open`,
    // so both descriptors began at the same offset and the second write landed
    // on top of the first — `sys_openat`'s own comment pinned that as a
    // divergence. The write path re-derives the position per call now, which
    // is `akuma-syscalls-glue`'s semantics and the reason the `openat` fold
    // can seed nothing.
    {
        const O_WRONLY: u64 = 1;
        const O_CREAT: u64 = 0o100;
        const O_TRUNC: u64 = 0o1000;
        const O_APPEND: u64 = 0o2000;
        let ap = b"/append-probe.txt\0";
        let seed = sys_openat(0, ap.as_ptr() as u64, O_WRONLY | O_CREAT | O_TRUNC, 0o644);
        if t.check("fd: append probe opens for create", !errno::is_err(seed)) {
            sys_write_file(seed, b"AAA".as_ptr() as u64, 3);
            sys_close(seed);
            let first = sys_openat(0, ap.as_ptr() as u64, O_WRONLY | O_APPEND, 0);
            let second = sys_openat(0, ap.as_ptr() as u64, O_WRONLY | O_APPEND, 0);
            sys_write_file(first, b"B".as_ptr() as u64, 1);
            // `b` was opened before `a` wrote, so its seeded cursor is stale;
            // only a per-write re-derivation puts this byte after the `B`.
            sys_write_file(second, b"C".as_ptr() as u64, 1);
            sys_close(first);
            sys_close(second);
            let back = sys_openat(0, ap.as_ptr() as u64, 0, 0);
            let n = sys_read(back, buf.as_mut_ptr() as u64, 8);
            sys_close(back);
            t.check_eq("fd: both appends landed", n, 5);
            t.check("fd: the second appender did not clobber the first", &buf[..5] == b"AAABC");
        }
    }

    let out_fd = install(FileDescriptor::Stdout);
    let in_fd = install(FileDescriptor::Stdin);
    t.check("fd: a Stdout descriptor is the console's write end",
        console_end(out_fd) == Some(ConsoleEnd::Write));
    t.check("fd: a Stdin descriptor is the console's read end",
        console_end(in_fd) == Some(ConsoleEnd::Read));
    // `S_IFCHR`, not the regular-file shape the fall-through arm would give:
    // `isatty(3)` is `fstat` plus `S_ISCHR`, so this is the answer that makes
    // a shell on these descriptors interactive.
    let mut st = [0u8; 160];
    t.check_eq("fd: fstat on a console descriptor succeeds",
        sys_fstat(out_fd, st.as_mut_ptr() as u64), 0);
    t.check("fd: and reports a character device",
        u32::from_le_bytes([st[24], st[25], st[26], st[27]]) & 0xF000 == 0x2000);
    // Not seekable — and **`ESPIPE`, not `EBADF`, since 4b batch 3a**. That is
    // the change the fold made and it is Linux's answer: `ESPIPE` says "this
    // *kind* of thing has no offset", where `EBADF` says "you are holding a
    // closed descriptor" and sends a caller looking at its own bookkeeping.
    // musl's `FILE` layer reads the two differently (see [`sys_pread64`]'s
    // header), and this arm could not tell them apart at all — its console
    // guard fired before it ever looked in the table.
    t.check_eq("fd: lseek on a console descriptor is ESPIPE",
        sys_lseek(out_fd, 0, 0), errno::ESPIPE);
    // The other half of that distinction, and the reason the check above is
    // not simply relaxed: a descriptor that names **nothing** is still
    // `EBADF`. `MAX_FDS - 1` is a number this suite never installs.
    t.check_eq("fd: lseek on an unbound descriptor is EBADF",
        sys_lseek(MAX_FDS as u64 - 1, 0, 0), errno::EBADF);
    // Reading the write end is `EBADF`; the read end blocks on the UART, so it
    // is deliberately not called here.
    t.check_eq("fd: read of the console's write end is EBADF",
        sys_read(out_fd, buf.as_mut_ptr() as u64, 1), errno::EBADF);
    t.check_eq("fd: closing the console descriptors", sys_close(out_fd) | sys_close(in_fd), 0);


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
    // **The x86_64 layout hop** (4b batch 3b). Glue fills the asm-generic
    // `Stat` — `st_mode` at offset 16 — and `akuma_syscalls_abi::stat::to_x86_64`
    // moves it to 24. Reading a regular-file type bit out of byte 24 is the
    // whole conversion in one check: with the converter dropped and glue's
    // struct written raw, byte 24 lands in the middle of `st_nlink` and this
    // reads 0.
    t.check_eq(
        "fd: fstat mode type bit is S_IFREG at the x86_64 offset",
        u64::from(u32::from_le_bytes(st[24..28].try_into().unwrap_or([0; 4])) & 0o170_000),
        0o100_000,
    );

    // `statx` — new on this target with batch 3b (was `ENOSYS`). `struct statx`
    // is arch-neutral, so this is glue's arm with no hop; `stx_size` at offset
    // 40, `stx_mode` (a `u16`) at 28.
    {
        let mut sx = [0u8; 256];
        let r = sys_statx(0, path.as_ptr() as u64, 0, 0, sx.as_mut_ptr() as u64);
        t.check_eq("fd: statx /probe.txt succeeds", r, 0);
        t.check_eq(
            "fd: statx reports the size",
            u64::from_le_bytes(sx[40..48].try_into().unwrap_or([0; 8])),
            6623,
        );
        t.check_eq(
            "fd: statx reports a regular file",
            u64::from(u16::from_le_bytes([sx[28], sx[29]]) & 0o170_000),
            0o100_000,
        );
    }

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

    // `ifconfig`'s read-only `SIOCGIF*` ioctls moved to `sock::smoke_test` with
    // the `ioctl` fold (4b batch 4b). They were issued here on an **unopened**
    // fd 3, which only worked because this module answered them regardless of
    // the descriptor; glue gates them on a `FileDescriptor::Socket(_)`, as Linux
    // does, and `sock::smoke_test` is the one that has a real socket fd.
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

    // ── the poll family, now `akuma-syscalls-glue`'s (4b batch 4b) ──────
    //
    // A regular file is always ready; a zero-length set with a zero timeout
    // returns 0; an oversized set is EINVAL.
    //
    // **`nfds` and the boot stack.** The oversized case asks for
    // `fdset::MAX_FDS + 1` fds and not, as it used to, 999. 999 was chosen
    // against this module's own 64-fd cap; the cap is now
    // `akuma_syscalls_poll::fdset::nfds_ok` — one number for the whole poll
    // family, where there used to be 64 here, 1024 in `pselect6` and *nothing*
    // in `ppoll` — so 999 became a legal `nfds`. The suite runs inside
    // `BypassValidationGuard`, so the range check that would refuse a ring-3
    // caller 7 992 bytes of unmapped `struct pollfd` passes here, and `poll`
    // duly wrote 7 992 bytes of `revents` back over an 8-byte **stack** array,
    // taking the return addresses with it: the boot died in a `#GP` at a
    // non-canonical `rip`. The number has to be above the cap for the call to
    // be refused before any copy happens.
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
        sys_poll(pfd.as_mut_ptr() as u64, akuma_syscalls_poll::fdset::MAX_FDS as u64 + 1, 0),
        errno::EINVAL,
    );

    // **The console.** The boot row registered above holds 0/1/2, so this is
    // the *bound* spelling and glue's own readiness arm answers it, through
    // the console `ProcessChannel` [`boot_row_register`] gave the row. The
    // unbound spelling — no fd table at all, which would otherwise map to
    // `FdState::Missing`, i.e. `POLLHUP|POLLERR` on an idle console — is what
    // [`poll_console_state`] is still registered for.
    //
    // With no key pressed, the read end is **not** ready and the screen end
    // **is** writable. A zero timeout is what keeps this from blocking: an
    // infinite one on an idle console is exactly the hang being tested for.
    let mut cfd = [0u8; 8];
    cfd[4..6].copy_from_slice(&0x001u16.to_le_bytes()); // fd 0, POLLIN
    t.check_eq("fd: poll(stdin, POLLIN, 0) on an idle console returns 0", sys_poll(cfd.as_mut_ptr() as u64, 1, 0), 0);
    t.check_eq(
        "fd: and reports no POLLHUP/POLLERR (the hook, not FdState::Missing)",
        u64::from(u16::from_le_bytes(cfd[6..8].try_into().unwrap_or([0; 2]))),
        0,
    );
    cfd = [0u8; 8];
    cfd[0..4].copy_from_slice(&1i32.to_le_bytes());
    cfd[4..6].copy_from_slice(&0x004u16.to_le_bytes()); // fd 1, POLLOUT
    t.check_eq("fd: poll(stdout, POLLOUT) is ready", sys_poll(cfd.as_mut_ptr() as u64, 1, 0), 1);

    // **A pipe pair**, which is the readiness glue actually has a waker for and
    // the shape every `cmd | cmd` and every ssh session's stdio is. Empty: the
    // read end is not ready, the write end is. After a byte: both.
    {
        let mut fds = [0u32; 2];
        let pr = sys_pipe2(fds.as_mut_ptr() as u64, 0);
        if t.check_eq("fd: pipe2 for the poll checks", pr, 0) {
            let (rfd, wfd) = (u64::from(fds[0]), u64::from(fds[1]));
            let mut p2 = [0u8; 16];
            p2[0..4].copy_from_slice(&(rfd as i32).to_le_bytes());
            p2[4..6].copy_from_slice(&0x001u16.to_le_bytes()); // POLLIN
            p2[8..12].copy_from_slice(&(wfd as i32).to_le_bytes());
            p2[12..14].copy_from_slice(&0x004u16.to_le_bytes()); // POLLOUT
            t.check_eq(
                "fd: poll on an empty pipe reports only the write end",
                sys_poll(p2.as_mut_ptr() as u64, 2, 0),
                1,
            );
            t.check_eq(
                "fd: and it is the write end that is ready",
                u64::from(u16::from_le_bytes(p2[14..16].try_into().unwrap_or([0; 2]))) & 0x004,
                0x004,
            );
            let byte = *b"x";
            t.check_eq("fd: write one byte into the pipe", sys_write_file(wfd, byte.as_ptr() as u64, 1), 1);
            t.check_eq(
                "fd: poll now reports both ends",
                sys_poll(p2.as_mut_ptr() as u64, 2, 0),
                2,
            );

            // **`select`**, on the same pair, through glue's `pselect6`. The
            // return value counts *bits left set*, so a pair ready in one
            // direction each is 2. `exceptfds` must come back all-zero — the
            // libcurl `CURL_CSELECT_ERR` rule in
            // `docs/runbooks/cargo-cannot-reach-crates-io.md`, and the reason
            // this arm receives a set it can never populate.
            let mut rset = [0u64; 1];
            let mut wset = [0u64; 1];
            let mut eset = [0u64; 1];
            rset[0] = 1u64 << rfd;
            wset[0] = 1u64 << wfd;
            eset[0] = (1u64 << rfd) | (1u64 << wfd);
            let tv = [0i64; 2]; // a non-NULL, zero timeval: one pass
            t.check_eq(
                "fd: select reports both ends of a pipe with a byte in it",
                sys_select(
                    (wfd + 1).max(rfd + 1),
                    rset.as_mut_ptr() as u64,
                    wset.as_mut_ptr() as u64,
                    eset.as_mut_ptr() as u64,
                    tv.as_ptr() as u64,
                ),
                2,
            );
            t.check_eq("fd: select cleared exceptfds", eset[0], 0);

            t.check_eq("fd: close the poll pipe read end", sys_close(rfd), 0);
            t.check_eq("fd: close the poll pipe write end", sys_close(wfd), 0);
        }
    }

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

    // ── what the `getdents64` fold gained (4b batch 3a) ──────────────────
    //
    // Both of these are red against the arm this file used to carry, which is
    // the only reason they are here — the ENOTDIR check above passes either
    // way.
    //
    // **A `/dev` node lists as a device.** `DirEntry` carries only
    // `is_dir`/`is_symlink`, so every character node under `/dev` came back
    // `DT_REG` (8). Glue asks `dev_node_named` for the real type, which for
    // `null` is `DT_CHR` (2). `ls -l /dev` reads `d_type` to decide whether to
    // `stat` at all.
    let dev = b"/dev\0";
    let devfd = sys_openat(0, dev.as_ptr() as u64, 0, 0);
    if t.check("fd: /dev opens as a directory", devfd >= FIRST_FILE_FD as u64) {
        let mut d = alloc::vec![0u8; 4096];
        let n = sys_getdents64(devfd, d.as_mut_ptr() as u64, 4096);
        let mut null_type = 0u8;
        let mut off = 0usize;
        while !errno::is_err(n) && off + 19 < n as usize {
            let reclen = u16::from_le_bytes([d[off + 16], d[off + 17]]) as usize;
            if reclen == 0 || off + reclen > n as usize {
                break;
            }
            let name_end = d[off + 19..off + reclen]
                .iter()
                .position(|b| *b == 0)
                .map_or(off + reclen, |i| off + 19 + i);
            if &d[off + 19..name_end] == b"null" {
                null_type = d[off + 18];
            }
            off += reclen;
        }
        t.check_eq("fd: getdents64 reports /dev/null as DT_CHR", u64::from(null_type), 2);
        t.check_eq("fd: closing /dev", sys_close(devfd), 0);
    }

    // **`rewinddir` re-reads the directory.** `lseek(dirfd, 0, SEEK_SET)`
    // clears `KernelFile::dir_cache` in glue; this file's arm reset the entry
    // index and left the first snapshot in place for the life of the
    // descriptor, so a walk after a rewind replayed a listing that could no
    // longer be true. Proven by *changing* the directory in between, which is
    // the only way to tell a cleared cache from a rewound index.
    let probe = b"/rewind-probe.txt\0";
    let rdir = b"/\0";
    let walk = |fd: u64| -> bool {
        let mut d = alloc::vec![0u8; 8192];
        let mut seen = false;
        loop {
            let n = sys_getdents64(fd, d.as_mut_ptr() as u64, 8192);
            if errno::is_err(n) || n == 0 {
                return seen;
            }
            let mut off = 0usize;
            while off + 19 < n as usize {
                let reclen = u16::from_le_bytes([d[off + 16], d[off + 17]]) as usize;
                if reclen == 0 || off + reclen > n as usize {
                    break;
                }
                let name_end = d[off + 19..off + reclen]
                    .iter()
                    .position(|b| *b == 0)
                    .map_or(off + reclen, |i| off + 19 + i);
                if &d[off + 19..name_end] == b"rewind-probe.txt" {
                    seen = true;
                }
                off += reclen;
            }
        }
    };
    let root = sys_openat(0, rdir.as_ptr() as u64, 0, 0);
    if t.check("fd: / opens as a directory", root >= FIRST_FILE_FD as u64) {
        t.check("fd: the rewind probe is not there yet", !walk(root));
        const O_WRONLY_C: u64 = 0o1 | 0o100;
        let made = sys_openat(0, probe.as_ptr() as u64, O_WRONLY_C, 0o644);
        if t.check("fd: creating a file in the walked directory", !errno::is_err(made)) {
            sys_close(made);
        }
        t.check_eq("fd: rewinddir seeks to 0", sys_lseek(root, 0, 0), 0);
        t.check("fd: and the second walk sees the new entry", walk(root));
        t.check_eq("fd: closing /", sys_close(root), 0);
        fs::remove_file("/rewind-probe.txt").ok();
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

    // Hand pid 1 back before `run_init` claims it for the real init process.
    let drained = boot_row_release(boot_tid);
    t.check("fd: the boot row's identity was reclaimed, not parked", drained >= 1);
}

#[cfg(not(feature = "no-tests"))]
/// **`O_NONBLOCK` on the console**, which has to run after the console exists.
///
/// Separate from [`smoke_test`] for one mechanical reason: `boot::self_tests`
/// calls `wire_console_and_syscalls()` — and therefore `console::init()` —
/// *after* `fd::smoke_test`, so there is no console `ProcessChannel` there and
/// a read of fd 0 answers 0 (no console, EOF) before it can reach any flag
/// test. Written as a check inside `smoke_test` first, this reported
/// `got 0x0 want -EAGAIN` and the missing console was the whole of it.
///
/// That failure came back, identically, the moment `read_console` was deleted
/// and glue's `Stdin` arm took over — for the same reason one layer down: the
/// boot row's `Stdin` resolved to a process with no channel. See
/// [`boot_row_register`], which now hands the row the console's.
pub fn console_nonblock_test(t: &mut Suite) {
    // Two claims, and the first is the one that was mis-diagnosed as two
    // disjoint flag stores. `fcntl` is glue's arm (4b batch 3c) and writes
    // `Process::set_nonblock`; [`is_nonblocking`] reads the same `fds.nonblock`
    // set through [`cur_table`]. One store, and this proves the two readers see
    // it. The real defect was that `read_console` — which served fd 0 here and
    // only here, because a registered process's fd 0 is a
    // `FileDescriptor::Stdin` and [`sys_read`]'s preamble claimed it before glue
    // saw it — had no flag test at all. There is no preamble and no
    // `read_console` any more: glue's `Stdin` arm serves this read and has
    // honoured `O_NONBLOCK` since before the fold, which is the point.
    //
    // The `EAGAIN` check assumes an idle console, which is true for all three
    // rigs during the suite (QEMU's serial is fed from `/dev/null`, and the
    // bare-metal box has no working keyboard). A keystroke landing here would
    // read as one failing check, not as a hang.
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const O_NONBLOCK: u64 = 0o4000;

    // `fcntl` is glue's arm and refuses an fd that is not in the table, so the
    // boot task needs the descriptor identity every folded arm needs — see
    // [`boot_row_register`].
    let boot_tid = boot_row_register();
    let mut buf = [0u8; 4];

    t.check_eq("fd: fcntl(stdin, F_SETFL, O_NONBLOCK)", sys_fcntl(0, F_SETFL, O_NONBLOCK), 0);
    t.check_eq(
        "fd: fcntl(stdin, F_GETFL) reports it back",
        sys_fcntl(0, F_GETFL, 0) & O_NONBLOCK,
        O_NONBLOCK,
    );
    t.check("fd: and is_nonblocking agrees — one flag store", is_nonblocking(0));
    t.check_eq(
        "fd: a non-blocking read of an idle console is EAGAIN, not a park",
        sys_read(0, buf.as_mut_ptr() as u64, 4),
        errno::EAGAIN,
    );
    // Cleared before returning: `run_init` inherits this descriptor table's
    // flags, and an init whose stdin is non-blocking reads its console as a
    // stream of `EAGAIN`.
    t.check_eq("fd: fcntl(stdin, F_SETFL, 0) clears it", sys_fcntl(0, F_SETFL, 0), 0);
    t.check("fd: is_nonblocking cleared", !is_nonblocking(0));
    // And a *blocking* read of an idle console still does not return data.
    // Not that it blocks — the suite cannot wait for that — but that clearing
    // the flag put the arm back on the path it was on.
    t.check(
        "fd: the flag is what changed, not the arm",
        !errno::is_err(sys_fcntl(0, F_GETFL, 0)),
    );

    boot_row_release(boot_tid);
}
