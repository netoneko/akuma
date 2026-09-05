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
//! # The table is global
//!
//! One table, not one per process. That is wrong in the way that matters as soon
//! as there is a `fork`, and right for now: this target runs one interactive
//! program at a time, and the boot self-tests that run several processes
//! concurrently use only the console descriptors, which are stateless.
//!
//! When `Process` grows a descriptor table, this moves into it unchanged — the
//! operations do not care where the array lives. Stated here so that move is a
//! relocation rather than a rewrite.

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
    pub const ENOTTY: u64 = (-25i64) as u64;
    pub const ENODEV: u64 = (-19i64) as u64;
    pub const ENOSYS: u64 = (-38i64) as u64;
    pub const ESRCH: u64 = (-3i64) as u64;
    pub const EAGAIN: u64 = (-11i64) as u64;
    pub const ENOMEM: u64 = (-12i64) as u64;
    pub const ENOTDIR: u64 = (-20i64) as u64;
    pub const EISDIR: u64 = (-21i64) as u64;
    pub const EEXIST: u64 = (-17i64) as u64;
    pub const ENOTEMPTY: u64 = (-39i64) as u64;
    pub const EIO: u64 = (-5i64) as u64;

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
/// How many files may be open at once.
///
/// A fixed array so the table allocates nothing; the *contents* are heap, but
/// the bookkeeping is not.
///
/// It was 16, on the reasoning that this is "more than a shell opens" and that
/// a small table makes a leak show up as `EMFILE` quickly rather than as steady
/// heap growth. Both halves are still true and the number was still wrong: a
/// shell is not the only thing that runs here. `apk update` opens the package
/// database, a repository index and a TLS connection per repository and gave up
/// before reaching the network at all —
///
///     ERROR: Unable to open root: No file descriptors available
///
/// — which reads like a permissions or path problem and is neither. 64 leaves
/// room for a package manager with a handful of repositories and is still small
/// enough that a descriptor leak announces itself in seconds.
pub const MAX_OPEN: usize = 64;

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
    /// The `PROCS` slot that held `current_proc_slot()` when this fd was
    /// opened. The table is one flat array shared by every task on this
    /// target (see the module header), so nothing closed a task's fds when it
    /// exited — a real apk install opens more concurrent fds than a shell
    /// ever did (its own directory-fd cache across a multi-package tar
    /// extraction) and reliably ran the table to `EMFILE` by the middle of
    /// installing 14 packages; the *next* `apk` invocation, in a fresh
    /// process, then started from an already-full table because the first
    /// process's fds were never reclaimed. [`close_owned_by`] is the fix:
    /// called once at process exit, it does exactly what [`sys_close`] would
    /// have done for each fd this task never closed itself.
    owner: usize,
}

/// Allocate a descriptor for an already-created socket.
///
/// Sockets live in the same table as files, as `FileDescriptor::Socket(idx)` —
/// the same variant the AArch64 kernel uses, carrying the same index into the
/// same `akuma_net::socket` table. Sharing the table is what makes `read` and
/// `write` work on a socket without the caller knowing.
pub fn alloc_socket_fd(idx: usize) -> Option<u64> {
    let mut table = TABLE.lock();
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(Entry {
                desc: FileDescriptor::Socket(idx),
                data: Vec::new(),
                nonblocking: false,
                is_dir: false,
                owner: crate::usermode::current_proc_slot(),
            });
            return Some((i + FIRST_FILE_FD) as u64);
        }
    }
    None
}

/// The socket index behind `fd`, or `None` if it is not a socket.
#[must_use]
pub fn socket_index(fd: u64) -> Option<usize> {
    let idx = fd.checked_sub(FIRST_FILE_FD as u64)?;
    let table = TABLE.lock();
    match table.get(idx as usize)? {
        Some(Entry { desc: FileDescriptor::Socket(s), .. }) => Some(*s),
        _ => None,
    }
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
    let mut table = TABLE.lock();
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(Entry {
                desc,
                data: Vec::new(),
                nonblocking: false,
                is_dir: false,
                owner: crate::usermode::current_proc_slot(),
            });
            return Some((i + FIRST_FILE_FD) as u64);
        }
    }
    None
}

