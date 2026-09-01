//! Pure data types for the process subsystem.
//!
//! These types have no architecture-specific or runtime dependencies
//! and can be compiled and tested on the host.

#![allow(dead_code)]

use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Default environment variables for new processes when none are provided.
/// Environment handed to a process spawned with no explicit environment (herd's
/// services, `box run`).
///
/// `PATH` is the full Linux search order, not the `/usr/bin:/bin` pair it used to
/// be. An OCI image's own `Env` is not propagated through the SPAWN abi
/// (`SpawnOptions` has no env field), so a container's shell gets exactly this
/// list — and every official image installs its program under `/usr/local/bin`.
/// `redis:alpine`'s `docker-entrypoint.sh` ends in `exec "$@"` with `$1 =
/// redis-server`, which failed with "redis-server: not found" against the short
/// PATH even though `/usr/local/bin/redis-server` was right there in the overlay.
///
/// Order matches Docker's and a Linux login shell's: local before system, sbin
/// before bin at each level, so an image that ships its own build of a tool wins
/// over the distro's — which is the point of `/usr/local`.
pub const DEFAULT_ENV: &[&str] = &[
    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "HOME=/",
    "TERM=xterm",
];

/// A future that yields once then completes
pub struct YieldOnce(bool);

impl YieldOnce {
    #[must_use]
    pub fn new() -> Self {
        Self(false)
    }
}

impl Default for YieldOnce {
    fn default() -> Self {
        Self::new()
    }
}

impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            Poll::Pending
        }
    }
}

/// Fixed address for process info page (read-only from userspace)
pub const PROCESS_INFO_ADDR: usize = 0x1000;

/// Process info structure shared between kernel and userspace
///
/// The kernel writes it via [`ProcessInfo::new`]; userspace reads it through
/// the matching struct in `userspace/libakuma/src/lib.rs`. Only `pid`/`ppid`/
/// `box_id` are meaningful — argv and cwd are not communicated through this
/// page (argv comes from the entry stack, cwd from the `GETCWD` syscall).
#[repr(C)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub box_id: u64,
    /// Layout padding to the 1024-byte size the userspace side expects (asserted
    /// by `process_info_size`). Underscore-prefixed per the tree's convention for
    /// "do not read this"; clippy reads `pub _name` as "public but unused", which
    /// is the opposite of why it is `pub` — the struct is `repr(C)` and shared
    /// with userspace at `PROCESS_INFO_ADDR`, so the field must exist by name.
    #[allow(clippy::pub_underscore_fields)]
    pub _reserved: [u8; 1008],
}

impl ProcessInfo {
    #[must_use]
    pub const fn new(pid: u32, ppid: u32, box_id: u64) -> Self {
        Self { pid, ppid, box_id, _reserved: [0u8; 1008] }
    }
}

const _: () = assert!(core::mem::size_of::<ProcessInfo>() == 1024);

/// Process id.
///
/// Hoisted to `akuma-primitives` on 2026-08-30 so `akuma-isolation`'s box
/// registry could name it without depending on this crate
/// (`docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §3.1). Re-exported here — it is a
/// plain alias, so this was always the same type, and the ~1,300
/// `akuma_exec::process::Pid` call sites are unchanged.
pub use akuma_primitives::Pid;

/// Stdio buffer for procfs visibility
pub struct StdioBuffer {
    pub data: Vec<u8>,
    pub pos: usize,
}

