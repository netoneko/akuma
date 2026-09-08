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
//! # Contents are cached, and that is the local part
//!
//! `open` reads the whole file through `fs::read_file` and holds the bytes
//! alongside the descriptor; `read` and `lseek` work on that buffer. The
//! AArch64 kernel reads by inode on every call instead, backed by `akuma-ext2`'s
//! own block cache. Doing that here needs the `VfsHooks` plumbing that lives in
//! `akuma-exec`, which does not build for this target — so this is a stated
//! divergence, not an oversight, and the cost is that a file occupies its own
//! size in kernel heap while open.
//!
//! # Descriptors are per-process; descriptions are not
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
//! It is now the POSIX split, which fixes all three at once:
//!
//! - [`FDS`] is one **row per process** — `FDS[row][fd]` — so every process
//!   has its own fd 3 and its own budget of [`MAX_FDS`].
//! - [`FILES`] is the machine-wide table of open file **descriptions**: the
//!   cursor, the cached contents, the socket or pipe identity. Reference
//!   counted, because `dup` and `fork` add a *name* without adding a
//!   description.
//!
//! `fork` copies a row and increments; `exit` drops a row and decrements; only
//! the last name going away releases the description (and persists a written
//! file). `close_owned_by` survives as the exit hook, but it is now a row
//! release rather than a search for an owner.
//!
//! **Descriptors 0/1/2 are still answered by number, above this layer.** They
//! are not in a row, which is why `dup2(fd, 1)` still has nowhere to land and
//! `cmd | cmd` and `echo x > file` still fail. Making them real entries is the
//! next step and is now a fill-in rather than a redesign: the rows already have
//! the three slots, held at [`NO_FILE`].

use akuma_exec_core::process::{FileDescriptor, KernelFile};
use akuma_selftest::Suite;
use akuma_terminal::TerminalState;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::fs;
use crate::serial;

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

/// Descriptors 0, 1 and 2 are the console and are never in the table.
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

/// How many **open file descriptions** exist machine-wide.
///
/// The other half of the POSIX split: a descriptor is a per-process *name*, and
/// what it names is one of these — the thing that carries the cursor, the
/// cached contents and the socket or pipe identity. `dup` and `fork` make a
/// second name for the same description, which is why this is reference
/// counted and `MAX_FDS` is not.
///
/// Deliberately smaller than `MAX_FDS * PROC_SLOTS` (256 × 128 = 32768): that
/// product is what the *names* can address, and sizing the descriptions to it
/// would reserve for a machine where every process holds a full table at once.
/// A description costs a `Vec` of the file's contents; 512 is the ceiling on
/// concurrently-open *files*, and running into it is a real condition worth
/// reporting rather than a limit worth pre-allocating past.
const MAX_FILES: usize = 512;

/// One entry: the tree's descriptor, plus this target's cached contents.
///
/// The cursor lives in the `KernelFile`'s own `position`, not beside it — so a
/// future move to reading by inode changes where the *bytes* come from and
/// nothing else.
#[derive(Clone)]
struct Entry {
    desc: FileDescriptor,
    /// The file's contents, cached at `open`. See the module header. Empty and
    /// unused for a directory descriptor (`is_dir`) — `getdents64` reads
    /// `desc`'s `KernelFile::dir_cache` instead, not this.
    data: Vec<u8>,
    /// `O_NONBLOCK`, set through `fcntl(F_SETFL)`. Only sockets consult it —
    /// `sshd`'s cooperative loop makes its listener and every accepted stream
    /// non-blocking so a session idling on its socket suspends instead of
    /// stalling its peers.
    nonblocking: bool,
    /// Was this fd opened on a directory? Set once at `open`, from
    /// `fs::metadata` — never a socket or pipe, so those constructors always
    /// pass `false`. Only `getdents64` may read a directory descriptor;
    /// `read`/`write` on one report `EISDIR`/`EBADF` per POSIX.
    is_dir: bool,
    /// How many descriptors name this description.
    ///
    /// One at `open`. `dup` and `fork` add a name without adding a
    /// description, so both increment; `close` decrements and only the
    /// transition to zero runs [`release`] — which is what persists a written
    /// file, closes a socket and frees a pipe.
    ///
    /// This field replaced an `owner: usize` (the `PROCS` slot that opened the
    /// fd), which existed because the table used to be one flat array shared
    /// by every task: nothing reclaimed a task's fds when it exited, and
    /// `apk` installing 14 packages left the *next* `apk` starting from a
    /// full table. Ownership is now expressed by which process's row holds the
    /// name, so the sweep at exit is "clear this row" and the lifetime
    /// question the `owner` field was answering badly is answered by this
    /// count instead.
    refs: u32,
}

/// Allocate a descriptor for an already-created socket.
///
/// Sockets live in the same table as files, as `FileDescriptor::Socket(idx)` —
/// the same variant the AArch64 kernel uses, carrying the same index into the
/// same `akuma_net::socket` table. Sharing the table is what makes `read` and
/// `write` work on a socket without the caller knowing.
pub fn alloc_socket_fd(idx: usize) -> Option<u64> {
    let fd = install(Entry {
        desc: FileDescriptor::Socket(idx),
        data: Vec::new(),
        nonblocking: false,
        is_dir: false,
        refs: 1,
    });
    (!errno::is_err(fd)).then_some(fd)
}

/// The socket index behind `fd`, or `None` if it is not a socket.
#[must_use]
pub fn socket_index(fd: u64) -> Option<usize> {
    with_file(fd, |e| match e.desc {
        FileDescriptor::Socket(s) => Some(s),
        _ => None,
    })?
}

