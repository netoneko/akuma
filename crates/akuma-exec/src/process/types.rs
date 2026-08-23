//! Pure data types for the process subsystem.
//!
//! These types have no architecture-specific or runtime dependencies
//! and can be compiled and tested on the host.

#![allow(dead_code)]

use crate::runtime::PhysFrame;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
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
    pub fn new() -> Self {
        YieldOnce(false)
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
    pub _reserved: [u8; 1008],
}

impl ProcessInfo {
    pub const fn new(pid: u32, ppid: u32, box_id: u64) -> Self {
        Self { pid, ppid, box_id, _reserved: [0u8; 1008] }
    }
}

const _: () = assert!(core::mem::size_of::<ProcessInfo>() == 1024);

/// Process ID type
pub type Pid = u32;

/// Stdio buffer for procfs visibility
pub struct StdioBuffer {
    pub data: Vec<u8>,
    pub pos: usize,
}

impl StdioBuffer {
    pub fn new() -> Self {
        Self { data: Vec::new(), pos: 0 }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

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
    Socket(usize),
    File(KernelFile),
    ChildStdout(Pid),
    PipeRead(u32),
    PipeWrite(u32),
    /// AF_UNIX socket endpoint, backed by two unidirectional kernel pipes.
    /// `rx` is the pipe this endpoint reads from; `tx` is the pipe it writes to.
    /// The peer endpoint has rx/tx swapped.
    ///
    /// `sock` indexes the entry in `akuma_net::unix::UnixTable` carrying
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
}

impl KernelFile {
    pub fn new(path: String, flags: u32) -> Self {
        Self { path, position: 0, flags, dir_cache: None }
    }
}

/// File open flags (Linux compatible)
pub mod open_flags {
    pub const O_RDONLY: u32 = 0;
    pub const O_WRONLY: u32 = 1;
    pub const O_RDWR: u32 = 2;
    pub const O_CREAT: u32 = 0o100;
    pub const O_TRUNC: u32 = 0o1000;
    pub const O_APPEND: u32 = 0o2000;
    pub const O_CLOEXEC: u32 = 0o2000000;
    /// `__O_TMPFILE | O_DIRECTORY` in the **arm64** encoding — arm64 keeps the
    /// 32-bit ARM fcntl values (`O_DIRECTORY = 0o40000`), *not* the asm-generic
    /// ones x86/riscv use (`0o200000`); this is what musl, glibc and Go all
    /// pass on this target. The kernel does not implement tmpfiles;
    /// `sys_openat` rejects the flag so portable callers (apk-tools 3's atomic
    /// writes) take their `.tmp` + `renameat` fallback instead of writing into
    /// a directory fd.
    pub const O_TMPFILE: u32 = 0o20040000;
}

/// Source of data for a lazy region page.
#[derive(Clone)]
pub enum LazySource {
    Zero,
    File {
        path: String,
        inode: u32,
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
        inode: u32,
        file_offset: usize,
        filesz: usize,
        segment_va: usize,
    ) -> Self {
        Self::File {
            path,
            inode,
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

/// An eagerly-mapped `mmap` region (all pages resident at mmap time).
///
/// `pages` — not `frames.len()` — is the authoritative extent of the region.
/// The two are equal for a region this process created itself via `mmap`, but a
/// **CoW-forked child inherits `pages` with an empty `frames`**: the child maps
/// every page (read-only, shared with the parent) but owns none of them, so it
/// has no per-region frame list to record. Frame ownership for such a child is
/// tracked solely in `UserAddressSpace::user_frames`, which is refcounted.
///
/// Deriving the extent from `frames.len()` therefore reports 0 pages for any
/// inherited region, which is how a *grandchild* fork used to lose its parent's
/// mmap regions entirely — `cow_share_range` skipped them as zero-length, and
/// the grandchild took an unrecoverable translation fault on first touch (see
/// `docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md`). Use `pages` for extent
/// (sharing, demotion, munmap sizing) and `frames` only when a real PA is
/// required, guarding the index against a short/empty list.
#[derive(Clone)]
pub struct MmapRegion {
    pub start_va: usize,
    pub pages: usize,
    pub frames: Vec<PhysFrame>,
    /// The protection this mapping is *supposed* to have, in `mmu::user_flags`
    /// terms — the eager counterpart of `LazyRegion::flags`.
    ///
    /// Without it an eager region records extent and frames but no permission, so
    /// the EL0 write-permission-fault handler cannot tell a PTE that is wrongly
    /// read-only (page state lost some other way) from a mapping that is
    /// legitimately read-only (`mprotect(PROT_READ)`). Lazy regions carry flags and
    /// therefore get a permission upgrade; eager regions had no such path and died
    /// with SIGSEGV instead. See
    /// `docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md` §3.
    pub flags: u64,

    /// `MAP_SHARED | MAP_ANONYMOUS`: this mapping must survive `fork` as **one
    /// object**, not as a copy-on-write copy.
    ///
    /// Everything else in an address space is private, so fork demotes it to RO and
    /// lets the first write break CoW. Doing that to a `MAP_SHARED` anonymous
    /// mapping silently gives parent and child separate pages — a child's write is
    /// then invisible to the parent, which is the opposite of what the flag asks
    /// for. Regions carrying this take
    /// [`share_rw_range`](crate::process::share_rw_range) at fork instead: same
    /// frames, mapped writable in the child, parent left alone.
    ///
    /// Must propagate to inherited regions too, or a grandchild silently stops
    /// sharing. Probe: `userspace/forktest/c_stress/shmanon.c`.
    pub shared_anon: bool,
}

impl MmapRegion {
    /// Region created by this process: it owns every frame, protection unrecorded.
    ///
    /// Defaults to `NONE` **deliberately**. `flags` exists so the fault handler can
    /// grant a write it would otherwise refuse, so an unknown protection has to be
    /// the one that grants nothing: a wrong `RW` default would silently defeat
    /// `mprotect(PROT_READ)` on any region built through this constructor. `NONE`
    /// leaves such a region behaving exactly as it did before `flags` existed.
    /// Callers that know the real protection use [`MmapRegion::owned_with_flags`].
    pub fn owned(start_va: usize, frames: Vec<PhysFrame>) -> Self {
        Self::owned_with_flags(start_va, frames, crate::mmu::user_flags::NONE)
    }

    /// Region created by this process, with its real protection recorded.
    pub fn owned_with_flags(start_va: usize, frames: Vec<PhysFrame>, flags: u64) -> Self {
        Self { start_va, pages: frames.len(), frames, flags, shared_anon: false }
    }

    /// Region inherited by a CoW-forked child: extent known, no owned frames,
    /// protection unrecorded (`NONE` — see [`MmapRegion::owned`] for why).
    pub fn inherited(start_va: usize, pages: usize) -> Self {
        Self::inherited_with_flags(start_va, pages, crate::mmu::user_flags::NONE)
    }

    /// Region inherited by a CoW-forked child, carrying the parent's protection.
    pub fn inherited_with_flags(start_va: usize, pages: usize, flags: u64) -> Self {
        Self { start_va, pages, frames: Vec::new(), flags, shared_anon: false }
    }

    /// Mark this region `MAP_SHARED | MAP_ANONYMOUS`. See [`MmapRegion::shared_anon`].
    #[must_use]
    pub fn shared_anon(mut self) -> Self {
        self.shared_anon = true;
        self
    }


    pub fn len_bytes(&self) -> usize {
        self.pages * 4096
    }

    pub fn contains(&self, va: usize) -> bool {
        va >= self.start_va && va < self.start_va + self.len_bytes()
    }

    /// Physical frame backing `va`, if this process owns a frame list covering it.
    /// Returns `None` for CoW-inherited regions (no owned frames) and for any VA
    /// outside the owned prefix.
    pub fn frame_for(&self, va: usize) -> Option<PhysFrame> {
        if !self.contains(va) {
            return None;
        }
        self.frames.get((va - self.start_va) / 4096).copied()
    }
}

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

    pub fn default() -> Self {
        Self::new(0, 0)
    }
}

pub const MAX_SIGNALS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq)]
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

/// Memory regions for a process
#[derive(Debug)]
pub struct ProcessMemory {
    pub code_end: usize,
    pub brk: usize,
    pub stack_bottom: usize,
    pub stack_top: usize,
    /// Next available mmap VA. AtomicUsize so CLONE_VM goroutine threads
    /// (which share the parent Process via lookup_process) can race-free
    /// advance it using CAS without disabling IRQs.
    pub next_mmap: AtomicUsize,
    pub mmap_limit: usize,
    pub free_regions: Vec<(usize, usize)>,
}

impl Clone for ProcessMemory {
    fn clone(&self) -> Self {
        Self {
            code_end: self.code_end,
            brk: self.brk,
            stack_bottom: self.stack_bottom,
            stack_top: self.stack_top,
            next_mmap: AtomicUsize::new(self.next_mmap.load(Ordering::Relaxed)),
            mmap_limit: self.mmap_limit,
            free_regions: self.free_regions.clone(),
        }
    }
}

impl ProcessMemory {
    pub fn new(code_end: usize, stack_bottom: usize, stack_top: usize, mmap_floor: usize) -> Self {
        let base = (code_end + 0x1000_0000) & !0xFFFF;
        let mmap_start = core::cmp::max(base, mmap_floor);
        let mmap_limit = stack_bottom.saturating_sub(0x10_0000);

        Self {
            code_end,
            brk: code_end,
            stack_bottom,
            stack_top,
            next_mmap: AtomicUsize::new(mmap_start),
            mmap_limit,
            free_regions: Vec::new(),
        }
    }