impl StdioBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self { data: Vec::new(), pos: 0 }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn clear(&mut self) {
        self.data.clear();
        self.pos = 0;
    }

    pub fn write_with_limit(&mut self, data: &[u8], max_size: usize) {
        if self.data.len() + data.len() > max_size {
            self.data.clear();
        }
        self.data.extend_from_slice(data);
    }

    pub fn set_with_limit(&mut self, data: &[u8], max_size: usize) {
        self.data.clear();
        self.pos = 0;
        if data.len() <= max_size {
            self.data.extend_from_slice(data);
        } else {
            self.data.extend_from_slice(&data[data.len() - max_size..]);
        }
    }

    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let remaining = &self.data[self.pos..];
        let to_read = buf.len().min(remaining.len());
        buf[..to_read].copy_from_slice(&remaining[..to_read]);
        self.pos += to_read;
        to_read
    }

    #[must_use]
    pub fn clone_data(&self) -> Vec<u8> {
        self.data.clone()
    }
}

impl Default for StdioBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// File descriptor types for the per-process FD table
#[derive(Debug, Clone)]
pub enum FileDescriptor {
    Stdin,
    Stdout,
    Stderr,
    /// `/dev/tty` — the calling process's controlling terminal.
    ///
    /// Pagers (`less`, `more`) and other full-screen programs read keyboard
    /// input from `/dev/tty`, NEVER from stdin, because stdin is the pipe
    /// carrying the content being paged. Without this node, `git log | less`
    /// opened `/dev/tty`, failed, and fell back to reading stdin — the pipe —
    /// for keystrokes: it never saw a key, hung forever, and the bytes typed
    /// while it hung drained into whatever consumed the pipe.
    ///
    /// Backed by the same `ProcessChannel`/`TerminalState` as fd 0/1/2 (resolved
    /// per-syscall via `current_channel()`/`current_terminal_state()`, so a
    /// `box grab`/reattach repoint is honoured), which is why it carries no
    /// payload: the identity IS "this process's console".
    DevTty,
    Socket(usize),
    File(KernelFile),
    ChildStdout(Pid),
    PipeRead(u32),
    PipeWrite(u32),
    /// AF_UNIX socket endpoint, backed by two unidirectional kernel pipes.
    /// `rx` is the pipe this endpoint reads from; `tx` is the pipe it writes to.
    /// The peer endpoint has rx/tx swapped.
    ///
    /// `sock` indexes the entry in `akuma_net_unix::UnixTable` carrying
    /// everything a pipe pair cannot express: the bound name, the socket type,
    /// the peer's identity and credentials, shutdown state, a listener's
    /// backlog, and the record boundaries that make `SOCK_SEQPACKET` and
    /// `SOCK_DGRAM` preserve messages.
    ///
    /// **`sock == 0` means "no table entry"**, and every table operation
    /// no-ops for it. That is not a placeholder to be cleaned up later — it is
    /// what keeps two callers working unchanged: `src/rump_proxy.rs` installs a
    /// kernel-internal pipe pair at box 0's fd 3 for the sysproxy channel, and
    /// it must stay byte-for-byte identical (a regression there stops the rump
    /// stack from coming up, several layers away from any socket code). A
    /// descriptor with `sock == 0` behaves exactly as the pre-table
    /// implementation did: `read`/`write`/`send`/`recv` on the raw pipes, no
    /// framing, no name.
    UnixSocket { rx: u32, tx: u32, sock: u32 },
    EventFd(u32),
    DevNull,
    DevUrandom,
    /// `/dev/zero`: reads fill the buffer with zero bytes (returning the full
    /// count), writes are discarded. Mirrors `/dev/null` except read semantics.
    /// Needed by libc/rump anonymous-memory and buffer-zeroing paths.
    DevZero,
    /// virtio-sound output device (`/dev/dsp`). Writes stream PCM frames to the
    /// kernel audio driver; ioctl sets OSS format/channels/rate.
    DevDsp,
    /// Raw L2 packet device (`/dev/net/tap0`) for the kernel `rump` feature.
    /// `read`/`write` move whole Ethernet frames to/from a dedicated second
    /// virtio-net NIC (bypassing smoltcp). Only ever constructed when the
    /// kernel is built with the `rump` feature and NIC1 is present; the variant
    /// is unconditional so non-rump builds still match exhaustively.
    /// `nonblock`: when false (the default for `open` without `O_NONBLOCK`), a
    /// `read` with no frame ready blocks (cooperatively yields) until one arrives;
    /// when true it returns `EAGAIN` — POSIX device-read semantics, so the rump
    /// virtif RX thread can do a plain blocking `read()` instead of busy-polling.
    Tap { nonblock: bool },
    /// A socket living in a `stack=rump` box's NetBSD `rump_server`. The box
    /// process sees a normal low-numbered fd; the kernel proxy forwards this
    /// fd's socket syscalls over the box's sysproxy channel, translating the box
    /// fd ⇄ the server's `rump_fd`. `nonblock` mirrors the requested socket type
    /// bit (the proxy keeps the rump socket blocking and emulates nonblock).
    /// Unconditional (like `Tap`) so non-rump builds still match exhaustively.
    /// A socket living in a `stack=rump` box's `rump_server`, addressed by the
    /// server-side fd number.
    ///
    /// `box_id` is carried on the descriptor because the rump fd number alone does
    /// not identify a socket: each box has its own `rump_server`, and two servers
    /// hand out the same small integers. Anything that keys per-socket state (the
    /// cross-fork reference count in `rump_proxy`) therefore needs the pair, and
    /// `SharedFdTable::clone_deep_for_fork` — which sees descriptors, not the
    /// process they belong to — has no other way to learn which server this fd is
    /// on. See `docs/archive/RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md`.
    RumpSocket { rump_fd: i32, nonblock: bool, box_id: u64 },
    TimerFd(u32),
    EpollFd(u32),
    PidFd(u32),
    /// Raw fd onto a virtio block device (`/dev/vdX`), addressed by
    /// `akuma_virtio::block` index. `pos` is the byte offset `read`/`write`
    /// advance (`dd` is sequential; `lseek` also updates it). `writable` is
    /// fixed at open time: an `O_RDONLY` fd cannot be upgraded by anything
    /// after the open-time mounted-device check
    /// (`proposals/RAW_BLOCK_DEVICE_FD.md` §3 — a raw write to a *mounted*
    /// device bypasses `Ext2Filesystem`'s cache, so write-open of a mounted
    /// device is refused with `EBUSY` and only ever an unmounted device like
    /// the `KERNEL_DROPOFF` drive gets a writable fd).
    BlockDev { idx: u32, pos: u64, writable: bool },
}

