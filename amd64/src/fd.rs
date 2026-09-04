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
    pub const ENOSYS: u64 = (-38i64) as u64;
    pub const ESRCH: u64 = (-3i64) as u64;
    pub const EAGAIN: u64 = (-11i64) as u64;
    pub const ENOMEM: u64 = (-12i64) as u64;
}

/// Descriptors 0, 1 and 2 are the console and are never in the table.
pub const FIRST_FILE_FD: usize = 3;
/// How many files may be open at once.
///
/// A fixed array so the table allocates nothing; the *contents* are heap, but
/// the bookkeeping is not. 16 is more than a shell opens and small enough that
/// a leak shows up as `EMFILE` quickly rather than as steady heap growth.
pub const MAX_OPEN: usize = 16;

/// One entry: the tree's descriptor, plus this target's cached contents.
///
/// The cursor lives in the `KernelFile`'s own `position`, not beside it — so a
/// future move to reading by inode changes where the *bytes* come from and
/// nothing else.
struct Entry {
    desc: FileDescriptor,
    /// The file's contents, cached at `open`. See the module header.
    data: Vec<u8>,
    /// `O_NONBLOCK`, set through `fcntl(F_SETFL)`. Only sockets consult it —
    /// `sshd`'s cooperative loop makes its listener and every accepted stream
    /// non-blocking so a session idling on its socket suspends instead of
    /// stalling its peers.
    nonblocking: bool,
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
            *slot = Some(Entry { desc, data: Vec::new(), nonblocking: false });
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
            Some(n) => return copy_to_user(buf, &tmp[..n]) as u64,
            None if nonblock => return errno::EAGAIN,
            None => crate::sched::yield_now(),
        }
    }
}

/// Write to a pipe, honouring `nonblock`. A short write is returned as-is;
/// `sshd`'s bridge carries the residue.
pub fn write_pipe(pipe_id: usize, buf: u64, len: usize, nonblock: bool) -> u64 {
    let data = copy_in(buf, len as u64);
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
#[must_use]
pub fn copy_in(ptr: u64, len: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        // SAFETY: the same user-pointer contract as every other access here.
        out.push(unsafe { (ptr as *const u8).add(i as usize).read_volatile() });
    }
    out
}