/// Give `pipe_id` a descriptor: `PipeRead` for a reader end, `PipeWrite` for a
/// writer end. Used by `sys_spawn` (the parent's stdout reader) and
/// `sys_openat`'s `/proc/<pid>/fd/0` (the parent's stdin writer).
pub fn alloc_pipe_fd(pipe_id: usize, is_write: bool) -> Option<u64> {
    let desc = if is_write {
        FileDescriptor::PipeWrite(pipe_id as u32)
    } else {
        FileDescriptor::PipeRead(pipe_id as u32)
    };
    let fd = install(Entry {
        desc,
        data: Vec::new(),
        nonblocking: false,
        is_dir: false,
        refs: 1,
    });
    (!errno::is_err(fd)).then_some(fd)
}

/// The pipe id behind `fd` if it is a `PipeRead` descriptor.
#[must_use]
pub fn pipe_read_id(fd: u64) -> Option<usize> {
    with_file(fd, |e| match e.desc {
        FileDescriptor::PipeRead(p) => Some(p as usize),
        _ => None,
    })?
}

/// The pipe id behind `fd` if it is a `PipeWrite` descriptor.
#[must_use]
pub fn pipe_write_id(fd: u64) -> Option<usize> {
    with_file(fd, |e| match e.desc {
        FileDescriptor::PipeWrite(p) => Some(p as usize),
        _ => None,
    })?
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
#[must_use]
pub fn is_nonblocking(fd: u64) -> bool {
    with_file(fd, |e| e.nonblocking).unwrap_or(false)
}

/// `fcntl(fd, cmd, arg)`. Only the two flag commands are implemented, and
/// `F_SETFL` only inspects the `O_NONBLOCK` bit — `sshd` is the sole caller and
/// that is all it sets. `F_GETFL` reports the same bit back and nothing else.
pub fn sys_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const F_SETFD: u64 = 2;
    const F_GETFD: u64 = 1;
    const O_NONBLOCK: u64 = 0x800;

    with_file(fd, |entry| match cmd {
        F_SETFL => {
            entry.nonblocking = arg & O_NONBLOCK != 0;
            0
        }
        F_GETFL => {
            if entry.nonblocking {
                O_NONBLOCK
            } else {
                0
            }
        }
        // **`FD_CLOEXEC` is accepted and ignored — a pinned divergence.** It
        // used to be "meaningless without exec", and that stopped being true
        // when this target grew a real `execve`: a descriptor a caller marked
        // close-on-exec now survives one. Nothing here has been bitten by it
        // yet because the flag is set defensively far more often than it is
        // relied on, and the honest fix is a per-descriptor flag word in the
        // row next to the `FileIdx` — cheap, but it belongs with the work that
        // makes 0/1/2 real entries rather than bolted on ahead of it.
        F_SETFD => 0,
        F_GETFD => 0,
        _ => errno::EINVAL,
    })
    .unwrap_or(errno::EBADF)
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

impl Entry {
    /// The `KernelFile` inside, which every operation here needs.
    ///
    /// Only `FileDescriptor::File` is ever stored in this table — the console
    /// descriptors are 0/1/2 and never enter it — so a different variant is a
    /// bug in this module rather than a case to handle.
    fn file(&mut self) -> Option<&mut KernelFile> {
        match &mut self.desc {
            FileDescriptor::File(f) => Some(f),
            _ => None,
        }
    }
}

/// The machine-wide table of open file **descriptions**.
///
/// One entry per `open`/`socket`/`pipe`, not per descriptor. Nothing outside
/// this module indexes it: a description is reached only by resolving a
/// descriptor through [`FDS`].
static FILES: Spinlock<[Option<Entry>; MAX_FILES]> =
    Spinlock::new([const { None }; MAX_FILES]);

/// An index into [`FILES`], or [`NO_FILE`] for a closed descriptor.
type FileIdx = u16;
const NO_FILE: FileIdx = FileIdx::MAX;

/// One row per `PROCS` slot, plus one on the end for the kernel.
///
/// The kernel row is not a courtesy: the boot self-tests open files, and
/// `current_proc_slot()` answers `usize::MAX` when no user task is running.
/// Folding that onto row 0 would have the suite share a table with pid 1.
const KERNEL_ROW: usize = crate::usermode::PROC_SLOTS;
const FD_ROWS: usize = crate::usermode::PROC_SLOTS + 1;

/// Per-process descriptor tables: `FDS[row][fd]` is the description `fd` names.
///
/// Indexed by descriptor *number*, so `FDS[row][7]` is that process's fd 7 and
/// every process's fd 3 is its own. Entries below [`FIRST_FILE_FD`] are always
/// [`NO_FILE`] — 0/1/2 are still answered by number above this layer, which is
/// why `dup2` onto them cannot work yet (see the module header).
///
/// 129 rows × 256 × 2 bytes = 64 KiB of `.bss`, allocated once and never grown.
/// A row of `u16` rather than a row of descriptions is the whole point: `fork`
/// gives the child its own *names* for its parent's *descriptions*, which is a
/// 512-byte copy and 256 increments, not a copy of any file's contents.
static FDS: Spinlock<[[FileIdx; MAX_FDS]; FD_ROWS]> =
    Spinlock::new([[NO_FILE; MAX_FDS]; FD_ROWS]);

/// Which row the calling context's descriptors live in.
fn current_row() -> usize {
    let slot = crate::usermode::current_proc_slot();
    if slot < crate::usermode::PROC_SLOTS { slot } else { KERNEL_ROW }
}

/// The description `fd` names in the calling process, or `None` if it names
/// nothing — a closed descriptor, one out of range, or a console descriptor.
fn file_index(fd: u64) -> Option<usize> {
    let fd = usize::try_from(fd).ok()?;
    if fd >= MAX_FDS {
        return None;
    }
    let idx = FDS.lock()[current_row()][fd];
    (idx != NO_FILE).then_some(idx as usize)
}

/// Does `fd` name an open file description in the calling process?
///
/// The question `sys_write` asks before falling back to the console: a bound
/// 1 or 2 has been redirected and must go where the row says, not to the
/// serial port.
#[must_use]
pub fn is_bound(fd: u64) -> bool {
    file_index(fd).is_some()
}

/// Borrow the description `fd` names, for the duration of `f`.
///
/// The two locks are taken one at a time and are **never nested**: [`FILES`]
/// and [`FDS`] have no ordering between them today and must not acquire one.
/// Resolving the name and then operating on the description is two separate
/// holds, which is safe here for the same reason the rest of this target is —
/// one core, under the BKL — and would need a real hold across both the day
/// `fork` runs at SMP > 1.
fn with_file<R>(fd: u64, f: impl FnOnce(&mut Entry) -> R) -> Option<R> {
    let fi = file_index(fd)?;
    FILES.lock()[fi].as_mut().map(f)
}

/// Put `entry` in the description table with one reference.
fn intern(entry: Entry) -> Option<usize> {
    let mut files = FILES.lock();
    for (i, slot) in files.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(entry);
            return Some(i);
        }
    }
    None
}