/// Cached directory entry for stable getdents64 enumeration.
#[derive(Debug, Clone)]
pub struct DirCacheEntry {
    pub name: String,
    pub d_type: u8,
}

/// Kernel file handle for open files
#[derive(Debug, Clone)]
pub struct KernelFile {
    pub path: String,
    pub position: usize,
    pub flags: u32,
    /// Snapshot of directory entries taken on the first getdents64 call.
    /// Prevents position drift when entries are deleted between calls.
    pub dir_cache: Option<Vec<DirCacheEntry>>,
    /// The inode `open(2)` resolved this path to, or `0` for "no inode: read by
    /// path". See [`KernelFile::inode`].
    inode: u32,
    /// Which mount `inode` belongs to (`akuma_vfs::ResolvedMount::id`), captured
    /// with it at `open(2)`.
    ///
    /// An inode number is only unique within one filesystem, so this is the other
    /// half of the fd's file identity — and it is what lets `read(2)` find the
    /// filesystem **without touching the path at all**: no `resolve_path`, no
    /// mount-relative rewrite, and no chance of the path resolving somewhere else
    /// than it did at `open(2)`.
    mount_id: u32,
    /// Keeps `inode`'s data alive for as long as this descriptor exists.
    ///
    /// Nothing reads this field; like [`LazySource::File`]'s, it is load-bearing
    /// purely through `Clone`/`Drop`, which is what makes `dup`, `fork`'s
    /// `clone_deep_for_fork`, `close`, `close_all` and `exec`'s table clear
    /// balanced without any of them knowing the pin exists. Always constructed
    /// together with `inode` in [`KernelFile::with_inode`] so the two cannot
    /// drift apart.
    pin: akuma_primitives::InodePin,
}