    pub fn overlaps_stack(&self, addr: usize, size: usize) -> bool {
        let end = addr.saturating_add(size);
        addr < self.stack_top && end > self.stack_bottom
    }

    pub const KERNEL_VA_START: usize = 0x4000_0000;
    /// Fallback top of the kernel-RAM identity-map VA hole, used only before the
    /// MMU knows the real RAM size (host unit tests).  At runtime the kernel uses
    /// the dynamic `crate::mmu::kernel_va_end()`, which scales with detected RAM —
    /// the 0xC000_0000 value here corresponds to a 2GB-RAM machine.  See
    /// `kernel_va_end()` for why this must track RAM size.
    pub const KERNEL_VA_END: usize   = 0xC000_0000;

    pub fn alloc_mmap(&mut self, size: usize) -> Option<usize> {
        // Dynamic top of the kernel identity-map hole (scales with RAM size).
        let kva_end = crate::mmu::kernel_va_end();
        for i in 0..self.free_regions.len() {
            let (start, f_size) = self.free_regions[i];

            // Skip regions that overlap the kernel RAM identity map.
            if start < kva_end && start + f_size > Self::KERNEL_VA_START {
                continue;
            }

            if f_size >= size {
                self.free_regions.remove(i);
                if f_size > size {
                    self.free_regions.push((start + size, f_size - size));
                }
                return Some(start);
            }
        }

        // CAS loop: race-free advance of next_mmap vs CLONE_VM sibling goroutine threads.
        // All goroutine threads share the parent Process via lookup_process(owner_pid),
        // so next_mmap is genuinely shared. CAS prevents two goroutines from receiving
        // the same VA (goroutine stack aliasing → WILD-IA crash).
        loop {
            let cur = self.next_mmap.load(Ordering::Relaxed);
            let mut candidate = cur;

            // Skip over the kernel RAM identity-map range if the allocation would
            // overlap it. Jump to the dynamic top (kva_end), NOT the 2GB-machine
            // const — otherwise the bump pointer lands at 0xC000_0000 inside the
            // real identity map on >2GB-RAM machines (the rustc MEMORY>2GB crash).
            if candidate < kva_end && candidate + size > Self::KERNEL_VA_START {
                candidate = kva_end;
            }

            if self.overlaps_stack(candidate, size) {
                return None;
            }
            if candidate + size > self.mmap_limit {
                return None;
            }

            if self.next_mmap
                .compare_exchange(cur, candidate + size, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                return Some(candidate);
            }
            // CAS failed: a CLONE_VM sibling updated next_mmap concurrently.
            // Reload and retry — they got a different address, so will we.
        }
    }