/// Copy out to a user pointer. Public for `sock`.
pub fn copy_out(ptr: u64, src: &[u8]) -> usize {
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

/// Copy `src` to a user pointer. Returns how many bytes were written.
fn copy_to_user(ptr: u64, src: &[u8]) -> usize {
    for (i, &b) in src.iter().enumerate() {
        // SAFETY: as `copy_from_user`.
        unsafe { (ptr as *mut u8).add(i).write_volatile(b) };
    }
    src.len()
}

/// Read a NUL-terminated path from user memory.
///
/// Bounded at 256 bytes, which is `PATH_MAX` for every path this kernel can
/// resolve; a longer one is a rejection rather than a truncation, because a
/// truncated path names a *different file* and opening it silently would be
/// worse than failing.
fn path_from_user(ptr: u64) -> Option<alloc::string::String> {
    if ptr == 0 {
        return None;
    }
    let mut buf = Vec::new();
    for i in 0..256u64 {
        // SAFETY: as `copy_from_user`.
        let b = unsafe { (ptr as *const u8).add(i as usize).read_volatile() };
        if b == 0 {
            return alloc::string::String::from_utf8(buf).ok();
        }
        buf.push(b);
    }
    None
}

/// `openat(dirfd, path, flags, mode)`.
///
/// `dirfd` is ignored and every path is resolved from the root. That is correct
/// for absolute paths and wrong for relative ones, which this kernel has no
/// working directory to resolve against yet — so a relative path is treated as
/// root-relative rather than rejected, which is what a shell listing `bin`
/// expects to work.
pub fn sys_openat(_dirfd: u64, path: u64, flags_: u64, _mode: u64) -> u64 {
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
        return errno::ENOENT;
    }

    let normalised = if path.starts_with('/') {
        path
    } else {
        let mut p = alloc::string::String::from("/");
        p.push_str(&path);
        p
    };
    let Some(data) = fs::read_file(&normalised) else {
        return errno::ENOENT;
    };

    let mut table = TABLE.lock();
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            // `KernelFile::new` leaves the inode 0 — "read by path", which is
            // what this target does.
            let desc = FileDescriptor::File(KernelFile::new(normalised, flags_ as u32));
            *slot = Some(Entry { desc, data, nonblocking: false });
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
        _ => {}
    }
    0
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
    if len > MAX_IO {
        return errno::EINVAL;
    }

    if fd == 0 {
        // A spawned child's stdin is a pipe, not the console — `sshd` feeds it.
        if let Some(pid) = crate::usermode::current_stdin_pipe() {
            return read_pipe(pid, buf, len as usize, false);
        }
        return read_console(buf, len as usize) as u64;
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
    copy_to_user(buf, chunk) as u64
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
                Some(_) => return copy_to_user(buf, &one) as u64,
                None => crate::sched::yield_now(),
            }
        }
    }
    loop {
        if let Some(b) = serial::getb() {
            return copy_to_user(buf, &[b]) as u64;
        }
        core::hint::spin_loop();
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
fn read_console(buf: u64, len: usize) -> usize {
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

        let Some(byte) = serial::getb() else {
            core::hint::spin_loop();
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

/// `fstat(fd, statbuf)` — enough of `struct stat` for a caller to learn a size.
///
/// The x86_64 layout: `st_size` is at offset 48 and `st_mode` at 24. The rest is
/// zeroed rather than invented; a caller that reads `st_dev` gets 0, which is
/// wrong but is not a plausible-looking lie.
pub fn sys_fstat(fd: u64, statbuf: u64) -> u64 {
    const STAT_SIZE: usize = 144;
    const ST_MODE: usize = 24;
    const ST_SIZE: usize = 48;
    /// `S_IFREG | 0644`.
    const REG_MODE: u32 = 0o100_644;
    /// `S_IFCHR | 0620`, for the console.
    const CHR_MODE: u32 = 0o020_620;

    let mut st = [0u8; STAT_SIZE];
    let (mode, size) = if fd < FIRST_FILE_FD as u64 {
        (CHR_MODE, 0u64)
    } else {
        let idx = fd - FIRST_FILE_FD as u64;
        let table = TABLE.lock();
        let Some(Some(entry)) = table.get(idx as usize) else {
            return errno::EBADF;
        };
        (REG_MODE, entry.data.len() as u64)
    };
    st[ST_MODE..ST_MODE + 4].copy_from_slice(&mode.to_le_bytes());
    st[ST_SIZE..ST_SIZE + 8].copy_from_slice(&size.to_le_bytes());
    copy_to_user(statbuf, &st);
    0
}

/// `ioctl` — always `ENOTTY`.
///
/// Deliberately not `ENOSYS`. A libc asking "is this a terminal?" treats
/// `ENOTTY` as a clear no and carries on unbuffered; `ENOSYS` reads as "this
/// kernel is broken" and some runtimes abort on it.
pub fn sys_ioctl(_fd: u64, _req: u64, _arg: u64) -> u64 {
    errno::ENOTTY
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

    let mut st = [0u8; 144];
    t.check_eq("fd: fstat succeeds", sys_fstat(fd, st.as_mut_ptr() as u64), 0);
    t.check_eq(
        "fd: fstat reports the size",
        u64::from_le_bytes(st[48..56].try_into().unwrap_or([0; 8])),
        6623,
    );

    t.check_eq("fd: close", sys_close(fd), 0);
    t.check_eq("fd: closing twice is EBADF", sys_close(fd), errno::EBADF);
    t.check_eq("fd: reading a closed fd is EBADF", sys_read(fd, buf.as_mut_ptr() as u64, 4), errno::EBADF);

    // A missing file, and a path that is not a path.
    let missing = b"/does-not-exist\0";
    t.check_eq("fd: opening a missing file is ENOENT",
               sys_openat(0, missing.as_ptr() as u64, 0, 0), errno::ENOENT);
    t.check_eq("fd: a null path is EFAULT", sys_openat(0, 0, 0, 0), errno::EFAULT);

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

    // An oversized read must be refused rather than trusted.
    let fd = sys_openat(0, path.as_ptr() as u64, 0, 0);
    t.check_eq("fd: an oversized read is EINVAL",
               sys_read(fd, buf.as_mut_ptr() as u64, MAX_IO + 1), errno::EINVAL);
    sys_close(fd);
}