impl KernelFile {
    #[must_use]
    pub fn new(path: String, flags: u32) -> Self {
        Self {
            path,
            position: 0,
            flags,
            dir_cache: None,
            inode: 0,
            mount_id: 0,
            pin: akuma_primitives::InodePin::none(),
        }
    }

    /// Bind this descriptor to the `(mount, inode)` pair `open(2)` resolved.
    ///
    /// `read(2)` then reads by inode number instead of re-walking the directory
    /// tree on every call — the whole point of resolving here — and the pin
    /// taken alongside it gives the fd Linux's "unlinked but still open"
    /// semantics, which is what makes reading by a number that the filesystem
    /// could otherwise reissue safe. See `src/vfs::open_file_ids` for which opens
    /// get one and why the rest keep reading by path.
    ///
    /// The two are set together and never separately: an inode number applied to
    /// the wrong filesystem is the aliasing this pair exists to prevent.
    #[must_use]
    pub fn with_inode(mut self, mount_id: u32, inode: u32) -> Self {
        self.inode = inode;
        self.mount_id = mount_id;
        self.pin = akuma_primitives::InodePin::new(inode);
        self
    }

    /// The inode this fd was opened on, or `0` when it must read by path.
    #[must_use]
    pub const fn inode(&self) -> u32 {
        self.inode
    }

    /// The mount this fd was opened on, or `0` when it must resolve by path.
    #[must_use]
    pub const fn mount_id(&self) -> u32 {
        self.mount_id
    }
}

/// File open flags (Linux compatible).
///
/// The definitions moved to `akuma_syscalls_linux::flags::open` on 2026-08-27.
/// They were declared here, in the process/exec crate, which put the *file
/// open* ABI in a crate that has nothing to do with it — and left the two bits
/// this module never had (`O_NONBLOCK`, `O_DIRECTORY`) to be redeclared
/// locally in `src/syscall/fs.rs` and `src/syscall/pidfd.rs`, at two different
/// widths. This alias keeps every `open_flags::O_*` call site unchanged.
pub use akuma_syscalls_linux::flags::open as open_flags;

/// Source of data for a lazy region page.
#[derive(Clone)]
pub enum LazySource {
    Zero,
    File {
        path: String,
        inode: u32,
        /// Which mount `inode` belongs to (`akuma_vfs::ResolvedMount::id`),
        /// captured with it at mmap time. `0` means "no identity", which
        /// disables page-cache sharing for this region.
        ///
        /// An inode number alone does not name a file: a second `mount(2)` puts
        /// another filesystem's numbers in the same range, and the page cache is
        /// global. Carrying the pair is what stops a mapping of inode 12 on one
        /// mount being served the cached page of inode 12 on another — finding
        /// F-1 of `docs/archive/EXT2_WRITEBACK_DESIGN.md`.
        mount_id: u32,
        file_offset: usize,
        filesz: usize,
        segment_va: usize,
        /// Keeps `inode`'s data alive for as long as this region exists.
        ///
        /// `inode` is a raw number with no lifetime tie to the file, so without
        /// this the filesystem was free to truncate it and reissue the number
        /// under a live mapping — the fill then read `Ok(0)` (zero page) or, after
        /// reuse, another file's bytes. That is root cause #2 of the self-host
        /// `rustc` ICE (`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §14).
        ///
        /// Nothing reads this field. It is load-bearing purely through
        /// [`InodePin`]'s `Clone`/`Drop`, which is what makes every region path —
        /// fork's propagate/clone, `mprotect`'s split, `munmap`'s four clip
        /// shapes, `exec`'s clear, `Process::drop` — balanced without any of them
        /// knowing the pin exists. Construct with [`LazySource::file`] rather
        /// than by hand so the pin can never be forgotten.
        pin: akuma_primitives::InodePin,
    },
}