/// Name the description `fi` with the lowest free descriptor in `row`,
/// **starting at [`FIRST_FILE_FD`]**.
///
/// **Divergence, pinned.** POSIX's "lowest available" includes 0/1/2, so on
/// Linux `close(1); open(f)` returns 1. Here it returns 3, because an *unbound*
/// 0/1/2 is not "free" — it is the console, or a spawned child's pipe, routed
/// by number below this layer. Redirection through `dup2`, which is what every
/// shell in practice emits (`open` → `dup2(fd,1)` → `close(fd)`), is exact;
/// the close-then-open idiom is what would land somewhere else. Closing that
/// gap means giving 0/1/2 real default descriptions at process creation, which
/// is a bigger change than making `dup2` land.
fn bind(row: usize, fi: usize) -> Option<u64> {
    let mut fds = FDS.lock();
    let row = &mut fds[row];
    for (fd, slot) in row.iter_mut().enumerate().skip(FIRST_FILE_FD) {
        if *slot == NO_FILE {
            *slot = fi as FileIdx;
            return Some(fd as u64);
        }
    }
    None
}

/// Intern `entry` and give the calling process a descriptor for it.
///
/// The two exhaustion cases are different errnos and are worth keeping apart:
/// `EMFILE` is *this process* out of descriptor numbers, `ENFILE` is the
/// machine out of open file descriptions. Reporting the second as the first
/// sends whoever reads it looking for a leak in the wrong program.
fn install(entry: Entry) -> u64 {
    let Some(fi) = intern(entry) else {
        return errno::ENFILE;
    };
    let Some(fd) = bind(current_row(), fi) else {
        // Un-intern rather than leak: the description has no name and so no
        // `close` will ever reach it.
        FILES.lock()[fi] = None;
        return errno::EMFILE;
    };
    fd
}

/// Drop one reference to description `fi`, releasing it if that was the last.
///
/// The description is taken out from under the lock before [`release`] runs:
/// releasing reaches into `akuma_net` and `crate::pipe` and writes a file back
/// to disk, and holding the description table across any of those is how a
/// lock inversion starts.
fn unref(fi: usize) {
    let taken = {
        let mut files = FILES.lock();
        let Some(entry) = files[fi].as_mut() else {
            return;
        };
        entry.refs = entry.refs.saturating_sub(1);
        if entry.refs > 0 {
            return;
        }
        files[fi].take()
    };
    if let Some(entry) = taken {
        release(entry);
    }
}

/// Give `child_slot` its own names for every description `parent_slot` holds.
///
/// This is `fork`'s half of the descriptor table. The child gets an
/// independent *row* — so a `close` in the child no longer reaches into the
/// parent, which is what the single shared table did and what made
/// `sh -c 'prog > file'` unsurvivable — while both rows name the same
/// descriptions, so the cursor and the socket really are shared, as POSIX
/// requires.
pub fn inherit_fds(parent_slot: usize, child_slot: usize) {
    if parent_slot >= FD_ROWS || child_slot >= FD_ROWS {
        return;
    }
    // Drop whatever the child's row held first. It should hold nothing — the
    // slot was found free — but overwriting a stale row would strand every
    // reference in it, and a description no `close` can reach is a leak the
    // machine never recovers from.
    close_owned_by(child_slot);
    let inherited = {
        let mut fds = FDS.lock();
        let row = fds[parent_slot];
        fds[child_slot] = row;
        row
    };
    let mut files = FILES.lock();
    for fi in inherited.iter().filter(|&&fi| fi != NO_FILE) {
        if let Some(entry) = files[*fi as usize].as_mut() {
            entry.refs = entry.refs.saturating_add(1);
        }
    }
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
    let base = with_file(dirfd, |entry| {
        if !entry.is_dir {
            return None;
        }
        match &entry.desc {
            FileDescriptor::File(f) => Some(f.path.clone()),
            _ => None,
        }
    })
    .flatten();
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
    const O_NOFOLLOW_X86: u64 = 0o400_000;
    let normalised = if flags_ & O_NOFOLLOW_X86 == 0 {
        fs::resolve_symlinks(&normalised)
    } else {
        normalised
    };
    // `O_TMPFILE` (x86_64 encoding, `0o20200000`) is answered with `EINVAL`,
    // as Linux kernels without tmpfile support do. This used to be *missing*,
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
    const O_TMPFILE_X86: u64 = 0o20200000;
    if flags_ & O_TMPFILE_X86 == O_TMPFILE_X86 {
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
    }

    // A directory has no bytes to cache as file contents — `read_file` would
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
    // What the descriptor's buffer starts as, and where its cursor starts.
    //
    // This used to be "empty if `creating`, else the file's bytes", which
    // collapsed three different opens into one: `O_TRUNC` (start empty),
    // `O_APPEND` (start at the end) and a plain `O_CREAT` on a file that
    // already exists (start at the beginning, keeping what is there — POSIX
    // truncates only when asked). Since `close` persists this buffer as the
    // file's *entire* contents, getting it wrong does not mis-position a
    // write, it destroys the rest of the file.
    let truncating = flags_ & u64::from(open_flags::O_TRUNC) != 0;
    let appending = flags_ & u64::from(open_flags::O_APPEND) != 0;
    let data = if is_dir || truncating {
        Vec::new()
    } else {
        match fs::read_file(&normalised) {
            Ok(d) => d,
            // A brand-new `O_CREAT` file: nothing to read, and that is not an
            // error. Without `O_CREAT` it is `ENOENT`.
            Err(_) if creating => Vec::new(),
            Err(_) => return errno::ENOENT,
        }
    };
    let start_pos = if appending { data.len() } else { 0 };

    // `KernelFile::new` leaves the inode 0 — "read by path", which is what
    // this target does — and the position 0, which `O_APPEND` overrides.
    //
    // **Divergence, pinned.** Real `O_APPEND` re-seeks to the end before
    // *every* write, so two processes appending to one file interleave whole
    // records. Here it only sets the starting cursor. That is right for a
    // shell's `>>` and wrong for concurrent appenders, and the fix is not
    // local: this target keeps a private copy of the file per descriptor and
    // writes it back whole at `close`, so two appenders already lose each
    // other's data regardless of where the cursor starts.
    let mut file = KernelFile::new(normalised, flags_ as u32);
    file.position = start_pos;
    install(Entry {
        desc: FileDescriptor::File(file),
        data,
        nonblocking: false,
        is_dir,
        refs: 1,
    })
}