    pub fn free_mmap(&mut self, start: usize, size: usize) {
        self.free_regions.push((start, size));
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
    use core::task::{RawWaker, RawWakerVTable, Waker};

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

    #[test]
    fn process_memory_new() {
        let mem = ProcessMemory::new(0x10000, 0x7FFF_0000, 0x8000_0000, 0);
        assert_eq!(mem.code_end, 0x10000);
        assert_eq!(mem.brk, 0x10000);
        assert_eq!(mem.stack_bottom, 0x7FFF_0000);
        assert_eq!(mem.stack_top, 0x8000_0000);
    }

    #[test]
    fn process_memory_overlaps_stack() {
        let mem = ProcessMemory::new(0x10000, 0x7FFF_0000, 0x8000_0000, 0);
        assert!(mem.overlaps_stack(0x7FFF_0000, 0x1000));
        assert!(!mem.overlaps_stack(0x1000, 0x1000));
    }

    #[test]
    fn process_memory_alloc_mmap_sequential() {
        let mut mem = ProcessMemory::new(0x10000, 0x3000_0000, 0x3010_0000, 0);
        let a1 = mem.alloc_mmap(0x1000);
        let a2 = mem.alloc_mmap(0x1000);
        assert!(a1.is_some());
        assert!(a2.is_some());
        assert_ne!(a1, a2);
    }

    #[test]
    fn process_memory_alloc_mmap_skips_kernel_va() {
        let mut mem = ProcessMemory::new(0x3FFF_0000, 0xD000_0000, 0xD010_0000, 0);
        let addr = mem.alloc_mmap(0x1000);
        if let Some(a) = addr {
            assert!(a < ProcessMemory::KERNEL_VA_START || a >= ProcessMemory::KERNEL_VA_END);
        }
    }

    #[test]
    fn process_memory_alloc_mmap_straddle_kernel_va_start() {
        // Regression: allocation starting one page before KERNEL_VA_START with size > 1 page
        // would straddle the boundary and land inside the kernel VA hole.
        let mut mem = ProcessMemory::new(0x1000_0000, 0xD000_0000, 0xD010_0000, 0);
        mem.next_mmap.store(ProcessMemory::KERNEL_VA_START - 0x1000, Ordering::Relaxed);
        let addr = mem.alloc_mmap(2 * 0x1000).unwrap();
        assert!(
            addr >= ProcessMemory::KERNEL_VA_END,
            "alloc straddled kernel VA hole: {:#x}",
            addr
        );
    }

    #[test]
    fn process_memory_free_and_reuse() {
        let mut mem = ProcessMemory::new(0x10000, 0x3000_0000, 0x3010_0000, 0);
        let a1 = mem.alloc_mmap(0x1000).unwrap();
        mem.free_mmap(a1, 0x1000);
        let a2 = mem.alloc_mmap(0x1000).unwrap();
        assert_eq!(a2, a1);
    }

    #[test]
    fn process_memory_alloc_no_duplicate_addresses() {
        // Two sequential alloc_mmap calls must return different addresses.
        // Regression: a race between CLONE_VM goroutine threads reading
        // next_mmap before either write could return the same VA to both.
        let mut mem = ProcessMemory::new(0x10000, 0x3000_0000, 0x3010_0000, 0);
        let a1 = mem.alloc_mmap(0x1000).unwrap();
        let a2 = mem.alloc_mmap(0x1000).unwrap();
        assert_ne!(a1, a2, "alloc_mmap returned same address twice: {:#x}", a1);
    }

    #[test]
    fn process_memory_lazy_munmap_no_recycle() {
        // Verifies that NOT calling free_mmap after a lazy munmap causes the
        // next alloc to advance past the freed range (no recycling loop).
        // Contrast with eager munmap where free_mmap IS called and reuse occurs.
        let mut mem = ProcessMemory::new(0x10000, 0x3000_0000, 0x3010_0000, 0);

        // Eager munmap pattern: free_mmap called → address reused.
        let a1 = mem.alloc_mmap(0x1000).unwrap();
        mem.free_mmap(a1, 0x1000);
        let a2 = mem.alloc_mmap(0x1000).unwrap();
        assert_eq!(a2, a1, "eager freed region should be reused");

        // Lazy munmap pattern: free_mmap NOT called → next alloc advances.
        let b1 = mem.alloc_mmap(0x1000).unwrap();
        // (simulate lazy munmap: skip free_mmap)
        let b2 = mem.alloc_mmap(0x1000).unwrap();
        assert_ne!(b2, b1, "lazy VA range must not be recycled without free_mmap");
    }

    fn noop_waker() -> Waker {
        fn noop(_: *const ()) {}
        fn clone(p: *const ()) -> RawWaker { RawWaker::new(p, &VTABLE) }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
    }

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