impl LazySource {
    /// The only way to build a [`LazySource::File`]: takes the pin on `inode`
    /// as part of construction, so a caller cannot create an unpinned mapping.
    #[must_use]
    pub fn file(
        path: String,
        mount_id: u32,
        inode: u32,
        file_offset: usize,
        filesz: usize,
        segment_va: usize,
    ) -> Self {
        Self::File {
            path,
            inode,
            mount_id,
            file_offset,
            filesz,
            segment_va,
            pin: akuma_primitives::InodePin::new(inode),
        }
    }
}

/// A lazily-backed virtual memory region.
#[derive(Clone)]
pub struct LazyRegion {
    pub start_va: usize,
    pub size: usize,
    pub flags: u64,
    pub source: LazySource,
}

/// An eagerly-mapped `mmap` region, re-exported from `akuma-mmap`.
///
/// The type and its two pure operations — `inherit_mmap_regions_for_cow_child` and
/// `detach_eager_regions_in_range` — moved there so region algebra could be host
/// tested without this crate, and so a future `akuma-syscalls-mem` can name a region
/// without depending on all of `akuma-exec`. What stayed here is everything that
/// needs a process or a lock: `Process::mmap_regions` itself, the `vm_lock` /
/// `vm_with_regions` discipline that guards it, and every pid-keyed accessor in
/// `process::children`.
pub use akuma_mmap::MmapRegion;

/// Process state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Ready,
    Running,
    Blocked,
    Zombie(i32),
}

/// User context saved during kernel entry
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UserContext {
    pub x0: u64, pub x1: u64, pub x2: u64, pub x3: u64,
    pub x4: u64, pub x5: u64, pub x6: u64, pub x7: u64,
    pub x8: u64, pub x9: u64, pub x10: u64, pub x11: u64,
    pub x12: u64, pub x13: u64, pub x14: u64, pub x15: u64,
    pub x16: u64, pub x17: u64, pub x18: u64, pub x19: u64,
    pub x20: u64, pub x21: u64, pub x22: u64, pub x23: u64,
    pub x24: u64, pub x25: u64, pub x26: u64, pub x27: u64,
    pub x28: u64, pub x29: u64, pub x30: u64,
    pub sp: u64,
    pub pc: u64,
    pub spsr: u64,
    pub tpidr: u64,
    pub ttbr0: u64,
}

impl UserContext {
    #[must_use]
    pub fn new(entry_point: usize, stack_pointer: usize) -> Self {
        Self {
            x0: 0, x1: 0, x2: 0, x3: 0, x4: 0, x5: 0, x6: 0, x7: 0,
            x8: 0, x9: 0, x10: 0, x11: 0, x12: 0, x13: 0, x14: 0, x15: 0,
            x16: 0, x17: 0, x18: 0, x19: 0, x20: 0, x21: 0, x22: 0, x23: 0,
            x24: 0, x25: 0, x26: 0, x27: 0, x28: 0, x29: 0, x30: 0,
            sp: stack_pointer as u64,
            pc: entry_point as u64,
            spsr: 0,
            tpidr: 0,
            ttbr0: 0,
        }
    }

}

/// The real trait, not an inherent `default()`.
///
/// It was inherent while this type was crate-private, where the shadowing was
/// invisible; as a public API an inherent `default` that is not `Default` reads
/// as the trait and is not one. The single call site resolves through the trait
/// unchanged.
impl Default for UserContext {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

pub const MAX_SIGNALS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalHandler {
    Default,
    Ignore,
    UserFn(usize),
}

#[derive(Debug, Clone, Copy)]
pub struct SignalAction {
    pub handler: SignalHandler,
    pub flags: u64,
    pub mask: u64,
    pub restorer: usize,
}

impl SignalAction {
    #[must_use]
    pub const fn default() -> Self {
        Self {
            handler: SignalHandler::Default,
            flags: 0,
            mask: 0,
            restorer: 0,
        }
    }