/// `O_CREAT`, `O_WRONLY`, `O_RDWR`, `O_TRUNC` — the bits [`sys_openat`] and
/// [`sys_write_file`] decode from the raw `flags` word. Spelled out locally
/// rather than pulled from `akuma_syscalls_linux` because those are the
/// AArch64/`asm-generic` values; on x86_64 they happen to share the same
/// numeric encoding (`open(2)`'s flag bits are one of the few things the two
/// architectures never diverged on), but naming that coincidence explicitly
/// here is cheaper than a reader having to go check.
pub mod open_flags {
    pub const O_ACCMODE: u32 = 0o3;
    pub const O_CREAT: u32 = 0o100;
    /// Start from nothing. x86_64 `0o1000`.
    pub const O_TRUNC: u32 = 0o1000;
    /// Start at the end. x86_64 `0o2000`.
    ///
    /// Added 2026-09-06 with `O_TRUNC`, because until then neither was read at
    /// all: **any** `O_CREAT` open began with an empty buffer, so `>>` behaved
    /// exactly like `>`. That was invisible while `dup2` did not work — nothing
    /// could redirect in the first place — and became a data-losing bug the
    /// moment it did. `echo a > f; echo b >> f` left `f` holding only `b`.
    pub const O_APPEND: u32 = 0o2000;
}

/// `mkdirat(dirfd, path, mode)` — x86_64 258. `mode` is not tracked (one
/// user, like `access`). First consumer: `apk`'s cache-directory setup.
pub fn sys_mkdirat(dirfd: u64, path: u64, _mode: u64) -> u64 {
    let Some(raw) = path_from_user(path) else {
        return errno::EFAULT;
    };
    let Ok(path) = resolve_at(dirfd, raw) else {
        return errno::ENOTDIR;
    };
    match fs::create_dir(&path) {
        Ok(()) => 0,
        Err(akuma_vfs::FsError::AlreadyExists) => errno::EEXIST,
        Err(akuma_vfs::FsError::NotFound) => errno::ENOENT,
        Err(_) => errno::EIO,
    }
}

/// `unlinkat(dirfd, path, flags)` — x86_64 263. `AT_REMOVEDIR` (`0x200`)
/// selects `rmdir`. First consumer: `apk`'s stale-cache cleanup.
pub fn sys_unlinkat(dirfd: u64, path: u64, flags: u64) -> u64 {
    const AT_REMOVEDIR: u64 = 0x200;
    let Some(raw) = path_from_user(path) else {
        return errno::EFAULT;
    };
    let Ok(path) = resolve_at(dirfd, raw) else {
        return errno::ENOTDIR;
    };
    match fs::remove(&path, flags & AT_REMOVEDIR != 0) {
        Ok(()) => 0,
        Err(akuma_vfs::FsError::NotFound) => errno::ENOENT,
        Err(akuma_vfs::FsError::DirectoryNotEmpty) => errno::ENOTEMPTY,
        Err(_) => errno::EIO,
    }
}

/// `renameat(olddirfd, oldpath, newdirfd, newpath)` — x86_64 264. Both
/// `dirfd`s are ignored (root-relative), as everywhere on this target. First
/// consumer: `apk`'s atomic `.tmp.<pid>` + rename cache write — the fallback
/// its `__apk_ostream_to_file` uses when `O_TMPFILE` is refused (which it now
/// is, above).
pub fn sys_renameat(olddirfd: u64, oldpath: u64, newdirfd: u64, newpath: u64) -> u64 {
    let Some(old_raw) = path_from_user(oldpath) else {
        return errno::EFAULT;
    };
    let Some(new_raw) = path_from_user(newpath) else {
        return errno::EFAULT;
    };
    let Ok(old) = resolve_at(olddirfd, old_raw) else {
        return errno::ENOTDIR;
    };
    let Ok(new) = resolve_at(newdirfd, new_raw) else {
        return errno::ENOTDIR;
    };
    match fs::rename(&old, &new) {
        Ok(()) => 0,
        Err(e) => {
            // Diagnostic for the apk cache-rename ENOENT (2026-09-04): the
            // persist reported success yet the rename could not find the file.
            serial::puts("[renameat] failed: old=\"");
            serial::puts(&old);
            serial::puts("\" new=\"");
            serial::puts(&new);
            serial::puts("\" base10=\"");
            serial::puts(&fd_path_debug(10));
            serial::puts("\" base7=\"");
            serial::puts(&fd_path_debug(7));
            serial::puts("\" err=");
            serial::puts(fs_err_str(e));
            serial::puts("\n");
            errno::ENOENT
        }
    }
}