/// The pipe id behind `fd` if it is a `PipeRead` descriptor.
#[must_use]
pub fn pipe_read_id(fd: u64) -> Option<usize> {
    let idx = fd.checked_sub(FIRST_FILE_FD as u64)?;
    match TABLE.lock().get(idx as usize)? {
        Some(Entry { desc: FileDescriptor::PipeRead(p), .. }) => Some(*p as usize),
        _ => None,
    }
}

/// The pipe id behind `fd` if it is a `PipeWrite` descriptor.
#[must_use]
pub fn pipe_write_id(fd: u64) -> Option<usize> {
    let idx = fd.checked_sub(FIRST_FILE_FD as u64)?;
    match TABLE.lock().get(idx as usize)? {
        Some(Entry { desc: FileDescriptor::PipeWrite(p), .. }) => Some(*p as usize),
        _ => None,
    }
}

/// Read from a pipe, honouring `nonblock`. A blocking read yields until data or
/// EOF — which is safe on this target only because a pipe reader is never also
/// the pipe's writer (spawn wires them to different tasks).
pub fn read_pipe(pipe_id: usize, buf: u64, len: usize, nonblock: bool) -> u64 {
    let mut tmp = alloc::vec![0u8; len.min(MAX_IO as usize)];
    loop {
        match crate::pipe::read(pipe_id, &mut tmp) {
            Some(0) => return 0, // EOF
            Some(n) => return copy_to_user(buf, &tmp[..n]),
            None if nonblock => return errno::EAGAIN,
            None => crate::sched::yield_now(),
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
        let n = crate::pipe::write(pipe_id, &data);
        if n > 0 || data.is_empty() {
            return n as u64;
        }
        if nonblock {
            return errno::EAGAIN;
        }
        crate::sched::yield_now();
    }
}

/// Is `fd` marked `O_NONBLOCK`? `false` for anything not in the table.
#[must_use]
pub fn is_nonblocking(fd: u64) -> bool {
    let Some(idx) = fd.checked_sub(FIRST_FILE_FD as u64) else {
        return false;
    };
    let table = TABLE.lock();
    matches!(table.get(idx as usize), Some(Some(e)) if e.nonblocking)
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

    let Some(idx) = fd.checked_sub(FIRST_FILE_FD as u64) else {
        return errno::EBADF;
    };
    let mut table = TABLE.lock();
    let Some(Some(entry)) = table.get_mut(idx as usize) else {
        return errno::EBADF;
    };
    match cmd {
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
        // FD_CLOEXEC is meaningless without exec; accept and ignore so a caller
        // that sets it on every fd does not fail.
        F_SETFD => 0,
        F_GETFD => 0,
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

static TABLE: Spinlock<[Option<Entry>; MAX_OPEN]> =
    Spinlock::new([const { None }; MAX_OPEN]);

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
    let Some(idx) = dirfd.checked_sub(FIRST_FILE_FD as u64) else {
        return Err(errno::ENOTDIR);
    };
    let table = TABLE.lock();
    let Some(Some(entry)) = table.get(idx as usize) else {
        return Err(errno::ENOTDIR);
    };
    if !entry.is_dir {
        return Err(errno::ENOTDIR);
    }
    let Some(FileDescriptor::File(f)) = Some(&entry.desc) else {
        return Err(errno::ENOTDIR);
    };
    let mut joined = f.path.clone();
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
        // `busybox ifconfig` with no interface name reads this to enumerate
        // devices before it will print anything. Generated, not stored.
        if rest == "net/dev" {
            let mut text = alloc::string::String::new();
            let _ = akuma_syscalls_net::write_proc_net_dev(&interfaces(), &mut text);
            return install_synthetic_file("/proc/net/dev", text.into_bytes(), flags_);
        }
        // `busybox free` / `top` read this. Only the three fields `free`
        // actually parses are filled — physical RAM the PMM was handed, what it
        // has free, and the kernel heap folded into `Cached` so the number
        // moves when a file-cache leak (see `net::mem_watch_tick`) is eating it.
        if rest == "meminfo" {
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
                 Buffers:        {:>10} kB\n\
                 Cached:         {heap_used_kib:>10} kB\n\
                 SwapTotal:      {:>10} kB\n\
                 SwapFree:       {:>10} kB\n\
                 Shmem:          {:>10} kB\n",
                0, 0, 0, 0,
            );
            return install_synthetic_file("/proc/meminfo", text.into_bytes(), flags_);
        }
        return errno::ENOENT;
    }

    let Ok(normalised) = resolve_at(dirfd, path) else {
        return errno::ENOTDIR;
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
    if creating && fs::metadata(&normalised).is_some_and(|m| m.is_dir) {
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
    let is_dir = !creating && fs::metadata(&normalised).is_some_and(|m| m.is_dir);
    if is_dir && flags_ & u64::from(open_flags::O_ACCMODE) != 0 {
        return errno::EISDIR;
    }
    let data = if creating || is_dir {
        Vec::new()
    } else {
        match fs::read_file(&normalised) {
            Some(d) => d,
            None => return errno::ENOENT,
        }
    };

    let mut table = TABLE.lock();
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            // `KernelFile::new` leaves the inode 0 — "read by path", which is
            // what this target does.
            let desc = FileDescriptor::File(KernelFile::new(normalised, flags_ as u32));
            *slot = Some(Entry {
                desc,
                data,
                nonblocking: false,
                is_dir,
                owner: crate::usermode::current_proc_slot(),
            });
            return (i + FIRST_FILE_FD) as u64;
        }
    }
    errno::EMFILE
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
    let Some(idx) = fd.checked_sub(FIRST_FILE_FD as u64) else {
        return alloc::format!("<console {fd}>");
    };
    let table = TABLE.lock();
    match table.get(idx as usize) {
        Some(Some(Entry { desc: FileDescriptor::File(f), .. })) => f.path.clone(),
        Some(Some(_)) => alloc::format!("<non-file {fd}>"),
        _ => alloc::format!("<free {fd}>"),
    }
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
    match fs::read_symlink(&path) {
        Ok(target) => {
            let bytes = target.as_bytes();
            let n = (bytes.len() as u64).min(bufsiz);
            let r = copy_to_user(buf, &bytes[..n as usize]);
            if errno::is_err(r) {
                return r;
            }
            n
        }
        Err(_) => errno::ENOENT,
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
/// **Divergence, pinned:** the copy is a *value* copy. The two descriptors
/// share nothing — an independent cursor, an independent cached-contents
/// buffer — and `close` on either releases the underlying socket or pipe
/// outright, where Linux refcounts. That is wrong for shared-offset semantics
/// and for closing one of two dups of a socket, and right for the hold-a-
/// reference-across-a-close shape apk actually uses. Revisit when a caller
/// needs the real thing.
pub fn sys_dup(fd: u64) -> u64 {
    let Some(idx) = fd.checked_sub(FIRST_FILE_FD as u64) else {
        return errno::EBADF;
    };
    let cloned = {
        let table = TABLE.lock();
        match table.get(idx as usize) {
            Some(Some(entry)) => entry.clone(),
            _ => return errno::EBADF,
        }
    };
    let mut table = TABLE.lock();
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(cloned);
            return (i + FIRST_FILE_FD) as u64;
        }
    }
    errno::EMFILE
}

