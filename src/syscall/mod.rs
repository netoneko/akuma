//! System Call Handlers
//!
//! Implements the syscall interface for user programs.
//! Uses Linux-compatible ABI: syscall number in x8, arguments in x0-x5.

use alloc::string::String;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::format;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use spinning_top::Spinlock;
// The folded user-copy API (validate + prefault + copy, safe `fn`s). Re-exported
// rather than imported per-module: every syscall submodule reaches these through
// `use super::*`, which is how it already reached the raw pair.
// `docs/archive/UNSAFE_AUDIT.md` §4 P0.
pub use akuma_exec::mmu::user_access::{
    Prefault, as_user_bytes, as_user_bytes_mut, copy_from_user, copy_to_user,
    copy_to_user_with, read_user_into, read_user_into_with, write_user_val,
    write_user_val_with,
};
// The raw byte-loop primitive. One caller left in this crate's syscall layer:
// `copy_from_user_byte`, which reads a NUL-terminated string one byte at a time and
// therefore has no range to validate — see its doc comment.
use akuma_exec::mmu::user_access::copy_from_user_safe;
// The excursion shape crate: which counter bucket a number falls in, which
// hooks run, and where the epilogue's identity comes from. Decisions only — see
// `EXCURSION_HOOKS` and docs/archive/AKUMA_EXTRACT_SYSCALLS.md §7.
use akuma_syscalls::Counter;

#[cfg(feature = "sc-aio")]
mod aio;
#[cfg(feature = "sc-reboot")]
mod reboot;
#[cfg(feature = "sc-containers")]
mod container;
#[cfg(feature = "sc-pidfd")]
pub mod pidfd;
#[cfg(feature = "sc-eventfd")]
pub mod eventfd;
pub mod log;
#[cfg(feature = "sc-sysv-ipc")]
pub mod msgqueue;
#[cfg(kernel_framebuffer)]
mod fb;
pub mod fs;
pub mod flock;
pub mod mem;
mod net;
/// Boot self-test for the net bounce-buffer allocator (see `net::alloc_net_bounce`).
#[cfg(kernel_tests)]
pub use net::run_net_bounce_tests;
/// Boot self-test for `SO_RCVTIMEO`/`SO_SNDTIMEO` (see `net::sys_setsockopt`).
#[cfg(all(kernel_tests, feature = "smoltcp"))]
pub use net::run_socket_timeout_tests;
#[cfg(kernel_tests)]
pub use poll::run_pselect6_exceptfds_test;
#[cfg(kernel_tests)]
pub use poll::run_pselect6_registers_waker_test;
#[cfg(kernel_tests)]
pub use poll::run_pselect6_eintr_test;
/// Boot self-test for `writev`'s short-write rule (see `fs::writev_stops_after`).
#[cfg(kernel_tests)]
pub use fs::run_writev_short_write_tests;
pub mod pipe;
pub mod poll;
pub mod proc;
/// AF_UNIX sockets. The decisions live in `akuma_net::unix` (host-tested); this
/// module is the kernel half — the one table, the user-pointer copies, and the
/// pipes that carry the bytes. See docs/archive/UNIX_SOCKET_IMPROVEMENTS.md.
pub mod unixsock;
pub mod signal;
mod sync;
/// `akuma_get_version` — the build identity packed into the return register,
/// and the floor control for the syscall boundary. See the module docs.
pub mod version;
mod term;
/// Itimers, clock_gettime/settime/getres, nanosleep, adjtimex — moved to the
/// `akuma-syscalls-time` crate 2026-08-25 (docs/archive/MISSING_NTP_SYSCALLS.md); this
/// alias keeps every `time::sys_*` call site below unchanged.
use akuma_syscalls_time as time;
#[cfg(feature = "sc-timerfd")]
mod timerfd;

/// The calling process's `(box_id, pid)`.
///
/// A kernel thread has no `Process`, which is how the built-in shell and the
/// boot path reach the box syscalls; it is the host, box 0. Every box-access
/// check keys off this, so it is the one place the caller's identity is
/// decided. It lives here rather than in `container` because `proc`'s
/// `SPAWN_EXT` / `SET_BOX_STACK` gate on it too, and those are dispatched even
/// when `sc-containers` is off (extreme-size).
pub fn caller_box_and_pid() -> (u64, u32) {
    akuma_exec::process::current_process_shared().map_or((0, 0), |p| (p.box_id, p.pid))
}

pub use sync::futex_wake;
pub use sync::futex_purge_tid;
pub use sync::futex_dump;
pub use time::check_itimers;
#[cfg(kernel_tests)]
pub use sync::futex_do_wake;
/// Futex waiter-table hooks for `process_tests::test_futex_table_irq_masked_requeue`.
#[cfg(kernel_tests)]
pub use sync::test_hooks as futex_test_hooks;
#[cfg(kernel_tests)]
pub use sync::futex_wait_at_tgid_for_test;
#[cfg(kernel_tests)]
pub use mem::membarrier_cmd;
#[cfg(kernel_tests)]
pub use fs::sys_close_range;

// Re-export the mmap alignment-EINVAL helper + the flag bits used by kernel
// tests. `mod mem` is private; these wrappers keep the module boundary intact.
#[cfg(kernel_tests)]
pub use mem::mmap_fixed_addr_unaligned_einval;
#[cfg(kernel_tests)]
pub use mem::{MAP_ANONYMOUS, MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_PRIVATE};


pub static CURRENT_SYSCALL_NR: AtomicU64 = AtomicU64::new(9999);
pub fn current_syscall_nr() -> u64 { CURRENT_SYSCALL_NR.load(Ordering::Relaxed) }

pub mod syscall_counters {
    use core::sync::atomic::{AtomicU64, Ordering};
    static MMAP_COUNT: AtomicU64 = AtomicU64::new(0);
    static MMAP_PAGES: AtomicU64 = AtomicU64::new(0);
    static MUNMAP_COUNT: AtomicU64 = AtomicU64::new(0);
    static BRK_COUNT: AtomicU64 = AtomicU64::new(0);
    static READ_COUNT: AtomicU64 = AtomicU64::new(0);
    static WRITE_COUNT: AtomicU64 = AtomicU64::new(0);
    static OPENAT_COUNT: AtomicU64 = AtomicU64::new(0);
    static CLOSE_COUNT: AtomicU64 = AtomicU64::new(0);
    static MPROTECT_COUNT: AtomicU64 = AtomicU64::new(0);
    static FUTEX_COUNT: AtomicU64 = AtomicU64::new(0);
    static SIGPROCMASK_COUNT: AtomicU64 = AtomicU64::new(0);
    static SIGACTION_COUNT: AtomicU64 = AtomicU64::new(0);
    static CLOCK_COUNT: AtomicU64 = AtomicU64::new(0);
    static IOCTL_COUNT: AtomicU64 = AtomicU64::new(0);
    static FSTAT_COUNT: AtomicU64 = AtomicU64::new(0);
    static YIELD_COUNT: AtomicU64 = AtomicU64::new(0);
    static MADVISE_COUNT: AtomicU64 = AtomicU64::new(0);
    static MREMAP_COUNT: AtomicU64 = AtomicU64::new(0);
    static LSEEK_COUNT: AtomicU64 = AtomicU64::new(0);
    static GETRANDOM_COUNT: AtomicU64 = AtomicU64::new(0);
    static GETPID_COUNT: AtomicU64 = AtomicU64::new(0);
    static FCNTL_COUNT: AtomicU64 = AtomicU64::new(0);
    static TOTAL_COUNT: AtomicU64 = AtomicU64::new(0);
    static PAGEFAULT_COUNT: AtomicU64 = AtomicU64::new(0);
    static PAGEFAULT_PAGES: AtomicU64 = AtomicU64::new(0);
    static OTHER_LAST_NR: AtomicU64 = AtomicU64::new(0);
    static OTHER_COUNT: AtomicU64 = AtomicU64::new(0);
    // QEMU TCG EC=0x15 misrouting emulation hit counters
    static QEMU_DC_ZVA_EC15_COUNT: AtomicU64 = AtomicU64::new(0);
    static QEMU_STP_XZR_EC15_COUNT: AtomicU64 = AtomicU64::new(0);