/// Static name for a VFS error, for console reporting (no allocation).
#[must_use]
fn fd_path_debug(fd: u64) -> alloc::string::String {
    if fd < FIRST_FILE_FD as u64 {
        return alloc::format!("<console {fd}>");
    }
    with_file(fd, |entry| match &entry.desc {
        FileDescriptor::File(f) => f.path.clone(),
        _ => alloc::format!("<non-file {fd}>"),
    })
    .unwrap_or_else(|| alloc::format!("<free {fd}>"))
}

/// Static name for a VFS error, for console reporting (no allocation).
fn fs_err_str(e: akuma_vfs::FsError) -> &'static str {
    use akuma_vfs::FsError as E;
    match e {
        E::NotFound => "not found",
        E::PermissionDenied => "permission denied",
        E::AlreadyExists => "already exists",
        E::NotADirectory => "not a directory",
        E::NotAFile => "not a file",
        E::DirectoryNotEmpty => "directory not empty",
        E::NoSpace => "no space",
        E::InvalidPath => "invalid path",
        E::Corrupt => "corrupt",
        _ => "filesystem error",
    }
}

/// `symlinkat(target, newdirfd, link_path)` — x86_64 266. Note the argument
/// order: the TARGET comes first, per POSIX. First consumer: `apk add` —
/// versioned-library symlinks (`libc.musl-x86_64.so.1`) are package contents,
/// and each failure was a counted install error.
pub fn sys_symlinkat(target: u64, newdirfd: u64, link_path: u64) -> u64 {
    let Some(target) = path_from_user(target) else {
        return errno::EFAULT;
    };
    let Some(link) = path_from_user(link_path) else {
        return errno::EFAULT;
    };
    let Ok(link) = resolve_at(newdirfd, link) else {
        return errno::ENOTDIR;
    };
    match fs::create_symlink(&link, &target) {
        Ok(()) => 0,
        Err(akuma_vfs::FsError::AlreadyExists) => errno::EEXIST,
        Err(akuma_vfs::FsError::NotFound) => errno::ENOENT,
        Err(_) => errno::EIO,
    }
}