    /// Should an expired `ITIMER_REAL` (`alarm()`/`setitimer()`) force-interrupt
    /// a blocking syscall via the unconditional, `SA_RESTART`-blind Ctrl-C-style
    /// flag (`ProcessChannel::interrupted`), rather than relying solely on
    /// ordinary signal delivery?
    ///
    /// - `Default`: yes — this is the only mechanism that can break a
    ///   handler-less `alarm(); pause();`; the mask-and-handler-gated
    ///   `current_thread_has_pending_interrupt` never fires for `SIG_DFL`.
    /// - `Ignore`: no — Linux delivers nothing observable for an ignored
    ///   signal, so nothing should interrupt either.
    /// - `UserFn`: only when the handler did **not** ask for `SA_RESTART`. The
    ///   signal is still queued via `pend_signal_for_thread` regardless, so an
    ///   `SA_RESTART` handler is delivered normally at the next syscall
    ///   return — it just doesn't get this extra, restart-ignorant kick. A
    ///   handler *with* `SA_RESTART` (e.g. a periodic heartbeat/low-speed-limit
    ///   timer that expects its own blocking syscalls to keep running after
    ///   each tick) previously got force-interrupted on every single tick
    ///   regardless — docs/archive/GIT_CLONE_STALE_ITIMER_SIGALRM.md.
    #[must_use]
    pub fn wants_itimer_force_interrupt(&self) -> bool {
        const SA_RESTART: u64 = 0x1000_0000;
        match self.handler {
            SignalHandler::UserFn(_) => self.flags & SA_RESTART == 0,
            SignalHandler::Default => true,
            SignalHandler::Ignore => false,
        }
    }
}

#[cfg(test)]
mod signal_action_tests {
    use super::*;

    #[test]
    fn default_disposition_wants_force_interrupt() {
        assert!(SignalAction::default().wants_itimer_force_interrupt());
    }

    #[test]
    fn ignore_disposition_never_wants_force_interrupt() {
        let action = SignalAction { handler: SignalHandler::Ignore, ..SignalAction::default() };
        assert!(!action.wants_itimer_force_interrupt());
    }

    #[test]
    fn handler_without_sa_restart_wants_force_interrupt() {
        let action = SignalAction {
            handler: SignalHandler::UserFn(0x1234),
            flags: 0,
            ..SignalAction::default()
        };
        assert!(action.wants_itimer_force_interrupt());
    }

    /// The regression: a periodic `SA_RESTART` handler (e.g. curl's
    /// low-speed-limit heartbeat) must NOT be force-interrupted — that bypasses
    /// its own restart request and was what made `git clone`'s checkout phase
    /// die mid-write with a bogus "signal 2" exit code under real network
    /// latency (docs/archive/GIT_CLONE_STALE_ITIMER_SIGALRM.md).
    #[test]
    fn handler_with_sa_restart_does_not_want_force_interrupt() {
        const SA_RESTART: u64 = 0x1000_0000;
        let action = SignalAction {
            handler: SignalHandler::UserFn(0x1234),
            flags: SA_RESTART,
            ..SignalAction::default()
        };
        assert!(!action.wants_itimer_force_interrupt());
    }