/// `close(fd)`. Closing a console descriptor succeeds and does nothing — a
/// program that closes stdin should not then find the kernel refusing to print.
pub fn sys_close(fd: u64) -> u64 {
    if fd < FIRST_FILE_FD as u64 {
        return 0;
    }
    let Some(idx) = fd.checked_sub(FIRST_FILE_FD as u64) else {
        return errno::EBADF;
    };
    // Take the entry out under the lock, then release the socket outside it:
    // `remove_socket` reaches into the network stack, and holding the
    // descriptor table across that is how a lock inversion starts.
    let taken = {
        let mut table = TABLE.lock();
        match table.get_mut(idx as usize) {
            Some(slot) if slot.is_some() => slot.take(),
            _ => return errno::EBADF,
        }
    };
    match taken {
        Some(Entry { desc: FileDescriptor::Socket(s), .. }) => crate::sock::close(s),
        // Closing the write end (the `/proc/<pid>/fd/0` handle) signals EOF to
        // the child but does not free the pipe — the child may still be draining
        // buffered input.
        Some(Entry { desc: FileDescriptor::PipeWrite(p), .. }) => {
            crate::pipe::close_write(p as usize);
        }
        // Closing the read end (`sshd`'s stdout reader) is the last reference to
        // a spawned child's stdout pipe — `waitpid` deliberately left it alive
        // for this final drain. Free the slot now.
        Some(Entry { desc: FileDescriptor::PipeRead(p), .. }) => {
            crate::pipe::free(p as usize);
        }
        // A file opened for writing: this is the one and only point its
        // buffered `data` reaches the disk (see the module header and
        // `sys_write_file`) — `akuma-ext2`'s `write_file` replaces the whole
        // file in one call, so there is nothing to flush incrementally.
        // Read-only opens skip this: writing back an unmodified `read_file`
        // copy on every `close` would be a silent no-op turned into needless
        // disk I/O.
        Some(Entry { desc: FileDescriptor::File(file), data, is_dir: false, .. })
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
    0
}

/// Close every fd `proc_slot` still holds. Real Linux does this implicitly at
/// `exit`/`exit_group`; this target's fd table is one array shared by every
/// task (see the module header), and nothing swept it on task exit until now
/// — a leak that stayed invisible through every earlier self-test (each opens
/// a handful of fds and closes them itself) and only showed up 2026-09-04
/// running `apk` twice in a row: the second invocation started from a table
/// the first had already filled and failed at the database write with
/// `EMFILE` before doing any real work. Called once, from `run_process`,
/// right after a task's last `enter_user_mode` returns.
///
/// Collects the fds under the lock, then closes each through [`sys_close`]
/// afterward — closing a socket calls into `akuma_net`, and nothing here
/// should hold `TABLE`'s lock across that (the same reason [`sys_close`]
/// itself takes the entry out before matching on it).
pub fn close_owned_by(proc_slot: usize) {
    let fds: Vec<u64> = {
        let table = TABLE.lock();
        table
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| match slot {
                Some(e) if e.owner == proc_slot => Some((i + FIRST_FILE_FD) as u64),
                _ => None,
            })
            .collect()
    };
    for fd in fds {
        sys_close(fd);
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

    if fd == 0 {
        // A spawned child's stdin is a pipe, not the console — `sshd` feeds it.
        if let Some(pid) = crate::usermode::current_stdin_pipe() {
            return read_pipe(pid, buf, len as usize, false);
        }
        return read_console(buf, len as usize);
    }
    if fd == 1 || fd == 2 {
        return errno::EBADF;
    }

    // A socket descriptor routes to the network stack. The lock is dropped
    // first: `socket_recv` blocks, and holding the descriptor table across a
    // blocking wait would stop every other task from opening a file.
    if let Some(sock) = socket_index(fd) {
        return crate::sock::recv(sock, buf, len, is_nonblocking(fd));
    }

    // A pipe descriptor — the parent's read end of a spawned child's stdout.
    if let Some(pid) = pipe_read_id(fd) {
        return read_pipe(pid, buf, len as usize, is_nonblocking(fd));
    }

    let idx = fd - FIRST_FILE_FD as u64;
    let mut table = TABLE.lock();
    let Some(Some(entry)) = table.get_mut(idx as usize) else {
        return errno::EBADF;
    };
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

    let idx = fd - FIRST_FILE_FD as u64;
    let mut written: usize = 0;
    while written < len as usize {
        let chunk_len = ((len as usize) - written).min(MAX_IO as usize);
        // Copied in before the lock: there is no reason to hold `TABLE`
        // across a user copy.
        let Some(incoming) = copy_in(buf + written as u64, chunk_len as u64) else {
            return if written == 0 { errno::EFAULT } else { written as u64 };
        };
        let mut table = TABLE.lock();
        let Some(Some(entry)) = table.get_mut(idx as usize) else {
            return errno::EBADF;
        };
        if entry.is_dir {
            return errno::EISDIR;
        }
        // The position read and the write-permission check both come from
        // `entry.desc`, borrowed immutably first so the mutable borrow of
        // `entry.data` just below is not fighting a live borrow of a sibling
        // field through the same `&mut Entry` — `entry.file()` would hold that
        // borrow for the rest of the function if used here instead.
        let pos = match &entry.desc {
            FileDescriptor::File(f) if f.flags & open_flags::O_ACCMODE != 0 => f.position,
            FileDescriptor::File(_) => return errno::EBADF, // opened read-only
            _ => return errno::EBADF,
        };
        let end = pos + incoming.len();
        if entry.data.len() < end {
            entry.data.resize(end, 0);
        }
        entry.data[pos..end].copy_from_slice(&incoming);
        if let FileDescriptor::File(f) = &mut entry.desc {
            f.position = end;
        }
        drop(table);
        written += incoming.len();
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
                None => crate::sched::yield_now(),
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
    let idx = fd - FIRST_FILE_FD as u64;
    let mut table = TABLE.lock();
    let Some(Some(entry)) = table.get_mut(idx as usize) else {
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
/// Three separate `TABLE` locks rather than one held across the call: the
/// cache-miss path calls `fs::read_dir`, which takes the *other* lock
/// (`fs::ROOT`), and nothing else in this module nests the two — see
/// [`sys_openat`], which reads the file before ever touching `TABLE`.
pub fn sys_getdents64(fd: u64, dirp: u64, count: u64) -> u64 {
    if count == 0 {
        return 0;
    }
    let Some(idx) = fd.checked_sub(FIRST_FILE_FD as u64) else {
        return errno::ENOTDIR;
    };

    let (path, cached) = {
        let mut table = TABLE.lock();
        let Some(Some(entry)) = table.get_mut(idx as usize) else {
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
        let Some(dir_entries) = fs::read_dir(&path) else {
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
        let mut table = TABLE.lock();
        if let Some(Some(entry)) = table.get_mut(idx as usize)
            && let Some(file) = entry.file()
        {
            file.dir_cache = Some(cache.clone());
        }
        cache
    };

    let mut table = TABLE.lock();
    let Some(Some(entry)) = table.get_mut(idx as usize) else {
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
    drop(table);

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
    let (mode, size, nlink) = if fd < FIRST_FILE_FD as u64 {
        (S_IFCHR_0620, 0u64, 1u64)
    } else {
        let idx = fd - FIRST_FILE_FD as u64;
        let table = TABLE.lock();
        let Some(Some(entry)) = table.get(idx as usize) else {
            return errno::EBADF;
        };
        if entry.is_dir {
            (S_IFDIR_0755, 0u64, 2u64)
        } else {
            (S_IFREG_0644, entry.data.len() as u64, 1u64)
        }
    };
    let st = encode_stat(mode, size, 0, nlink, None, None, None);
    if errno::is_err(copy_to_user(statbuf, &st)) {
        return errno::EFAULT;
    }
    0
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

    let Some(meta) = fs::metadata(&normalised) else {
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
fn poll_ready(fd: u64) -> (bool, bool) {    if fd == 0 {
        return match crate::usermode::current_stdin_pipe() {
            Some(p) => (crate::pipe::readable(p), false),
            None => (crate::input::has_byte(), false),
        };
    }
    if fd == 1 || fd == 2 {
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
    let in_table = {
        let idx = fd.wrapping_sub(FIRST_FILE_FD as u64) as usize;
        matches!(TABLE.lock().get(idx), Some(Some(_)))
    };
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
    if fs::metadata(&normalised).is_some() {
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

/// Install a read-only fd whose contents are `data` (a generated file like
/// `/proc/net/dev`). Reads serve from `Entry::data` exactly as a cached real
/// file does.
fn install_synthetic_file(path: &str, data: Vec<u8>, flags: u64) -> u64 {
    let mut table = TABLE.lock();
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(Entry {
                desc: FileDescriptor::File(KernelFile::new(
                    alloc::string::String::from(path),
                    flags as u32,
                )),
                data,
                nonblocking: false,
                is_dir: false,
                owner: crate::usermode::current_proc_slot(),
            });
            return (i + FIRST_FILE_FD) as u64;
        }
    }
    errno::EMFILE
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
            sys_close(rfd);
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

    // Fill the table, then check the next open is refused rather than
    // overwriting a live descriptor.
    let mut opened = Vec::new();
    loop {
        let fd = sys_openat(0, path.as_ptr() as u64, 0, 0);
        if fd >= FIRST_FILE_FD as u64 && fd < (FIRST_FILE_FD + MAX_OPEN) as u64 {
            opened.push(fd);
        } else {
            t.check_eq("fd: a full table is EMFILE", fd, errno::EMFILE);
            break;
        }
        if opened.len() > MAX_OPEN {
            t.check("fd: the table has a bound", false);
            break;
        }
    }
    t.check_eq("fd: the table held exactly MAX_OPEN files", opened.len() as u64, MAX_OPEN as u64);
    for fd in opened {
        sys_close(fd);
    }

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