/// `readlinkat(dirfd, path, buf, bufsiz)` — x86_64 267. Replaces the old
/// "return EINVAL" answer, which was honest while no symlink could exist and
/// became a lie the moment [`sys_symlinkat`] landed.
pub fn sys_readlinkat(dirfd: u64, path: u64, buf: u64, bufsiz: u64) -> u64 {
    let Some(raw) = path_from_user(path) else {
        return errno::EFAULT;
    };
    let Ok(path) = resolve_at(dirfd, raw) else {
        return errno::ENOTDIR;
    };
    // `Option`, not `Result`: the crate reports "not a symlink" and "no such
    // path" the same way, and `readlink(2)` distinguishes them as `EINVAL` vs
    // `ENOENT`. Collapsing both to `ENOENT` is what this target already did.
    match fs::read_symlink(&path) {
        Some(target) => {
            let bytes = target.as_bytes();
            let n = (bytes.len() as u64).min(bufsiz);
            let r = copy_to_user(buf, &bytes[..n as usize]);
            if errno::is_err(r) {
                return r;
            }
            n
        }
        None => errno::ENOENT,
    }
}

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
        Err(akuma_vfs::FsError::NotFound) => errno::ENOENT,
        Err(akuma_vfs::FsError::NotSupported) => errno::ENOSYS,
        Err(_) => errno::EIO,
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
/// **The pinned "value copy" divergence is gone.** This used to clone the
/// whole entry, so the two descriptors shared nothing — separate cursors,
/// separate cached contents, and a `close` on either releasing the underlying
/// socket or pipe outright. It was right for the one shape apk uses (hold a
/// reference across a close) and wrong for everything else. Now that a
/// descriptor is a *name* and the description is refcounted, `dup` is the real
/// thing: one more name for one description, a shared cursor, and a release
/// only when the last name goes.
pub fn sys_dup(fd: u64) -> u64 {
    let Some(fi) = file_index(fd) else {
        return errno::EBADF;
    };
    let Some(newfd) = bind(current_row(), fi) else {
        return errno::EMFILE;
    };
    // Bumped only once the new name exists: an increment before a failed
    // `bind` would strand the description at a count no `close` can reach.
    if let Some(entry) = FILES.lock()[fi].as_mut() {
        entry.refs = entry.refs.saturating_add(1);
    }
    newfd
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
    let Some(fi) = file_index(oldfd) else {
        return errno::EBADF;
    };
    if oldfd == newfd {
        return if strict_same { errno::EINVAL } else { newfd };
    }
    let Some(new_idx) = usize::try_from(newfd).ok().filter(|f| *f < MAX_FDS) else {
        return errno::EBADF;
    };

    // Install the new name and take out whatever it displaced, in one hold, so
    // no window exists in which `newfd` names nothing. Then bump, then release
    // the displaced description outside the lock.
    let displaced = {
        let mut fds = FDS.lock();
        core::mem::replace(&mut fds[current_row()][new_idx], fi as FileIdx)
    };
    if let Some(entry) = FILES.lock()[fi].as_mut() {
        entry.refs = entry.refs.saturating_add(1);
    }
    if displaced != NO_FILE {
        unref(displaced as usize);
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
    let read_fd = install(Entry {
        desc: FileDescriptor::PipeRead(id as u32),
        data: Vec::new(),
        nonblocking: false,
        is_dir: false,
        refs: 1,
    });
    if errno::is_err(read_fd) {
        crate::pipe::free(id);
        return read_fd;
    }
    let write_fd = install(Entry {
        desc: FileDescriptor::PipeWrite(id as u32),
        data: Vec::new(),
        nonblocking: false,
        is_dir: false,
        refs: 1,
    });
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
    // An *unbound* console descriptor: closing succeeds and does nothing, so a
    // program that closes stdin does not then find the kernel refusing to
    // print. A **bound** one has been redirected and is a real descriptor —
    // `sh` does `dup2(f,1); close(f)` and later `close(1)`, and that last close
    // has to reach the file or its buffered contents are never persisted.
    if fd < FIRST_FILE_FD as u64 && !is_bound(fd) {
        return 0;
    }
    // Unbind the *name* first, then drop the reference. Only the last name
    // going away releases the description, which is why `close` on one of two
    // dups no longer tears the socket down under the other one.
    let Some(fd_idx) = usize::try_from(fd).ok().filter(|f| *f < MAX_FDS) else {
        return errno::EBADF;
    };
    let fi = {
        let mut fds = FDS.lock();
        let slot = &mut fds[current_row()][fd_idx];
        if *slot == NO_FILE {
            return errno::EBADF;
        }
        core::mem::replace(slot, NO_FILE)
    };
    unref(fi as usize);
    0
}

/// Tear down an open file description whose last descriptor has gone.
///
/// Called only from [`unref`], which has already taken the entry out of
/// [`FILES`]: releasing reaches into the network stack, the pipe table and the
/// filesystem, and holding the description table across any of those is how a
/// lock inversion starts.
fn release(entry: Entry) {
    match entry {
        Entry { desc: FileDescriptor::Socket(s), .. } => crate::sock::close(s),
        // Closing the write end (the `/proc/<pid>/fd/0` handle) signals EOF to
        // the child. It frees the pipe only if this was the last end: the child
        // may still be draining buffered input, and buffered bytes outlive
        // their writer.
        Entry { desc: FileDescriptor::PipeWrite(p), .. } => {
            crate::pipe::close_write(p as usize);
        }
        // Closing the read end (`sshd`'s stdout reader) is the last *reader* of
        // a spawned child's stdout pipe — `waitpid` deliberately left it alive
        // for this final drain. Whether that also destroys the pipe is the end
        // counts' decision now, not this arm's: a `pipe(2)` pair whose writer is
        // still open keeps its buffer, and the child's own `close_write` at exit
        // is what completes the pair.
        Entry { desc: FileDescriptor::PipeRead(p), .. } => {
            crate::pipe::close_read(p as usize);
        }
        // A file opened for writing: this is the one and only point its
        // buffered `data` reaches the disk (see the module header and
        // `sys_write_file`) — `akuma-ext2`'s `write_file` replaces the whole
        // file in one call, so there is nothing to flush incrementally.
        // Read-only opens skip this: writing back an unmodified `read_file`
        // copy on every `close` would be a silent no-op turned into needless
        // disk I/O.
        Entry { desc: FileDescriptor::File(file), data, is_dir: false, .. }
            if file.flags & open_flags::O_ACCMODE != 0 =>
        {
            // A failed persist is REPORTED, not discarded: this is the whole
            // file's worth of data, there is no second chance, and a silent
            // loss here once surfaced as apk's rename finding no tmp file —
            // an `ENOENT` pointing three layers away from the real failure.
            // (`close(2)` still returns 0 — Linux's errno slots are taken and
            // the data is already unreachable — so the console line is the
            // caller's only signal.)
            if let Err(e) = fs::write_file(&file.path, &data) {
                serial::puts("[close] persist failed for \"");
                serial::puts(&file.path);
                serial::puts("\": ");
                serial::puts(fs_err_str(e));
                serial::puts("\n");
            }
        }
        _ => {}
    }
}

/// Drop every descriptor `proc_slot` still holds. Real Linux does this
/// implicitly at `exit`/`exit_group`.
///
/// It exists because the fd table used to be one array shared by every task
/// and nothing swept it on exit — a leak that stayed invisible through every
/// self-test (each opens a handful of fds and closes them itself) and only
/// showed up 2026-09-04 running `apk` twice in a row: the second invocation
/// started from a table the first had already filled and failed at the
/// database write with `EMFILE` before doing any real work. With per-process
/// rows the leak is gone by construction, and this is now what it always
/// should have been — the row is released, not searched. Called once, from
/// `run_process`, right after a task's last `enter_user_mode` returns.
///
/// **A row is dropped, not each fd closed.** Descriptions this row shares with
/// a live process (through `fork` or `dup`) survive: only the reference goes.
pub fn close_owned_by(proc_slot: usize) {
    if proc_slot >= FD_ROWS {
        return;
    }
    // Clear the whole row in one hold, then drop the references outside it.
    // Going through `sys_close` per fd is no longer possible and no longer
    // wanted: it resolves against the *calling* row, and this runs after the
    // task it is cleaning up has stopped being the current one.
    let row = {
        let mut fds = FDS.lock();
        core::mem::replace(&mut fds[proc_slot], [NO_FILE; MAX_FDS])
    };
    for fi in row.iter().filter(|&&fi| fi != NO_FILE) {
        unref(*fi as usize);
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
            // A spawned child's stdin is a pipe, not the console — `sshd`
            // feeds it.
            if let Some(pid) = crate::usermode::current_stdin_pipe() {
                return read_pipe(pid, buf, len as usize, false);
            }
            return read_console(buf, len as usize);
        }
        return errno::EBADF;
    }

    with_file(fd, |entry| {
        if entry.is_dir {
            return errno::EISDIR;
        }
        let total = entry.data.len();
        let Some(file) = entry.file() else {
            return errno::EBADF;
        };
        let pos = file.position;
        let n = total.saturating_sub(pos).min(len as usize);
        if n == 0 {
            return 0;
        }
        file.position = pos + n;
        let chunk = &entry.data[pos..pos + n];
        copy_to_user(buf, chunk)
    })
    .unwrap_or(errno::EBADF)
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

    with_file(fd, |entry| {
        if entry.is_dir {
            return errno::EISDIR;
        }
        // Read-only descriptors are fine; what is not is a descriptor that is
        // not a file at all. `entry.file()` is the same gate `sys_read` uses.
        if entry.file().is_none() {
            return errno::EBADF;
        }
        let total = entry.data.len();
        let pos = off as usize;
        // Past the end is 0, not an error — `read(2)`'s rule, and what a
        // digest loop reading to EOF depends on to terminate.
        let n = total.saturating_sub(pos).min(len as usize);
        if n == 0 {
            return 0;
        }
        // **`file.position` is deliberately not touched.** That is the entire
        // contract of this call.
        copy_to_user(buf, &entry.data[pos..pos + n])
    })
    .unwrap_or(errno::EBADF)
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
/// This reads `Entry::data` — the whole-file buffer `sys_openat` fills. That is
/// what makes a file mapping cheap to serve here and also what bounds it: a file
/// that could not be read into the kernel cannot be mapped either, and both fail
/// at `open`.
pub fn file_bytes_at(fd: u64, offset: usize, dst: &mut [u8]) -> Option<usize> {
    with_file(fd, |entry| {
        if entry.is_dir || entry.file().is_none() {
            return None;
        }
        let total = entry.data.len();
        let n = total.saturating_sub(offset).min(dst.len());
        if n > 0 {
            dst[..n].copy_from_slice(&entry.data[offset..offset + n]);
        }
        Some(n)
    })
    .flatten()
}

/// Is `fd` a regular file — something `mmap` can back a mapping with?
///
/// Separate from [`file_bytes_at`] because `mmap` has to refuse a socket, a pipe
/// or a directory **before** it places a region, and a zero-byte answer from the
/// copy is not the same thing as "this cannot be mapped".
#[must_use]
pub fn is_regular_file(fd: u64) -> bool {
    with_file(fd, |entry| !entry.is_dir && entry.file().is_some()).unwrap_or(false)
}

/// `write(fd, buf, len)` on a real file descriptor — everything `sys_write` in
/// `usermode.rs` does not itself handle (console, pipe, socket).
///
/// Writes into the descriptor's own `data` buffer at its cursor, growing it as
/// needed; nothing reaches the disk here. `akuma-ext2`'s `write_file` replaces
/// a file's entire contents in one call, so writing back after every `write(2)`
/// would mean re-writing everything already written on each subsequent call —
/// quadratic, and pointless before the program is done. `sys_close` is the one
/// place this buffer is ever persisted; see the module header.
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

    let mut written: usize = 0;
    while written < len as usize {
        let chunk_len = ((len as usize) - written).min(MAX_IO as usize);
        // Copied in before the lock: there is no reason to hold `FILES`
        // across a user copy.
        let Some(incoming) = copy_in(buf + written as u64, chunk_len as u64) else {
            return if written == 0 { errno::EFAULT } else { written as u64 };
        };
        let step = with_file(fd, |entry| {
            if entry.is_dir {
                return Err(errno::EISDIR);
            }
            // The position read and the write-permission check both come from
            // `entry.desc`, borrowed immutably first so the mutable borrow of
            // `entry.data` just below is not fighting a live borrow of a
            // sibling field through the same `&mut Entry` — `entry.file()`
            // would hold that borrow for the rest of the closure if used here.
            let pos = match &entry.desc {
                FileDescriptor::File(f) if f.flags & open_flags::O_ACCMODE != 0 => f.position,
                FileDescriptor::File(_) => return Err(errno::EBADF), // opened read-only
                _ => return Err(errno::EBADF),
            };
            let end = pos + incoming.len();
            if entry.data.len() < end {
                entry.data.resize(end, 0);
            }
            entry.data[pos..end].copy_from_slice(&incoming);
            if let FileDescriptor::File(f) = &mut entry.desc {
                f.position = end;
            }
            Ok(incoming.len())
        });
        match step {
            Some(Ok(n)) => written += n,
            Some(Err(e)) => return e,
            None => return errno::EBADF,
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
    if let Some(pipe_id) = crate::usermode::current_stdin_pipe() {
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

    if fd < FIRST_FILE_FD as u64 {
        return errno::EBADF;
    }
    let Some(fi) = file_index(fd) else {
        return errno::EBADF;
    };
    let mut files = FILES.lock();
    let Some(entry) = files[fi].as_mut() else {
        return errno::EBADF;
    };
    let total = entry.data.len();
    let Some(file) = entry.file() else {
        return errno::EBADF;
    };
    // `offset` is signed on the wire; a negative seek from SEEK_CUR/SEEK_END is
    // legal and must not be read as an enormous unsigned value.
    let delta = offset.cast_signed();
    let base = match whence {
        SEEK_SET => 0i64,
        SEEK_CUR => i64::try_from(file.position).unwrap_or(i64::MAX),
        SEEK_END => i64::try_from(total).unwrap_or(i64::MAX),
        _ => return errno::EINVAL,
    };
    let Some(target) = base.checked_add(delta) else {
        return errno::EINVAL;
    };
    if target < 0 {
        return errno::EINVAL;
    }
    // Seeking past the end is legal; reading there returns 0.
    file.position = target as usize;
    file.position as u64
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
/// Three separate `FILES` locks rather than one held across the call: the
/// cache-miss path calls `fs::read_dir`, which takes the *other* lock
/// (`fs::ROOT`), and nothing else in this module nests the two — see
/// [`sys_openat`], which reads the file before ever touching `FILES`.
pub fn sys_getdents64(fd: u64, dirp: u64, count: u64) -> u64 {
    if count == 0 {
        return 0;
    }
    let Some(fi) = file_index(fd) else {
        return errno::EBADF;
    };

    let (path, cached) = {
        let mut files = FILES.lock();
        let Some(entry) = files[fi].as_mut() else {
            return errno::EBADF;
        };
        if !entry.is_dir {
            return errno::ENOTDIR;
        }
        let Some(file) = entry.file() else {
            return errno::EBADF;
        };
        (file.path.clone(), file.dir_cache.clone())
    };

    let entries = if let Some(c) = cached {
        c
    } else {
        let Ok(dir_entries) = fs::list_dir(&path) else {
            return errno::ENOENT;
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
        let mut files = FILES.lock();
        if let Some(entry) = files[fi].as_mut()
            && let Some(file) = entry.file()
        {
            file.dir_cache = Some(cache.clone());
        }
        cache
    };

    let mut files = FILES.lock();
    let Some(entry) = files[fi].as_mut() else {
        return errno::EBADF;
    };
    let Some(file) = entry.file() else {
        return errno::EBADF;
    };
    let position = file.position;
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
    file.position += consumed;
    // Released before the user copy: `copy_to_user` can fault, and the fault
    // handler has no business finding `FILES` held.
    drop(files);

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
/// The size comes from the cached contents (see the module header); the mode is
/// a fixed `S_IFREG | 0644` because a `KernelFile` on this target carries no
/// inode to read a real one from. A console descriptor reports `S_IFCHR`, a
/// directory descriptor `S_IFDIR` — musl's `fdopendir` fstats the fd and
/// refuses it with `ENOTDIR` unless `S_ISDIR` holds, so `ls`/`find` need this
/// to be right, not just `openat` succeeding.
pub fn sys_fstat(fd: u64, statbuf: u64) -> u64 {
    let (mode, size, nlink) = if fd < FIRST_FILE_FD as u64 && !is_bound(fd) {
        (S_IFCHR_0620, 0u64, 1u64)
    } else {
        let Some(triple) = with_file(fd, |entry| {
            if entry.is_dir {
                (S_IFDIR_0755, 0u64, 2u64)
            } else {
                (S_IFREG_0644, entry.data.len() as u64, 1u64)
            }
        }) else {
            return errno::EBADF;
        };
        triple
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
        let Some(path) = with_file(fd, |entry| match &entry.desc {
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
    // Same rule as `sys_read`: an *unbound* 0/1/2 is the console (or a spawned
    // child's pipe), a bound one has been redirected and is described by what
    // it now names — so the console answers below are guarded, not first.
    if fd < FIRST_FILE_FD as u64 && !is_bound(fd) {
        if fd == 0 {
            return match crate::usermode::current_stdin_pipe() {
                Some(p) => (crate::pipe::readable(p), false),
                None => (crate::input::has_byte(), false),
            };
        }
        return match crate::usermode::current_stdout_pipe() {
            Some(p) => (false, crate::pipe::writable(p)),
            None => (false, true),
        };
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
    let in_table = file_index(fd).is_some();
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
    install(Entry {
        desc: FileDescriptor::File(file),
        data: Vec::new(),
        nonblocking: false,
        is_dir: true,
        refs: 1,
    })
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
/// this target can name is the running one: `PROCS` is keyed by scheduler slot,
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
        return Some(install_synthetic_dir(rest, names, flags));
    }

    render_proc_file(rest).map(|data| install_synthetic_file(rest, data, flags))
}

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

/// Install a read-only fd whose contents are `data` (a generated file like
/// `/proc/net/dev`). Reads serve from `Entry::data` exactly as a cached real
/// file does.
fn install_synthetic_file(path: &str, data: Vec<u8>, flags: u64) -> u64 {
    install(Entry {
        desc: FileDescriptor::File(KernelFile::new(
            alloc::string::String::from(path),
            flags as u32,
        )),
        data,
        nonblocking: false,
        is_dir: false,
        refs: 1,
    })
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

    // `dup` is a second *name*, not a second description. Both halves matter
    // and only the second one is new: closing one of two dups used to release
    // the description outright, so the survivor read `EBADF` on a descriptor
    // that was still open. That was pinned as a divergence for as long as the
    // table stored descriptions directly.
    {
        let a = sys_openat(0, path.as_ptr() as u64, 0, 0);
        let b = sys_dup(a);
        t.check("fd: dup returns a new descriptor", b != a && !errno::is_err(b));
        let mut one = [0u8; 16];
        t.check_eq(
            "fd: reading through a dup advances the shared cursor",
            sys_read(a, one.as_mut_ptr() as u64, 8),
            8,
        );
        t.check_eq("fd: the dup sees the advanced cursor", sys_lseek(b, 0, 1), 8);
        sys_close(a);
        // The description is still named by `b`, so it must still be readable.
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

    // And it is empty again afterwards: a row that leaked would hand out a
    // descriptor above `FIRST_FILE_FD` here, which is exactly the shape of the
    // `apk`-twice-in-a-row failure that `close_owned_by` was written for.
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