    pub fn inc_mmap(pages: usize) { MMAP_COUNT.fetch_add(1, Ordering::Relaxed); MMAP_PAGES.fetch_add(pages as u64, Ordering::Relaxed); }
    pub fn inc_munmap() { MUNMAP_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_brk() { BRK_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_read() { READ_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_write() { WRITE_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_openat() { OPENAT_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_close() { CLOSE_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_mprotect() { MPROTECT_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_futex() { FUTEX_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_sigprocmask() { SIGPROCMASK_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_sigaction() { SIGACTION_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_clock() { CLOCK_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_ioctl() { IOCTL_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_fstat() { FSTAT_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_yield() { YIELD_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_madvise() { MADVISE_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_mremap() { MREMAP_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_lseek() { LSEEK_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_getrandom() { GETRANDOM_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_getpid() { GETPID_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_fcntl() { FCNTL_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_other(nr: u64) { OTHER_COUNT.fetch_add(1, Ordering::Relaxed); OTHER_LAST_NR.store(nr, Ordering::Relaxed); }
    pub fn inc_total() { TOTAL_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_pagefault(pages_mapped: u64) { PAGEFAULT_COUNT.fetch_add(1, Ordering::Relaxed); PAGEFAULT_PAGES.fetch_add(pages_mapped, Ordering::Relaxed); }
    pub fn inc_qemu_dc_zva_ec15() { QEMU_DC_ZVA_EC15_COUNT.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_qemu_stp_xzr_ec15() { QEMU_STP_XZR_EC15_COUNT.fetch_add(1, Ordering::Relaxed); }
    #[cfg(kernel_tests)]
    pub fn get_qemu_stp_xzr_ec15() -> u64 { QEMU_STP_XZR_EC15_COUNT.load(Ordering::Relaxed) }

    pub fn dump() {
        let total = TOTAL_COUNT.load(Ordering::Relaxed);
        let other = OTHER_COUNT.load(Ordering::Relaxed);
        let last_nr = OTHER_LAST_NR.load(Ordering::Relaxed);
        crate::safe_print!(512,
            "[SC-STATS] total={} madvise={} mremap={} lseek={} rnd={} pid={} fcntl={} other={}(last_nr={})\n",
            total,
            MADVISE_COUNT.load(Ordering::Relaxed),
            MREMAP_COUNT.load(Ordering::Relaxed),
            LSEEK_COUNT.load(Ordering::Relaxed),
            GETRANDOM_COUNT.load(Ordering::Relaxed),
            GETPID_COUNT.load(Ordering::Relaxed),
            FCNTL_COUNT.load(Ordering::Relaxed),
            other, last_nr,
        );
        crate::safe_print!(512,
            "[SC-STATS] futex={} sigmask={} sigact={} clk={} ioctl={} fstat={} yield={}\n",
            FUTEX_COUNT.load(Ordering::Relaxed),
            SIGPROCMASK_COUNT.load(Ordering::Relaxed),
            SIGACTION_COUNT.load(Ordering::Relaxed),
            CLOCK_COUNT.load(Ordering::Relaxed),
            IOCTL_COUNT.load(Ordering::Relaxed),
            FSTAT_COUNT.load(Ordering::Relaxed),
            YIELD_COUNT.load(Ordering::Relaxed),
        );
        crate::safe_print!(384,
            "[SC-STATS] mmap={}({}pg) munmap={} brk={} read={} write={} open={} close={} mprot={} pgfault={}({}pg)\n",
            MMAP_COUNT.load(Ordering::Relaxed),
            MMAP_PAGES.load(Ordering::Relaxed),
            MUNMAP_COUNT.load(Ordering::Relaxed),
            BRK_COUNT.load(Ordering::Relaxed),
            READ_COUNT.load(Ordering::Relaxed),
            WRITE_COUNT.load(Ordering::Relaxed),
            OPENAT_COUNT.load(Ordering::Relaxed),
            CLOSE_COUNT.load(Ordering::Relaxed),
            MPROTECT_COUNT.load(Ordering::Relaxed),
            PAGEFAULT_COUNT.load(Ordering::Relaxed),
            PAGEFAULT_PAGES.load(Ordering::Relaxed),
        );
    }
}

/// Flag to bypass pointer validation during kernel-originated syscall tests.
///
/// Re-export: the flag moved to `akuma_exec::mmu::user_access` with the check it
/// disables, so the copy helpers can honour it. The ~50 boot-test call sites keep
/// spelling it `crate::syscall::BYPASS_VALIDATION`.
pub use akuma_exec::mmu::user_access::BYPASS_VALIDATION;

/// Syscall numbers (Linux-compatible subset).
///
/// Moved to `akuma_syscalls_linux::nr` on 2026-08-27 — 261 lines of table that
/// lived in the bin crate, so no library crate could name a syscall it
/// implements (`akuma-syscalls-time` owns six of them). Re-exported rather than
/// imported per-module: every `nr::FOO` below, and every submodule reaching it
/// through `use super::*`, keeps its spelling.
///
/// The `#[cfg(feature = ...)]` gates the table used to carry are gone with the
/// move; see that module's docs. The dispatch arms in this file stay gated.
pub use akuma_syscalls_linux::nr;

/// Thread CPU statistics for top command
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadCpuStat {
    pub tid: u32,
    pub pid: u32,
    pub box_id: u64,
    pub total_time_us: u64,
    pub state: u8,
    /// Last core the thread ran on (MPIDR aff0). 0xFF = never scheduled.
    pub last_core: u8,
    pub _reserved: [u8; 6],
    pub name: [u8; 16],
}

// The negated errno values every syscall arm returns (`x0 = -errno`; userspace
// decodes via `if ((long)ret < 0) errno = -(int)ret`). These used to be 29
// hand-written consts here, which is how `E2BIG` came to be defined a second time
// in `msgqueue.rs` and `EROFS` a third in `fs.rs`: this table was private to the
// bin crate, so anything outside it — including `akuma-net`, which needs the same
// values positively signed — had to write its own.
//
// One table now, in the dependency-free leaf, with the negated forms generated
// from the positive ones so they cannot drift. Re-exported rather than imported
// per-module: the submodules below reach these through `use super::*`, which is
// how they always saw them.
// See `akuma_primitives::errno` and TRIM_FAT_EMBARASSING_DUPLICATIONS.md §5.7.
pub use akuma_primitives::errno::negated::*;
pub use akuma_primitives::errno::neg_errno;

/// ENETDOWN as the syscall ABI expects it. Used by the AF_INET dispatch arms on a
/// rump-only build (smoltcp compiled out): a socket syscall that somehow reaches
/// native dispatch (it normally can't — the rump proxy short-circuits it) gets a
/// clean "network is down" instead of a missing-symbol link error.
#[cfg(not(feature = "smoltcp"))]
#[inline]
fn net_enetdown() -> u64 {
    neg_errno(akuma_net::socket::libc_errno::ENETDOWN)
}

// The `repr(C)` wire structs, from the one place they are defined and
// layout-asserted (`akuma-syscalls-linux`, 2026-08-27). Re-exported rather
// than imported per-module: the submodules below reach them through
// `use super::*`, which is how they reached the local definitions these
// replace. `Copy` is what `read_user_into`/`write_user_val` take as the marker
// for "plain ABI data, safe to move byte-wise"; every type here has it.
pub use akuma_syscalls_linux::{
    CloneArgs, IoVec, KernelSigaction, MsgHdr, PollFd, Rlimit, SigChld, Siginfo, StackT, Stat,
    Statfs, Statx, StatxTimestamp, Sysinfo, Timespec, Ucred, makedev,
};
// The `ifreq`/`ifconf` ioctl shapes and `struct timeval` are reached only from
// the native (smoltcp) socket surface; a rump-only build compiles none of it.
#[cfg(feature = "smoltcp")]
pub use akuma_syscalls_linux::{IfConfHdr, SockAddrHw, Timeval};
// Split out only because `unused_imports` is `deny` and an `extreme-size` build
// compiles no epoll surface at all.
#[cfg(feature = "sc-epoll")]
pub use akuma_syscalls_linux::EpollEvent;

/// Exposed for kernel tests only.
#[cfg(kernel_tests)]
pub fn user_va_limit_value() -> u64 {
    user_va_limit()
}

/// Exposed for kernel tests only — see `test_ensure_user_pages_mapped_as_lock`.
#[cfg(kernel_tests)]
pub fn ensure_user_pages_mapped_for_test(start: usize, len: usize) -> bool {
    akuma_exec::mmu::user_access::prefault_user_range(start, len)
}

/// The 48-bit user VA limit. Now `akuma_exec::mmu::user_access::USER_VA_LIMIT` —
/// see there for why it is not a smaller cap (Go's high arenas).
fn user_va_limit() -> u64 {
    akuma_exec::mmu::user_access::USER_VA_LIMIT
}

/// Validate a user pointer range, faulting lazy pages in.
///
/// Thin forwarder: the body — the range tests *and* the demand-paging half — moved
/// to `akuma_exec::mmu::user_access` so the copy helpers could fold it in and stop
/// being skippable (`docs/archive/UNSAFE_AUDIT.md` §4 P0). Kept as a named function
/// because plenty of syscall arms validate a pointer they never copy through
/// (`futex` addresses, `mmap` args), and because `Prefault::Yes` is the right
/// default for every caller on a syscall stack.
fn validate_user_ptr(ptr: u64, len: usize) -> bool {
    akuma_exec::mmu::user_access::validate_user_range(
        ptr,
        len,
        akuma_exec::mmu::user_access::Prefault::Yes,
    )
}

/// Safely read a single byte from user memory.
///
/// **The one deliberate raw-copy caller left in the syscall layer.** Its only user is
/// [`copy_from_user_str`], which walks a NUL-terminated string one byte at a time —
/// and the length of a NUL-terminated string is not known until it has been read, so
/// there is no range to validate. Routing each byte through `copy_from_user` would
/// page-table-walk **per byte**: 4096 walks for a `PATH_MAX` path. The string reader
/// does its own per-byte limit check and relies on the byte loop's fixup for
/// mapped-ness, which is the correct shape for an unknown-length read.
/// Measurement helpers for this layer — see [`utils`].
pub mod utils;

pub fn copy_from_user_byte(ptr: u64) -> Result<u8, u64> {
    let mut b: u8 = 0;
    // SAFETY: one byte into a live local; the user side is covered by the recovery
    // trampoline, and `copy_from_user_str` bounds `ptr` against the VA limit.
    if unsafe { copy_from_user_safe(&raw mut b, ptr as *const u8, 1).is_err() } {
        return Err(EFAULT);
    }
    Ok(b)
}

pub fn copy_from_user_str(ptr: u64, max_len: usize) -> Result<String, u64> {
    let limit = user_va_limit();
    if !BYPASS_VALIDATION.load(Ordering::Acquire)
        && (ptr < 0x1000 || ptr >= limit) { return Err(EFAULT); }
    let mut bytes = Vec::new();
    let mut len = 0;
    while len < max_len {
        let addr = ptr + len as u64;
        if !BYPASS_VALIDATION.load(Ordering::Acquire) && addr >= limit {
            return Err(EFAULT);
        }
        
        let c = copy_from_user_byte(addr)?;
        if c == 0 { break; }
        bytes.push(c);
        len += 1;
    }
    if len == max_len {
        return Err(EINVAL);
    }
    
    if let Ok(s) = core::str::from_utf8(&bytes) { Ok(String::from(s)) } else {
        crate::safe_print!(64, "[syscall] copy_from_user_str: invalid UTF-8\n");
        Err(EINVAL)
    }
}

/// This build's excursion gates, handed to `akuma-syscalls` as data.
///
/// The crate takes the config consts as a parameter rather than reading them,
/// which is what makes every gate combination reachable from a host test —
/// `PROCESS_SYSCALL_STATS` and `PROC_SYSCALL_LOG_ENABLED` are `true` in every
/// profile but `kernel_profile_extreme`, so the off-arms were previously only
/// testable by building a different kernel.
///
/// `identity` is the one field that is a decision rather than a mirror of an
/// existing const — see `akuma_syscalls::IdentitySource` and the note on
/// `EPILOGUE_IDENTITY` below.
const EXCURSION_HOOKS: akuma_syscalls::HookConfig = akuma_syscalls::HookConfig {
    process_stats: crate::config::PROCESS_SYSCALL_STATS,
    proc_log: crate::config::PROC_SYSCALL_LOG_ENABLED,
    debug_io: crate::config::SYSCALL_DEBUG_IO_ENABLED,
    errno_diag: crate::config::SYSCALL_ERRNO_DIAG_ENABLED,
    identity_audit: crate::config::IDENTITY_AUDIT,
    identity: EPILOGUE_IDENTITY,
};

/// Which resolution the epilogue writes through.
///
/// `Prologue` — reuse the pointer resolved before the dispatch — is what
/// shipped from the syscall audit until 2026-08-28, and it is a use-after-free:
/// `akuma_syscalls::slot` enumerates the table's `claim / retire / reclaim /
/// stamp / validate` lifecycle and finds a witness **two peer operations deep**
/// (`Retire(0)`, `Reclaim(0)`), which is `kill_thread_group` retiring a sibling
/// that is still inside a blocking syscall, followed by any idle core's reclaim
/// drain 10 ms later. `IDENTITY_CACHE_SMP_REVIEW.md` Finding A argued this;
/// the crate's search settles it, and the same search finds no witness for
/// `Reresolve` anywhere in its bound.
///
/// Re-reading costs one cache read — a validated slot-state load and a
/// generation load — against the lock + map walk + IRQ-masked table scan the
/// pre-cache epilogue paid twice for the same guard. Measured: no change to the
/// `getpid` floor (230 ns either way, best of 100x100).
pub const EPILOGUE_IDENTITY: akuma_syscalls::IdentitySource =
    akuma_syscalls::IdentitySource::Reresolve;

pub fn handle_syscall(syscall_num: u64, args: &[u64; 6]) -> u64 {
    // Outer span for `read-profile` (ZST otherwise): started before the pid
    // lookup and counter bumps below, so `hs - sr` names this prologue/epilogue.
    let rp_span = crate::syscall::utils::read_profile::Span::new();
    // The generic shape of this excursion: which counter bucket, whether the
    // signal-state clear applies, whether each hook runs, and where the
    // epilogue gets its identity. Decisions only — every effect below is still
    // performed here. All of it is `const fn` over plain data, so it inlines
    // into the branches it replaced rather than adding a call.
    let excursion = akuma_syscalls::Excursion::new(syscall_num, EXCURSION_HOOKS);
    let plan = excursion.prologue();
    CURRENT_SYSCALL_NR.store(syscall_num, Ordering::Relaxed);
    crate::syscall::utils::read_profile::floor_laps::start(syscall_num);

    akuma_exec::threading::set_thread_current_syscall(syscall_num);
    // A fresh excursion starts with no delivered-signal record, so one delivery
    // cannot fabricate an EINTR in an unrelated later syscall.
    //
    // `rt_sigreturn` (139) is deliberately exempt: the handler returns THROUGH it,
    // so clearing there would erase the record belonging to the blocking syscall
    // that is about to resume — the exact starvation this mask fixes. See
    // docs/archive/PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md and
    // `akuma_syscalls::clears_signal_state`, which is where that exemption now
    // has a test.
    if plan.clear_signal_state {
        let slot = akuma_exec::threading::current_thread_id();
        akuma_exec::threading::clear_delivered_signals(slot);
        // Reaching any other syscall proves userspace ran, which is the unit of
        // progress that re-arms signal delivery. Same `rt_sigreturn` exemption and
        // for the same reason: the handler returns THROUGH it, so userspace has not
        // run yet at that point.
        akuma_exec::threading::clear_sigframe_active(slot);
    }
    // One resolution per excursion, from the per-thread identity cache
    // (`table::THREAD_IDENTITY`): the tgid + leader `Process` pair the whole
    // prologue/epilogue below wants. The cache is one validated slot-state
    // load; the previous shape re-derived this up to five times per syscall
    // (lock + map walk + IRQ-masked table scan each) and was most of the gap
    // to Linux on a bare `getpid` (docs/archive/AKUMA_SYSCALL_PERFORMANCE_AUDIT.md).
    // Skipped entirely for a `FastPath::Leaf` number (`akuma_syscalls::fast_path`):
    // an arm that reads no arguments and touches nothing reachable from "who is
    // calling" has no use for an identity, and the bookkeeping done on its behalf
    // has nothing to write. That drops the resolve, both `Process` stamps, the
    // per-process stats, the clock reads and the epilogue's re-resolve. It does
    // NOT drop `CURRENT_SYSCALL_NR`/`set_thread_current_syscall` above (a crash
    // dump reads those to say which syscall a thread was in) or the counters
    // below (the totals would stop adding up). Membership and the four admission
    // criteria are in the crate; the cost of admission is that those syscalls
    // stop appearing in `/proc/<pid>/syscalls`.
    let cur = if plan.resolve_identity {
        akuma_exec::process::table::current_thread_tgid_process()
    } else {
        None
    };
    let owner_pid = cur.map_or(0, |(pid, _)| pid);
    if let Some((_, proc)) = cur {
        proc.last_syscall.store(syscall_num, Ordering::Relaxed);
        proc.current_syscall.store(syscall_num, Ordering::Relaxed);
    }
    crate::syscall::utils::read_profile::floor_laps::lap(
        crate::syscall::utils::read_profile::F_LAP_IDENT,
    );

    if akuma_exec::process::is_current_interrupted() {
        // Plain `Process`-field writes still rely on the BKL for cross-core exclusion
        // (`with_current_process` masks IRQs = same-core only; locking.md's
        // load-bearing table). A BKL-opted-out syscall (Phase 7f) pauses its window so
        // this cold arm runs held, exactly like every non-opted-out path; no-op there.
        let _held = akuma_exec::bkl::DroppedWindowPause::new();
        akuma_exec::process::with_current_process(|p| {
            p.exited = true;
            p.exit_code = 130;
            p.state = akuma_exec::process::ProcessState::Zombie(130);
        });
        return EINTR;
    }
    crate::syscall::utils::read_profile::floor_laps::lap(
        crate::syscall::utils::read_profile::F_LAP_INTRPT,
    );


    // The suppression list this gate used to spell inline lives in
    // `akuma_syscalls::debug_io_suppressed`, which is the only place a test can
    // reach it: no shipping profile sets `SYSCALL_DEBUG_IO_ENABLED`, so a number
    // silently added to or dropped from the list changes nothing observable
    // until someone turns the flag on to debug something else.
    if plan.debug_print {
        crate::safe_print!(128, "[SC] nr={} a0=0x{:x} a1=0x{:x} a2=0x{:x}\n", syscall_num, args[0], args[1], args[2]);
    }

    syscall_counters::inc_total();
    // Classification is `akuma_syscalls::counter_for` (host-tested against the
    // arms this `match` used to hold); the counters themselves stay here. Both
    // halves are `match`es over a C-like enum with no indirection between them,
    // so this compiles back to the single dispatch it replaced — checked by
    // measurement, not assumed.
    match plan.counter {
        // The one arm with a payload: `pages` comes from `args[1]`, which the
        // classifier never sees.
        Counter::Mmap => { syscall_counters::inc_mmap((args[1] as usize).div_ceil(4096)); }
        Counter::Munmap => { syscall_counters::inc_munmap(); }
        Counter::Brk => { syscall_counters::inc_brk(); }
        Counter::Read => { syscall_counters::inc_read(); }
        Counter::Write => { syscall_counters::inc_write(); }
        Counter::Openat => { syscall_counters::inc_openat(); }
        Counter::Close => { syscall_counters::inc_close(); }
        Counter::Mprotect => { syscall_counters::inc_mprotect(); }
        Counter::Futex => { syscall_counters::inc_futex(); }
        Counter::SigProcMask => { syscall_counters::inc_sigprocmask(); }
        Counter::SigAction => { syscall_counters::inc_sigaction(); }
        Counter::Clock => { syscall_counters::inc_clock(); }
        Counter::Ioctl => { syscall_counters::inc_ioctl(); }
        Counter::Fstat => { syscall_counters::inc_fstat(); }
        Counter::Yield => { syscall_counters::inc_yield(); }
        Counter::Madvise => { syscall_counters::inc_madvise(); }
        Counter::Mremap => { syscall_counters::inc_mremap(); }
        Counter::Lseek => { syscall_counters::inc_lseek(); }
        Counter::Getrandom => { syscall_counters::inc_getrandom(); }
        Counter::Getpid => { syscall_counters::inc_getpid(); }
        Counter::Fcntl => { syscall_counters::inc_fcntl(); }
        Counter::Other => { syscall_counters::inc_other(syscall_num); }
    }

    if plan.record_stats && let Some((_, proc)) = cur {
        proc.syscall_stats.inc(syscall_num);
    }

    // One clock read serves both hooks; `need_timing` is their union, decided
    // once in the prologue and read again in the epilogue rather than
    // recomputed there.
    let t0 = if plan.need_timing { crate::timer::uptime_us() } else { 0 };
    crate::syscall::utils::read_profile::floor_laps::lap(
        crate::syscall::utils::read_profile::F_LAP_HOOKS,
    );

    // For a `stack=rump` box, the proxy intercepts socket-family syscalls (and
    // read/write/close on rump-owned fds) and forwards them to the box's
    // rump_server. `Some(r)` short-circuits the normal smoltcp dispatch with the
    // proxied result; `None` falls through unchanged (the common case, incl. all
    // non-rump boxes — a single relaxed atomic load). It also emits the
    // `[RUMP-SP]` trace.
    #[cfg(feature = "rump")]
    let rump_result = crate::rump_proxy::intercept_box_syscall(syscall_num, args);
    #[cfg(not(feature = "rump"))]
    let rump_result: Option<u64> = None;

    let result = match rump_result {
        Some(r) => r,
        None => match syscall_num {
        // exit/exit_group must NEVER return to EL0. sys_exit/sys_exit_group
        // normally park the calling thread, but fall through (returning the
        // exit code!) when `current_process()` is already None — e.g. a
        // CLONE_VM sibling still running on another core after its group's
        // teardown unregistered the process (SMP forktest SIGTERM deadline).
        // Go's runtime deliberately crashes (`str xzr,[x0]`, x0=0) when exit
        // returns — the WILD-DA FAR=0x0 ELR=runtime.fatalthrow noise.
        // return_to_kernel handles the process-already-gone case and parks
        // the thread.
        nr::EXIT => {
            let code = args[0] as i32;
            proc::sys_exit(code);
            akuma_exec::process::return_to_kernel(code)
        }
        nr::READ => fs::sys_read(args[0], args[1], args[2] as usize),
        nr::WRITE => fs::sys_write(args[0], args[1], args[2] as usize),
        nr::READV => fs::sys_readv(args[0], args[1], args[2] as usize),
        nr::WRITEV => fs::sys_writev(args[0], args[1], args[2] as usize),
        nr::IOCTL => term::sys_ioctl(args[0] as u32, args[1] as u32, args[2]),
        nr::DUP => fs::sys_dup(args[0] as u32),
         nr::FSTATFS => fs::sys_fstatfs(args[0] as u32, args[1]),
         nr::STATFS => fs::sys_statfs(args[0], args[1]),
        nr::DUP3 => fs::sys_dup3(args[0] as u32, args[1] as u32, args[2] as u32),
        nr::PIPE2 => pipe::sys_pipe2(args[0], args[1] as u32),
        nr::BRK => mem::sys_brk(args[0] as usize),
        nr::OPENAT => fs::sys_openat(args[0] as i32, args[1], args[2] as u32, args[3] as u32),
        nr::CLOSE => fs::sys_close(args[0] as u32),
        nr::LSEEK => fs::sys_lseek(args[0] as u32, args[1] as i64, args[2] as i32),
        nr::FSTAT => fs::sys_fstat(args[0] as u32, args[1]),
        nr::NANOSLEEP => time::sys_nanosleep(args[0], args[1]),
        nr::CLOCK_NANOSLEEP => time::sys_clock_nanosleep(args[0] as u32, args[1] as i32, args[2], args[3]),
        // AF_INET socket ops. In a rump box these never reach here (the rump proxy
        // short-circuits above); with the smoltcp native stack compiled out they
        // have no implementation, so a stray box-0 call gets a clean ENETDOWN
        // instead of a link error. SOCKETPAIR (pipe-backed) and SHUTDOWN (no-op)
        // stay available on both builds.
        // Every socket-family syscall is dispatched UNCONDITIONALLY through a
        // `net::dispatch_*` wrapper that tries AF_UNIX first and only then the
        // native stack (or `ENETDOWN` when smoltcp is compiled out). Gating
        // these on `smoltcp` — as they were before AF_UNIX had a socket object
        // — gives the rump-only devbox, which is the DEFAULT devbox, a family
        // that can create sockets and then refuse to bind them. AF_UNIX is
        // smoltcp-free by construction and is the family box 0's rump sysproxy
        // channel already uses. See `net::dispatch_socket` and
        // docs/archive/UNIX_SOCKET_IMPROVEMENTS.md.
        nr::SOCKET => net::dispatch_socket(args[0] as i32, args[1] as i32, args[2] as i32),
        nr::SOCKETPAIR => net::sys_socketpair(args[0] as i32, args[1] as i32, args[2] as i32, args[3]),
        nr::BIND => net::dispatch_bind(args[0] as u32, args[1], args[2] as usize),
        nr::LISTEN => net::dispatch_listen(args[0] as u32, args[1] as i32),
        // `accept` is `accept4` with flags == 0; one path serves both.
        nr::ACCEPT => net::dispatch_accept(args[0] as u32, args[1], args[2], 0),
        nr::ACCEPT4 => net::dispatch_accept(args[0] as u32, args[1], args[2], args[3] as u32),
        nr::CONNECT => net::dispatch_connect(args[0] as u32, args[1], args[2] as usize),
        // Always dispatched (both smoltcp and rump-only builds define these): the
        // rump-only variants handle a UnixSocket (pipe-backed) fd — the box-0
        // rump_server's fd-3 sysproxy channel uses send()/recv() on it — and EBADF
        // anything else. Gating these to net_enetdown() breaks the rump handshake.
        nr::SENDTO => net::sys_sendto(args[0] as u32, args[1], args[2] as usize, args[3] as i32, args[4], args[5] as usize),
        nr::RECVFROM => net::sys_recvfrom(args[0] as u32, args[1], args[2] as usize, args[3] as i32, args[4], args[5]),
        nr::GETSOCKNAME => net::dispatch_getsockname(args[0] as u32, args[1], args[2]),
        nr::GETPEERNAME => net::dispatch_getpeername(args[0] as u32, args[1], args[2]),
        nr::SETSOCKOPT => net::dispatch_setsockopt(args[0] as u32, args[1] as i32, args[2] as i32, args[3], args[4] as u32),
        nr::GETSOCKOPT => net::dispatch_getsockopt(args[0] as u32, args[1] as i32, args[2] as i32, args[3], args[4]),
        nr::SHUTDOWN => net::dispatch_shutdown(args[0] as u32, args[1] as i32),
        // Always dispatched: the rump-only variant handles the box-0 rump_server's
        // fd-3 UnixSocket channel (dosend → sendmsg for the handshake RESP + all
        // proxied-syscall replies). Gating it to net_enetdown() breaks rump entirely.
        nr::SENDMSG => net::sys_sendmsg(args[0] as u32, args[1], args[2] as i32),
        nr::RECVMSG => net::dispatch_recvmsg(args[0] as u32, args[1], args[2] as i32),
        nr::MREMAP => mem::sys_mremap(args[0] as usize, args[1] as usize, args[2] as usize, args[3] as u32),
        nr::MMAP => mem::sys_mmap(args[0] as usize, args[1] as usize, args[2] as u32, args[3] as u32, args[4] as i32, args[5] as usize),
        nr::MUNMAP => mem::sys_munmap(args[0] as usize, args[1] as usize),
        nr::CLONE => proc::sys_clone(args[0], args[1], args[2], args[3], args[4]),
        nr::CLONE3 => proc::sys_clone3(args[0], args[1] as usize),
        nr::EXECVE => proc::sys_execve(args[0], args[1], args[2]),
        nr::UPTIME => time::sys_uptime(),
        // The floor control: no arguments read, no user memory touched, no
        // process resolved — a constant into x0. Everything this arm's cost
        // consists of is the boundary itself, which is the point.
        nr::AKUMA_GET_VERSION => version::sys_akuma_get_version(),
        #[cfg(feature = "smoltcp")]
        nr::RESOLVE_HOST => net::sys_resolve_host(args[0], args[1] as usize, args[2]),
        #[cfg(not(feature = "smoltcp"))]
        nr::RESOLVE_HOST => net_enetdown(),
        nr::GETDENTS64 => fs::sys_getdents64(args[0] as u32, args[1], args[2] as usize),
        nr::PSELECT6 => poll::sys_pselect6(args[0] as usize, args[1], args[2], args[3], args[4], args[5]),
        nr::PPOLL => poll::sys_ppoll(args[0], args[1] as usize, args[2], args[3]),
        nr::MKDIRAT => fs::sys_mkdirat(args[0] as i32, args[1], args[2] as u32),
        nr::UNLINKAT => fs::sys_unlinkat(args[0] as i32, args[1], args[2] as u32),
        nr::SYMLINKAT => fs::sys_symlinkat(args[0], args[1] as i32, args[2]),
        nr::LINKAT => fs::sys_linkat(args[0] as i32, args[1], args[2] as i32, args[3], args[4] as u32),
        nr::RENAMEAT => fs::sys_renameat(args[0] as i32, args[1], args[2] as i32, args[3]),
        nr::RENAMEAT2 => fs::sys_renameat2(args[0] as i32, args[1], args[2] as i32, args[3], args[4] as u32),
        nr::STATX => fs::sys_statx(args[0] as i32, args[1], args[2] as u32, args[3] as u32, args[4]),
        nr::READLINKAT => fs::sys_readlinkat(args[0] as i32, args[1], args[2], args[3] as usize),
        nr::SPAWN => proc::sys_spawn(args[0], args[1], args[2], args[3], args[4] as usize, args[5]),
        nr::KILL => proc::sys_kill(args[0] as u32, args[1] as u32),
        #[cfg(feature = "sc-reboot")]
        nr::REBOOT => reboot::sys_reboot(args[0] as u32, args[1] as u32, args[2] as u32),
        nr::WAITPID => proc::sys_waitpid(args[0] as u32, args[1]),
        nr::GETRANDOM => proc::sys_getrandom(args[0], args[1] as usize),
        nr::TIME => time::sys_time(),
        nr::CHDIR => fs::sys_chdir(args[0]),
        nr::FCHDIR => fs::sys_fchdir(args[0] as u32),
        nr::SET_TERMINAL_ATTRIBUTES => term::sys_set_terminal_attributes(args[0], args[1], args[2]),
        nr::GET_TERMINAL_ATTRIBUTES => term::sys_get_terminal_attributes(args[0], args[1]),
        nr::SET_CURSOR_POSITION => term::sys_set_cursor_position(args[0], args[1]),
        nr::HIDE_CURSOR => term::sys_hide_cursor(),
        nr::SHOW_CURSOR => term::sys_show_cursor(),
        nr::CLEAR_SCREEN => term::sys_clear_screen(),
        nr::POLL_INPUT_EVENT => term::sys_poll_input_event(args[0], args[1] as usize, args[2]),
        nr::GET_CPU_STATS => term::sys_get_cpu_stats(args[0], args[1] as usize),
        nr::SPAWN_EXT => proc::sys_spawn_ext(args[0], args[1], args[2], args[3], args[4], args[5]),
        nr::SET_BOX_STACK => proc::sys_set_box_stack(args[0], args[1]),
        nr::CLOSE_CHILD_STDIN => proc::sys_close_child_stdin(args[0] as u32),
        nr::CORE_INIT => proc::sys_core_init(args[0] as usize, args[1]),
        #[cfg(feature = "sc-containers")]
        nr::REGISTER_BOX => container::sys_register_box(args[0], args[1], args[2] as usize, args[3], args[4] as usize, args[5] as u32),
        #[cfg(feature = "sc-containers")]
        nr::KILL_BOX => container::sys_kill_box(args[0]),
        #[cfg(feature = "sc-containers")]
        nr::REATTACH => container::sys_reattach(args[0] as u32, args[1] as u32),
        nr::SET_TID_ADDRESS => proc::sys_set_tid_address(args[0]),
        // Same never-return contract as nr::EXIT above.
        nr::EXIT_GROUP => {
            let code = args[0] as i32;
            proc::sys_exit_group(code);
            akuma_exec::process::return_to_kernel(code)
        }
        nr::RT_SIGPROCMASK => signal::sys_rt_sigprocmask(args[0] as u32, args[1], args[2], args[3] as usize),
        nr::RT_SIGSUSPEND => signal::sys_rt_sigsuspend(args[0], args[1] as usize),
        nr::RT_SIGTIMEDWAIT => signal::sys_rt_sigtimedwait(args[0], args[1], args[2], args[3] as usize),
        nr::RT_SIGRETURN => 0,
        nr::RT_SIGACTION => signal::sys_rt_sigaction(args[0] as u32, args[1] as usize, args[2] as usize, args[3] as usize),
        nr::GETCWD => fs::sys_getcwd(args[0], args[1] as usize),
        nr::FCNTL => fs::sys_fcntl(args[0] as u32, args[1] as u32, args[2]),
        nr::NEWFSTATAT => fs::sys_newfstatat(args[0] as i32, args[1], args[2], args[3] as u32),
        nr::FACCESSAT => fs::sys_faccessat2(args[0] as i32, args[1], args[2] as u32, 0),
        nr::CLOCK_GETTIME => time::sys_clock_gettime(args[0], args[1]),
        nr::CLOCK_SETTIME => time::sys_clock_settime(args[0] as u32, args[1]),
        nr::ADJTIMEX => time::sys_adjtimex(args[0]),
        nr::CLOCK_ADJTIME => time::sys_clock_adjtime(args[0] as u32, args[1]),
        nr::FACCESSAT2 => fs::sys_faccessat2(args[0] as i32, args[1], args[2] as u32, args[3] as u32),
        nr::WAIT4 => proc::sys_wait4(args[0] as i32, args[1], args[2] as i32, args[3]),
        nr::WAITID => proc::sys_waitid(args[0] as u32, args[1] as u32, args[2], args[3] as i32),
        nr::SET_TPIDR_EL0 => proc::sys_set_tpidr_el0(args[0]),
        #[cfg(kernel_framebuffer)]
        nr::FB_INIT => fb::sys_fb_init(args[0] as u32, args[1] as u32),
        #[cfg(kernel_framebuffer)]
        nr::FB_DRAW => fb::sys_fb_draw(args[0], args[1] as usize),
        #[cfg(kernel_framebuffer)]
        nr::FB_INFO => fb::sys_fb_info(args[0]),
        nr::GETPID => proc::sys_getpid(),
        nr::GETPPID => proc::sys_getppid(),
        nr::GETUID => 0,
        nr::GETEUID => proc::sys_geteuid(),
        nr::GETGID => 0,
        nr::GETEGID => 0,
        nr::GETTID => akuma_exec::threading::current_thread_id() as u64,
        nr::GETRESUID => proc::sys_getresugid(args[0], args[1], args[2]),
        nr::GETRESGID => proc::sys_getresugid(args[0], args[1], args[2]),
        nr::GETGROUPS => proc::sys_getgroups(args[0] as i32, args[1]),
        nr::KILL_LINUX => proc::sys_kill(args[0] as u32, args[1] as u32),
        nr::SETPGID => proc::sys_setpgid(args[0] as u32, args[1] as u32),
        nr::GETPGID => proc::sys_getpgid(args[0] as u32),
        nr::SETSID => proc::sys_setsid(),
        nr::UNAME => proc::sys_uname(args[0]),
        nr::FLOCK => flock::sys_flock(args[0] as u32, args[1] as u32),
        nr::UMASK => 0o022,
        nr::UTIMENSAT => 0,
        nr::FDATASYNC => 0,
        nr::FSYNC => 0,
        nr::FCHMOD => fs::sys_fchmod(args[0] as u32, args[1] as u32),
        nr::FCHMODAT => fs::sys_fchmodat(args[0] as i32, args[1], args[2] as u32),
        nr::FCHOWNAT => 0,
        nr::FCHOWN => 0,
        nr::TRUNCATE => fs::sys_truncate(args[0], args[1] as i64),
        nr::FTRUNCATE => fs::sys_ftruncate(args[0] as u32, args[1] as i64),
        nr::FALLOCATE => fs::sys_fallocate(args[0] as u32, args[1] as i32, args[2] as i64, args[3] as i64),
        nr::MADVISE => mem::sys_madvise(args[0] as usize, args[1] as usize, args[2] as i32),
        nr::MPROTECT => mem::sys_mprotect(args[0] as usize, args[1] as usize, args[2] as u32),
        nr::FUTEX => sync::sys_futex(args[0] as usize, args[1] as i32, args[2] as u32, args[3], args[4] as usize, args[5] as u32),
        nr::SET_ROBUST_LIST => proc::sys_set_robust_list(args[0], args[1] as usize),
        nr::SIGALTSTACK => signal::sys_sigaltstack(args[0], args[1]),
        nr::GETRLIMIT => proc::sys_prlimit64(0, args[0] as u32, 0, args[1]),
        nr::PRLIMIT64 => proc::sys_prlimit64(args[0] as u32, args[1] as u32, args[2], args[3]),
        #[cfg(feature = "sc-eventfd")]
        nr::EVENTFD2 => eventfd::sys_eventfd2(args[0] as u32, args[1] as u32),
        nr::PREAD64 => fs::sys_pread64(args[0] as u32, args[1], args[2] as usize, args[3] as i64),
        nr::PWRITE64 => fs::sys_pwrite64(args[0] as u32, args[1], args[2] as usize, args[3] as i64),
        // args[4] is `pos_h`, which carries nothing on a 64-bit kernel — see
        // `fs::sys_pvec2`. The `2` variants add the `RWF_*` flags word.
        nr::PREADV => fs::sys_pvec2(args[0], args[1], args[2] as usize, args[3], 0, false),
        nr::PWRITEV => fs::sys_pvec2(args[0], args[1], args[2] as usize, args[3], 0, true),
        nr::PREADV2 => {
            fs::sys_pvec2(args[0], args[1], args[2] as usize, args[3], args[5] as u32, false)
        }
        nr::PWRITEV2 => {
            fs::sys_pvec2(args[0], args[1], args[2] as usize, args[3], args[5] as u32, true)
        }
        nr::SETITIMER => {
            time::sys_setitimer(args[0] as u32, args[1], args[2])
        }
        nr::MEMBARRIER => mem::membarrier_cmd(args[0] as u32),
        nr::PRCTL => proc::sys_prctl(args[0] as i32, args[1], args[2], args[3], args[4]),
        nr::TIMES => time::sys_times(args[0] as usize),
        nr::GETRUSAGE => time::sys_getrusage(args[0] as i32, args[1] as usize),
        nr::MSYNC => mem::sys_msync(args[0] as usize, args[1] as usize, args[2] as u32),
        nr::PROCESS_VM_READV => {
            if crate::config::SYSCALL_ENOSYS_DIAG {
                crate::tprint!(96, "[ENOSYS] nr=270 (process_vm_readv) pid={}\n",
                    akuma_exec::process::read_current_pid().unwrap_or(0));
            }
            ENOSYS
        }
        nr::SCHED_SETAFFINITY => 0,
        // setpriority(which, who, prio) — we don't schedule by nice value; accept it.
        nr::SETPRIORITY => 0,
        // getpriority(which, who) — the raw syscall returns `20 - nice` (kept >= 0 so
        // a real nice can't look like an errno); musl computes the caller-visible nice
        // as `20 - ret`. Return 20 => normal priority (nice 0). Leaving this
        // unimplemented returned ENOSYS, which rustc's threadpool then used as a
        // pointer → [WILD-DA] SIGSEGV that intermittently killed a build unit
        // (docs/AKUMA_SELF_HOSTING.md §7i).
        nr::GETPRIORITY => 20,
        // sched_setparam(pid, *param) — we don't implement real-time scheduling
        // params; accept unconditionally and ignore the pointer.
        nr::SCHED_SETPARAM => { 0 }
        // sched_setscheduler(pid, policy, *param) — args[1] is `policy`, an int,
        // not a pointer. This arm predates the named constant above and was
        // dispatched under the raw literal `119`; its body writes a zero
        // `sched_priority` into `args[1]` treated as a user pointer, which is the
        // shape of `sched_getparam`'s OUT-param (unistd nr 121, currently
        // undispatched — falls through to the `_` arm's `ENOSYS`), not
        // `sched_setscheduler`'s. In practice this is harmless: `policy` is a
        // tiny int (0-6), so `write_user_val` almost always rejects it as an
        // unmapped address and the `let _ =` discards the error, leaving `0`
        // (success) either way — but the logic itself answers the wrong
        // syscall. Flagged, not fixed, in this pass — naming the constant
        // (rather than the raw `119`) is what made the mismatch visible.
        nr::SCHED_SETSCHEDULER => {
            let param_ptr = args[1] as usize;
            if param_ptr != 0 {
                let zero: i32 = 0;
                let _ = write_user_val(param_ptr as u64, &zero);
            }
            0
        }
        nr::SCHED_YIELD => {
            akuma_exec::threading::yield_now();
            0
        }
        // restart_syscall: kernel-internal mechanism to restart interrupted
        // syscalls after signal delivery. We don't implement SA_RESTART semantics, so the
        // best we can do is return EINTR so callers know to retry the operation.  Returning
        // ENOSYS causes Go's runtime to crash because it doesn't check for ENOSYS here.
        nr::RESTART_SYSCALL => EINTR,
        #[cfg(feature = "sc-sysv-ipc")]
        nr::MSGGET => msgqueue::sys_msgget(args[0] as i32, args[1] as i32),
        #[cfg(feature = "sc-sysv-ipc")]
        nr::MSGCTL => msgqueue::sys_msgctl(args[0] as u32, args[1] as i32, args[2]),
        #[cfg(feature = "sc-sysv-ipc")]
        nr::MSGRCV => msgqueue::sys_msgrcv(args[0] as u32, args[1], args[2] as usize, args[3] as i64, args[4] as i32),
        #[cfg(feature = "sc-sysv-ipc")]
        nr::MSGSND => msgqueue::sys_msgsnd(args[0] as u32, args[1], args[2] as usize, args[3] as i32),
        nr::SCHED_GETAFFINITY => {
            let mask_ptr = args[2] as usize;
            let cpusetsize = args[1] as usize;
            if cpusetsize >= 8 && validate_user_ptr(mask_ptr as u64, cpusetsize) {
                let mut kernel_mask = alloc::vec![0u8; cpusetsize];
                // CPUs the process may run on. On the real shared-kernel SMP
                // build that is the DTB-reported core count (BSP + secondaries,
                // all online after `bringup_secondaries`); single-core builds
                // report 1. The old code hardcoded `1`, so `busybox nproc` and
                // cargo's `num_cpus` always saw one CPU and `cargo build`
                // defaulted to `-j1` even on an SMP=2+ kernel.
                #[cfg(kernel_smp_shared)]
                let nr_cpus: usize = crate::smp_shared::probed_core_count();
                #[cfg(not(kernel_smp_shared))]
                let nr_cpus: usize = 1;
                let mask: u64 = if nr_cpus >= 64 { u64::MAX } else { (1u64 << nr_cpus).wrapping_sub(1) };
                unsafe { core::ptr::write(kernel_mask.as_mut_ptr().cast::<u64>(), mask); }
                let _ = copy_to_user(mask_ptr as u64, &kernel_mask);
                // Linux returns the number of bytes placed in the mask, and
                // musl's `sched_getaffinity` wrapper zeroes the remainder based
                // on this count (`if (r < size) memset(mask+r, 0, size-r)`).
                // Returning 0 made musl wipe the whole buffer, so `busybox
                // nproc`/cargo saw 0 CPUs and fell back to 1. The mask fits in
                // one u64 (≤64 CPUs; Akuma's SMP scope), so we wrote 8 bytes.
                cpusetsize.min(8) as u64
            } else {
                0
            }
        }
        nr::TKILL => signal::sys_tkill(args[0] as u32, args[1] as u32),
        nr::TGKILL => signal::sys_tgkill(args[0] as u32, args[1] as u32, args[2] as u32),
        #[cfg(feature = "sc-pidfd")]
        nr::PIDFD_OPEN => pidfd::sys_pidfd_open(args[0] as u32, args[1] as u32),
        nr::CLOSE_RANGE => {
            fs::sys_close_range(args[0] as u32, args[1] as u32, args[2] as u32)
        }
        nr::CAPGET => proc::sys_capget(args[0], args[1]),
        // Accepting no-ops — see the `CAPSET` doc comment for why "success" here
        // means "not implemented", not "privileges dropped".
        nr::CAPSET
        | nr::SETGID
        | nr::SETUID
        | nr::SETRESUID
        | nr::SETRESGID
        | nr::SETGROUPS => 0,
        nr::SYSINFO => proc::sys_sysinfo(args[0] as usize),
        nr::CLOCK_GETRES => time::sys_clock_getres(args[0] as u32, args[1] as usize),
        #[cfg(feature = "sc-epoll")]
        nr::EPOLL_CREATE1 => poll::sys_epoll_create1(args[0] as u32),
        #[cfg(feature = "sc-epoll")]
        nr::EPOLL_CTL => poll::sys_epoll_ctl(args[0] as u32, args[1] as i32, args[2] as u32, args[3] as usize),
        #[cfg(feature = "sc-epoll")]
        nr::EPOLL_PWAIT => {
            if crate::config::SYSCALL_DEBUG_NET_ENABLED && (args[4] != 0 || args[5] != 0) {
                crate::safe_print!(128, "[epoll_pwait] sigmask=0x{:x} sigsetsize={}\n", args[4], args[5]);
            }
            poll::sys_epoll_pwait(args[0] as u32, args[1] as usize, args[2] as i32, args[3] as i32)
        }
        #[cfg(feature = "sc-timerfd")]
        nr::TIMERFD_CREATE => timerfd::sys_timerfd_create(args[0] as i32, args[1] as i32),
        #[cfg(feature = "sc-timerfd")]
        nr::TIMERFD_SETTIME => timerfd::sys_timerfd_settime(args[0] as u32, args[1] as i32, args[2] as usize, args[3] as usize),
        #[cfg(feature = "sc-timerfd")]
        nr::TIMERFD_GETTIME => timerfd::sys_timerfd_gettime(args[0], args[1]),
        nr::IO_URING_SETUP | nr::IO_URING_ENTER | nr::IO_URING_REGISTER => {
            // `io_uring_enter` is a *loop* call in any runtime that gets as far
            // as trying it, so this one especially must not print per call.
            if crate::config::SYSCALL_ENOSYS_DIAG {
                crate::tprint!(96, "[ENOSYS] nr={} (io_uring) pid={}\n", syscall_num,
                    akuma_exec::process::read_current_pid().unwrap_or(0));
            }
            ENOSYS
        }
        #[cfg(feature = "sc-aio")]
        nr::IO_SETUP => aio::sys_io_setup(args[0], args[1]),
        #[cfg(feature = "sc-aio")]
        nr::IO_DESTROY => aio::sys_io_destroy(args[0]),
        #[cfg(feature = "sc-aio")]
        nr::IO_SUBMIT => aio::sys_io_submit(args[0], args[1] as i64, args[2]),
        #[cfg(feature = "sc-aio")]
        nr::IO_CANCEL => aio::sys_io_cancel(args[0], args[1], args[2]),
        #[cfg(feature = "sc-aio")]
        nr::IO_GETEVENTS => aio::sys_io_getevents(args[0], args[1] as i64, args[2] as i64, args[3], args[4]),
        // Extended attributes syscalls (setxattr..fremovexattr, 5-16) - return
        // EOPNOTSUPP (95) on Linux AArch64. Must be encoded as `x0 = -95`
        // (0xffffffa1), never `!95` which is `-96` (0xffffffa0 = EPFNOSUPPORT)
        // and breaks musl/Go callers. Pinned by
        // `errno::tests::eopnotsupp_encodes_as_negation_not_complement`.
        nr::SETXATTR..=nr::FREMOVEXATTR => {
            // setxattr, lsetxattr, fsetxattr, getxattr, lgetxattr, fgetxattr
            // listxattr, llistxattr, flistxattr, removexattr, lremovexattr, fremovexattr
            EOPNOTSUPP
        }
        nr::INOTIFY_INIT1 | nr::INOTIFY_ADD_WATCH | nr::INOTIFY_RM_WATCH => {
            if crate::config::SYSCALL_ENOSYS_DIAG {
                crate::tprint!(128, "[ENOSYS] nr={} (inotify) pid={}\n", syscall_num,
                    akuma_exec::process::read_current_pid().unwrap_or(0));
            }
            ENOSYS
        }
        #[cfg(feature = "sc-containers")]
        nr::MOUNT => container::sys_mount(args[0], args[1], args[2], args[3], args[4]),
        #[cfg(feature = "sc-containers")]
        nr::UMOUNT2 => container::sys_umount2(args[0], args[1] as i32),
        #[cfg(feature = "sc-containers")]
        nr::MOUNT_IN_NS => container::sys_mount_in_ns(args[0], args[1], args[2] as usize, args[3], args[4] as usize, args[5]),
        _ => {
            if crate::config::SYSCALL_ENOSYS_DIAG {
                crate::safe_print!(128,
                    "[ENOSYS] nr={} pid={} args=[0x{:x}, 0x{:x}, 0x{:x}]\n",
                    syscall_num, akuma_exec::process::read_current_pid().unwrap_or(0),
                    args[0], args[1], args[2]);
            }
            ENOSYS
        }
        },
    };
    crate::syscall::utils::read_profile::floor_laps::lap(
        crate::syscall::utils::read_profile::F_LAP_DISP,
    );

    let epi = excursion.epilogue(u64::from(owner_pid), result == EFAULT);

    // MEASUREMENT ONLY. `cur` is the PROLOGUE's resolution, and the dispatch
    // above can be open-ended (ppoll/futex/blocking read). `kill_thread_group`
    // retires a still-blocked sibling's `Process` and only *then* wakes it, and
    // every secondary core's idle loop drains retired processes once the 10 ms
    // cooldown expires (src/smp_shared.rs), so the prologue's `Process` may
    // already be freed by the time we get here. These counters observe that
    // window directly; the fix for it is `epi.identity` below.
    // See docs/archive/IDENTITY_CACHE_SMP_REVIEW.md Finding A.
    if epi.audit_identity && let Some((audit_pid, audit_proc)) = cur {
        match akuma_exec::process::lookup_process_shared(audit_pid) {
            None => {
                akuma_exec::process::table::EPILOGUE_STALE_IDENTITY
                    .fetch_add(1, Ordering::Relaxed);
            }
            Some(live) if !core::ptr::eq(live, audit_proc) => {
                akuma_exec::process::table::EPILOGUE_IDENTITY_MOVED
                    .fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    // The identity every `Process` write below goes through. Under
    // `IdentitySource::Reresolve` this is a fresh read of the per-thread
    // identity cache, which re-validates the slot state AND its reuse
    // generation — so a process retired during the dispatch yields `None` and
    // every write below is skipped, exactly as the pre-cache epilogue's
    // `lookup_process_shared` returning `None` did. Two loads, against the
    // lock + map walk + IRQ-masked table scan that guard used to cost twice.
    //
    // Under `IdentitySource::Prologue` it is the pointer from before the
    // dispatch, which `akuma_syscalls::slot` proves is a use-after-free two
    // peer operations deep. See `EPILOGUE_IDENTITY`.
    let epi_cur = if !epi.clear_current_syscall && !epi.record_time && !epi.log {
        // A leaf: nothing below writes through a `Process`, so resolving one
        // would be the work this tier exists to skip.
        None
    } else {
        match epi.identity {
            akuma_syscalls::IdentitySource::Prologue => cur,
            akuma_syscalls::IdentitySource::Reresolve => {
                akuma_exec::process::table::current_thread_tgid_process()
            }
        }
    };

    akuma_exec::threading::set_thread_current_syscall(!0u64);
    if epi.clear_current_syscall && let Some((_, proc)) = epi_cur {
        proc.current_syscall.store(!0u64, Ordering::Relaxed);
    }
    crate::syscall::utils::read_profile::floor_laps::lap(
        crate::syscall::utils::read_profile::F_LAP_EPI1,
    );

    if plan.need_timing {
        let elapsed = crate::timer::uptime_us().saturating_sub(t0);
        // `epi_cur`, not `cur`: these dereference a `Process`, so they take the
        // validated identity. `owner_pid` stays the prologue's — it is a scalar
        // copy and the ring is keyed by it, so a process that retired mid-call
        // still files its last entry under the pid it had, with `box_id` 0.
        if epi.record_time && let Some((_, p)) = epi_cur {
            p.syscall_stats.add_time_us(syscall_num, elapsed);
        }
        if epi.log {
            let box_id = epi_cur.map_or(0, |(_, p)| p.box_id);
            log::record(owner_pid, box_id, syscall_num, t0, elapsed, result);
        }
    }
    crate::syscall::utils::read_profile::floor_laps::lap(
        crate::syscall::utils::read_profile::F_LAP_EPI2,
    );

    // Log when a syscall returns a dangerous negative error code.  Go's runtime may
    // not check the error and dereference the negative return value as a pointer,
    // causing a WILD-DA crash (FAR = the error code).
    // TEMP DEBUG nca-build EFAULT: EINVAL floods readlinkat probes during cargo builds.
    if epi.errno_diag {
        let owner_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
        let err_name = if result == EFAULT { "EFAULT" } else if result == ENOSYS { "ENOSYS" } else { "EINVAL" };

        if crate::config::SYSCALL_ERRNO_DIAG_EXTRA {
            let tid = akuma_exec::threading::current_thread_id();
            let elr = akuma_exec::threading::current_trap_frame_elr();
            crate::safe_print!(192,
                "[{}] nr={} pid={} tid={} ELR={} args=[{:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}]\n",
                err_name, syscall_num, owner_pid, tid,
                ElrFmt(elr),
                args[0], args[1], args[2], args[3], args[4], args[5]);

            // mmap-specific decode: the §E investigation (docs/GO_FORKTEST_DEBUG.md)
            // hinges on knowing whether MAP_FIXED is set, whether `len` is zero, and
            // whether the address would overlap the kernel identity map.
            if syscall_num == nr::MMAP && result == EINVAL {
                let addr = args[0] as usize;
                let len = args[1] as usize;
                let prot = args[2] as u32;
                let flags = args[3] as u32;
                let reason = if len == 0 {
                    "len==0"
                } else if mem::mmap_fixed_addr_unaligned_einval(addr, flags) {
                    "fixed+unaligned"
                } else if (flags & mem::MAP_FIXED) != 0
                    && addr != 0
                    && mem::mmap_fixed_overlaps_kernel_va(addr, len)
                {
                    "kernel_va"
                } else {
                    "other"
                };
                crate::safe_print!(192,
                    "  [mmap-einval] reason={} addr={:#x} len={:#x} prot={:#x} flags={:#x}({})\n",
                    reason, addr, len, prot, flags, MmapFlagsFmt(flags));
            }
        } else {
            crate::safe_print!(128,
                "[{}] nr={} pid={} args=[{:#x}, {:#x}, {:#x}, {:#x}]\n",
                err_name, syscall_num, owner_pid, args[0], args[1], args[2], args[3]);
        }
    }

    CURRENT_SYSCALL_NR.store(!0u64, Ordering::Relaxed);
    rp_span.end_handle_syscall(syscall_num);
    result
}

/// `Display` shim for an optional ELR value: prints `0x…` or `?`.
struct ElrFmt(Option<u64>);
impl core::fmt::Display for ElrFmt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(v) => write!(f, "{v:#x}"),
            None => f.write_str("?"),
        }
    }
}

/// `Display` shim for an mmap `flags` bitmask: prints a compact decode like
/// `FIXED|PRIVATE|ANON`. Empty mask renders as `0`.
struct MmapFlagsFmt(u32);
impl core::fmt::Display for MmapFlagsFmt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let flags = self.0;
        let entries: [(u32, &'static str); 8] = [
            (mem::MAP_SHARED, "SHARED"),
            (mem::MAP_PRIVATE, "PRIVATE"),
            (mem::MAP_FIXED, "FIXED"),
            (mem::MAP_ANONYMOUS, "ANON"),
            (mem::MAP_NORESERVE, "NORESERVE"),
            (mem::MAP_POPULATE, "POPULATE"),
            (mem::MAP_STACK, "STACK"),
            (mem::MAP_FIXED_NOREPLACE, "FIXED_NOREPLACE"),
        ];
        let mut first = true;
        for (bit, name) in &entries {
            if flags & *bit != 0 {
                if !first { f.write_str("|")?; }
                f.write_str(name)?;
                first = false;
            }
        }
        if first { f.write_str("0")?; }
        Ok(())
    }
}