    #[test]
    fn sa_restart_bit_ignored_for_non_userfn_handlers() {
        const SA_RESTART: u64 = 0x1000_0000;
        // SA_RESTART set but disposition is Default/Ignore — the flag is only
        // meaningful for a real handler; it must not change the outcome.
        let default_with_flag = SignalAction { flags: SA_RESTART, ..SignalAction::default() };
        assert!(default_with_flag.wants_itimer_force_interrupt());

        let ignore_with_flag = SignalAction {
            handler: SignalHandler::Ignore,
            flags: SA_RESTART,
            ..SignalAction::default()
        };
        assert!(!ignore_with_flag.wants_itimer_force_interrupt());
    }
}

/// Process info for display (used by ps command)
#[derive(Debug, Clone)]
pub struct ProcessInfo2 {
    pub pid: Pid,
    pub ppid: Pid,
    pub box_id: u64,
    pub name: String,
    pub state: &'static str,
    pub current_syscall: u64,
    pub last_syscall: u64,
    pub args: Vec<String>,
}

// SharedSignalTable moved to signal.rs (as it had internal dependencies)
#[cfg(test)]
mod tests {
    use super::*;
    use core::task::Waker;

    #[test]
    fn process_info_size() {
        assert_eq!(core::mem::size_of::<ProcessInfo>(), 1024);
    }

    #[test]
    fn process_info_new() {
        let info = ProcessInfo::new(42, 1, 7);
        assert_eq!(info.pid, 42);
        assert_eq!(info.ppid, 1);
        assert_eq!(info.box_id, 7);
    }

    #[test]
    fn stdio_buffer_write_and_read() {
        let mut buf = StdioBuffer::new();
        assert!(buf.is_empty());
        buf.write_with_limit(b"hello", 1024);
        assert_eq!(buf.len(), 5);
        let mut out = [0u8; 3];
        let n = buf.read(&mut out);
        assert_eq!(n, 3);
        assert_eq!(&out, b"hel");
        let n = buf.read(&mut out);
        assert_eq!(n, 2);
        assert_eq!(&out[..2], b"lo");
    }

    #[test]
    fn stdio_buffer_write_over_limit_clears() {
        let mut buf = StdioBuffer::new();
        buf.write_with_limit(b"hello", 8);
        buf.write_with_limit(b"world!", 8);
        assert_eq!(buf.len(), 6);
    }

    #[test]
    fn stdio_buffer_set_with_limit_truncates() {
        let mut buf = StdioBuffer::new();
        buf.set_with_limit(b"abcdefghij", 5);
        assert_eq!(buf.len(), 5);
        assert_eq!(buf.clone_data(), b"fghij");
    }

    #[test]
    fn stdio_buffer_clear() {
        let mut buf = StdioBuffer::new();
        buf.write_with_limit(b"data", 1024);
        buf.clear();
        assert!(buf.is_empty());
    }

    #[test]
    fn kernel_file_new() {
        let f = KernelFile::new(String::from("/etc/test"), 0o100);
        assert_eq!(f.path, "/etc/test");
        assert_eq!(f.position, 0);
        assert_eq!(f.flags, 0o100);
    }

    #[test]
    fn user_context_new() {
        let ctx = UserContext::new(0x1000, 0x2000);
        assert_eq!(ctx.pc, 0x1000);
        assert_eq!(ctx.sp, 0x2000);
        assert_eq!(ctx.x0, 0);
    }

    #[test]
    fn signal_action_default() {
        let sa = SignalAction::default();
        assert_eq!(sa.handler, SignalHandler::Default);
        assert_eq!(sa.flags, 0);
    }

    fn noop_waker() -> Waker {
        Waker::noop().clone()
    }

    /// Moved here with `YieldOnce` on 2026-09-01 — it had stayed behind in
    /// `akuma-exec`, where it still compiled (the type is re-exported) but was
    /// testing this crate's type from the wrong side of the boundary.
    #[test]
    fn yield_once_future() {
        let waker = noop_waker();
        let mut cx = core::task::Context::from_waker(&waker);
        let mut y = YieldOnce::new();
        let pinned = core::pin::Pin::new(&mut y);
        assert_eq!(pinned.poll(&mut cx), core::task::Poll::Pending);
        let pinned = core::pin::Pin::new(&mut y);
        assert_eq!(pinned.poll(&mut cx), core::task::Poll::Ready(()));
    }
}
