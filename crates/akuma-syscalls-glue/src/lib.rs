//! The Linux syscall dispatcher and its per-family implementations.
//!
//! Syscall number in `x8`, arguments in `x0`-`x5`. `akuma-exceptions` reaches
//! `handle_syscall` through `ExceptionHooks`, so the SVC path does not name this
//! crate at all.
//!
//! # Why it is a crate, as of 2026-09-01
//!
//! It was `src/syscall/` — 23 files, ~17k lines, the largest thing left in the
//! binary and the one 542 of the boot suite's `crate::` references pointed at.
//! Four things had to move first, none of them syscall code:
//!
//! | blocker | resolution |
//! |---|---|
//! | `src/vfs/` cycle (110 refs out, 10 back) | `akuma-vfs-glue`, after `-log`/`-ipc` cut the back-edge |
//! | `crate::config` (217 refs) | `akuma-config`, a crate of `const`s — **not** a handover struct |
//! | `crate::process_tests::make_test_process` | moved to `akuma-exec::process` |
//! | `crate::fs`, `crate::pmm` wrappers | `akuma-vfs-glue::fs`, `akuma-exec::pmm` |
//!
//! What was left needed **seven** function pointers ([`SyscallHooks`]).
//! `docs/archive/SRC_SYSCALL_EXTRACTION.md`.
//!
//! # This module forbids `unsafe`
//!
//! `src/syscall/` went from 17 `unsafe` blocks to 0 on 2026-08-31, and the ban
//! below is what keeps it there. The bin crate as a whole can never be
//! `forbid`-enforced — `exceptions.rs` alone has 87 sites, and page-table and
//! trap-frame work is the job — but *this* subtree is the one that runs with
//! userspace-controlled arguments on every call, so it is the subtree where a
//! stray `unsafe` is worth a compile error.
//!
//! `forbid`, not `deny`: `deny` can be switched back off by a module-local
//! `#[allow(unsafe_code)]`, which is exactly the move that would erode this.
//!
//! **What the ban does and does not mean.** It means no `unsafe` is written
//! *here*. It does not mean the syscall layer is proven sound: the operations
//! that were genuinely unsafe still are, they now live behind named functions in
//! the crate that owns the thing being poked, where the obligation is stated once
//! and discharged once instead of at each call site:
//!
//! | was, here | is, there |
//! |---|---|
//! | `copy_from_user_safe` byte loop | `akuma_exec::process::user_access::read_user_byte` |
//! | `map_user_page` + hand-rolled frame tracking | `UserAddressSpace::map_user_page_tracked` |
//! | `phys_to_virt` + `slice::from_raw_parts` | `akuma_mmu::copy_from_phys` / `copy_to_phys` |
//! | `msr tpidr_el0` | `akuma_cpu::sysreg::set_tpidr_el0` |
//! | `with_process_exclusive(pid, …)` | `akuma_exec::process::with_own_process_exclusive` |
//! | `enter_user_mode` | `akuma_exec::process::enter_user_mode_checked` |
//!
//! Three of those became genuinely checkable in the move (a PMM-range bounds
//! check, an installed-TTBR0 check, an SPSR-targets-EL0 check). One did not:
//! `with_own_process_exclusive` discharges two of its three clauses and rests on
//! the call site staying enumerated for the third — see its doc comment. Adding a
//! second caller of it is a change to that argument.
//!
//! Full record: `docs/archive/SYSCALL_UNSAFE_CLEANUP.md`.
#![no_std]
#![forbid(unsafe_code)]
// ---- lints ----------------------------------------------------------------
// The first three were already allowed by `src/main.rs` on this exact code; they
// move with it. Nothing is being newly suppressed:
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::cast_possible_wrap)] // a syscall return *is* a reinterpreted u64
#![allow(clippy::inline_always)] // used for hot syscall paths
//
// The next two only fire because this is a library now, not a module tree in a
// binary, and neither is describing a defect here:
//
// `redundant_pub_crate` wants `pub` where a `pub(crate)` item sits in a private
// module. Every module below except the handful re-exported for the boot suite
// *is* private, and `pub(crate)` states the real visibility — widening it to
// `pub` to satisfy the lint would make the crate's surface a lie.
#![allow(clippy::redundant_pub_crate)]
// `must_use_candidate` wants `#[must_use]` on ~40 syscall implementations. The
// dispatcher is their only caller and consumes every return, so there is no
// dropped-value bug class here to catch. Where one genuinely existed — tests
// discarding an errno from `sys_msgctl` — the attribute was added deliberately,
// in `akuma-syscalls-ipc`.
#![allow(clippy::must_use_candidate)]

extern crate alloc;

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
pub use akuma_exec::process::user_access::{
    Prefault, as_user_bytes, as_user_bytes_mut, copy_from_user, copy_to_user,
    copy_to_user_with, read_user_byte, read_user_into, read_user_into_with,
    write_user_val, write_user_val_with,
};
// The excursion shape crate: which counter bucket a number falls in, which
// hooks run, and where the epilogue's identity comes from. Decisions only — see
// `EXCURSION_HOOKS` and docs/archive/AKUMA_EXTRACT_SYSCALLS.md §7.
use akuma_syscalls::Counter;
use akuma_syscalls_linux::proc::{CPU_SET_BITS_PER_WORD, CPU_SET_WORD_BYTES};

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
/// AF_UNIX sockets — the kernel half.
///
/// The decisions live in `akuma_net_unix` (host-tested); this module holds the
/// one table, the user-pointer copies, and the pipes that carry the bytes. See
/// docs/archive/UNIX_SOCKET_IMPROVEMENTS.md.
pub mod unixsock;
pub mod signal;
mod sync;
/// `akuma_get_version` — the build identity, packed into the return register.
///
/// Also the floor control for the syscall boundary. See the module docs.
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
#[cfg(kernel_tests)]
pub use mem::{MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE};


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
        akuma_primitives::safe_print!(512,
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
        akuma_primitives::safe_print!(512,
            "[SC-STATS] futex={} sigmask={} sigact={} clk={} ioctl={} fstat={} yield={}\n",
            FUTEX_COUNT.load(Ordering::Relaxed),
            SIGPROCMASK_COUNT.load(Ordering::Relaxed),
            SIGACTION_COUNT.load(Ordering::Relaxed),
            CLOCK_COUNT.load(Ordering::Relaxed),
            IOCTL_COUNT.load(Ordering::Relaxed),
            FSTAT_COUNT.load(Ordering::Relaxed),
            YIELD_COUNT.load(Ordering::Relaxed),
        );
        akuma_primitives::safe_print!(384,
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
/// Re-export: the flag moved to `akuma_exec::process::user_access` with the check it
/// disables, so the copy helpers can honour it. The ~50 boot-test call sites keep
/// spelling it `crate::BYPASS_VALIDATION`.
pub use akuma_exec::process::user_access::BYPASS_VALIDATION;

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
    /// Wire padding. `pub` because the struct is, `_`-prefixed because nothing
    /// reads it — it exists so the `repr(C)` layout matches what userspace
    /// expects. Clippy's `pub_underscore_fields` wants one or the other; the ABI
    /// wants both.
    #[allow(clippy::pub_underscore_fields)]
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

/// A syscall arm that reports failure through `Err` instead of a bare `-errno`.
///
/// `Ok` carries the success value, `Err` a **negated** errno — the same
/// convention the plain-`u64` arms return, just typed. [`flat`] collapses the
/// two back into `x0`.
pub type SysResult = Result<u64, u64>;

/// Collapse a [`SysResult`] into the raw `x0` value the dispatch returns.
///
/// This is a type change, not a conversion: both halves are already in the
/// ABI's return convention, so `Ok(v)` and `Err(v)` produce the same `x0`.
/// That property is why converting an arm to `SysResult` cannot change
/// behaviour even if a value is classified into the "wrong" variant — the
/// worst case is a misleading type, not a different syscall result.
///
/// # The sign trap this exists next to
///
/// **Two families of `Result<_, u64>` meet in this module and their `Err`
/// values have opposite signs.**
///
/// - This module's own helpers — [`copy_from_user_str`], [`copy_from_user_byte`]
///   — carry the **negated** form, because they are written for syscall arms.
/// - `akuma_exec::process::user_access`'s helpers (`copy_from_user`,
///   `read_user_into`, `write_user_val`, …) carry the **positive** form. That
///   is deliberate and documented at its definition: *"`x0 = -errno` happens at
///   the syscall boundary, not here"*, because that crate is also used off the
///   syscall path.
///
/// So `read_user_into(&mut ts, ptr)?` inside a `SysResult` arm compiles and is
/// **wrong**: it returns `Err(14)`, and userspace reads a positive 14 as a
/// successful syscall returning 14. Every call site therefore uses
/// `.is_err()` and returns this module's `EFAULT` explicitly. Audited
/// 2026-08-28: zero violations, and `scripts/check_errno_sign.py` keeps it
/// that way — the risk is new, because before `SysResult` there was no `?` in
/// these functions for the mistake to hide in.
#[must_use]
pub const fn flat(r: SysResult) -> u64 {
    match r {
        Ok(v) | Err(v) => v,
    }
}

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
    akuma_exec::process::user_access::prefault_user_range(start, len)
}

/// The 48-bit user VA limit. Now `akuma_exec::process::user_access::USER_VA_LIMIT` —
/// see there for why it is not a smaller cap (Go's high arenas).
fn user_va_limit() -> u64 {
    akuma_exec::process::user_access::USER_VA_LIMIT
}

/// Validate a user pointer range, faulting lazy pages in.
///
/// Thin forwarder: the body — the range tests *and* the demand-paging half — moved
/// to `akuma_exec::process::user_access` so the copy helpers could fold it in and stop
/// being skippable (`docs/archive/UNSAFE_AUDIT.md` §4 P0). Kept as a named function
/// because plenty of syscall arms validate a pointer they never copy through
/// (`futex` addresses, `mmap` args), and because `Prefault::Yes` is the right
/// default for every caller on a syscall stack.
fn validate_user_ptr(ptr: u64, len: usize) -> bool {
    akuma_exec::process::user_access::validate_user_range(
        ptr,
        len,
        akuma_exec::process::user_access::Prefault::Yes,
    )
}

/// Measurement helpers for this layer — see [`utils`].
pub mod utils;

/// Read a single byte from user memory.
///
/// The raw `copy_from_user_safe` this used to wrap moved into
/// [`akuma_exec::process::user_access::read_user_byte`], which is the crate that owns
/// user-memory access — the last `unsafe` block in `src/syscall/` that was about
/// *reaching* user memory rather than editing a page table.
///
/// Kept as a named alias because the name says which caller it exists for:
/// [`copy_from_user_str`], which walks a NUL-terminated string one byte at a time.
/// The length of such a string is not known until it has been read, so there is no
/// range to validate up front; the string reader supplies the bound (its own
/// `max_len` and the VA limit) and the fault trampoline supplies the mapped-ness.
pub fn copy_from_user_byte(ptr: u64) -> Result<u8, u64> {
    read_user_byte(ptr)
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
        if akuma_config::SYSCALL_DEBUG_INFO_ENABLED {
            akuma_primitives::safe_print!(64, "[syscall] copy_from_user_str: invalid UTF-8\n");
        }
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
    process_stats: akuma_config::PROCESS_SYSCALL_STATS,
    proc_log: akuma_config::PROC_SYSCALL_LOG_ENABLED,
    debug_io: akuma_config::SYSCALL_DEBUG_IO_ENABLED,
    errno_diag: akuma_config::SYSCALL_ERRNO_DIAG_ENABLED,
    identity_audit: akuma_config::IDENTITY_AUDIT,
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
    let rp_span = crate::utils::read_profile::Span::new();
    // The generic shape of this excursion: which counter bucket, whether the
    // signal-state clear applies, whether each hook runs, and where the
    // epilogue gets its identity. Decisions only — every effect below is still
    // performed here. All of it is `const fn` over plain data, so it inlines
    // into the branches it replaced rather than adding a call.
    let excursion = akuma_syscalls::Excursion::new(syscall_num, EXCURSION_HOOKS);
    let plan = excursion.prologue();
    CURRENT_SYSCALL_NR.store(syscall_num, Ordering::Relaxed);
    crate::utils::read_profile::floor_laps::start(syscall_num);

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
    crate::utils::read_profile::floor_laps::lap(
        crate::utils::read_profile::F_LAP_IDENT,
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
    crate::utils::read_profile::floor_laps::lap(
        crate::utils::read_profile::F_LAP_INTRPT,
    );


    // The suppression list this gate used to spell inline lives in
    // `akuma_syscalls::debug_io_suppressed`, which is the only place a test can
    // reach it: no shipping profile sets `SYSCALL_DEBUG_IO_ENABLED`, so a number
    // silently added to or dropped from the list changes nothing observable
    // until someone turns the flag on to debug something else.
    if plan.debug_print {
        akuma_primitives::safe_print!(128, "[SC] nr={} a0=0x{:x} a1=0x{:x} a2=0x{:x}\n", syscall_num, args[0], args[1], args[2]);
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
    let t0 = if plan.need_timing { akuma_primitives::clock::uptime_us() } else { 0 };
    crate::utils::read_profile::floor_laps::lap(
        crate::utils::read_profile::F_LAP_HOOKS,
    );

    // For a `stack=rump` box, the proxy intercepts socket-family syscalls (and
    // read/write/close on rump-owned fds) and forwards them to the box's
    // rump_server. `Some(r)` short-circuits the normal smoltcp dispatch with the
    // proxied result; `None` falls through unchanged (the common case, incl. all
    // non-rump boxes — a single relaxed atomic load). It also emits the
    // `[RUMP-SP]` trace.
    #[cfg(feature = "rump")]
    let rump_result = crate::hooks::intercept_box_syscall(syscall_num, args);
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
         nr::STATFS => flat(fs::sys_statfs(args[0], args[1])),
        nr::DUP3 => fs::sys_dup3(args[0] as u32, args[1] as u32, args[2] as u32),
        nr::PIPE2 => pipe::sys_pipe2(args[0], args[1] as u32),
        nr::BRK => mem::sys_brk(args[0] as usize),
        nr::OPENAT => flat(fs::sys_openat(args[0] as i32, args[1], args[2] as u32, args[3] as u32)),
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
        nr::PSELECT6 => flat(poll::sys_pselect6(args[0] as usize, args[1], args[2], args[3], args[4], args[5])),
        nr::PPOLL => flat(poll::sys_ppoll(args[0], args[1] as usize, args[2], args[3])),
        nr::MKDIRAT => flat(fs::sys_mkdirat(args[0] as i32, args[1], args[2] as u32)),
        nr::UNLINKAT => flat(fs::sys_unlinkat(args[0] as i32, args[1], args[2] as u32)),
        nr::SYMLINKAT => flat(fs::sys_symlinkat(args[0], args[1] as i32, args[2])),
        nr::LINKAT => flat(fs::sys_linkat(args[0] as i32, args[1], args[2] as i32, args[3], args[4] as u32)),
        nr::RENAMEAT => flat(fs::sys_renameat(args[0] as i32, args[1], args[2] as i32, args[3])),
        nr::RENAMEAT2 => flat(fs::sys_renameat2(args[0] as i32, args[1], args[2] as i32, args[3], args[4] as u32)),
        nr::STATX => flat(fs::sys_statx(args[0] as i32, args[1], args[2] as u32, args[3] as u32, args[4])),
        nr::READLINKAT => flat(fs::sys_readlinkat(args[0] as i32, args[1], args[2], args[3] as usize)),
        nr::SPAWN => flat(proc::sys_spawn(args[0], args[1], args[2], args[3], args[4] as usize, args[5])),
        nr::KILL => proc::sys_kill(args[0] as u32, args[1] as u32),
        #[cfg(feature = "sc-reboot")]
        nr::REBOOT => reboot::sys_reboot(args[0] as u32, args[1] as u32, args[2] as u32),
        nr::WAITPID => proc::sys_waitpid(args[0] as u32, args[1]),
        nr::GETRANDOM => proc::sys_getrandom(args[0], args[1] as usize),
        nr::TIME => time::sys_time(),
        nr::CHDIR => flat(fs::sys_chdir(args[0])),
        nr::FCHDIR => fs::sys_fchdir(args[0] as u32),
        nr::SET_TERMINAL_ATTRIBUTES => term::sys_set_terminal_attributes(args[0], args[1], args[2]),
        nr::GET_TERMINAL_ATTRIBUTES => term::sys_get_terminal_attributes(args[0], args[1]),
        nr::SET_CURSOR_POSITION => term::sys_set_cursor_position(args[0], args[1]),
        nr::HIDE_CURSOR => term::sys_hide_cursor(),
        nr::SHOW_CURSOR => term::sys_show_cursor(),
        nr::CLEAR_SCREEN => term::sys_clear_screen(),
        nr::POLL_INPUT_EVENT => term::sys_poll_input_event(args[0], args[1] as usize, args[2]),
        nr::GET_CPU_STATS => term::sys_get_cpu_stats(args[0], args[1] as usize),
        nr::SPAWN_EXT => flat(proc::sys_spawn_ext(args[0], args[1], args[2], args[3], args[4], args[5])),
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
        nr::RT_SIGTIMEDWAIT => flat(signal::sys_rt_sigtimedwait(args[0], args[1], args[2], args[3] as usize)),
        nr::RT_SIGRETURN => 0,
        nr::RT_SIGACTION => signal::sys_rt_sigaction(args[0] as u32, args[1] as usize, args[2] as usize, args[3] as usize),
        nr::GETCWD => fs::sys_getcwd(args[0], args[1] as usize),
        nr::FCNTL => fs::sys_fcntl(args[0] as u32, args[1] as u32, args[2]),
        nr::NEWFSTATAT => flat(fs::sys_newfstatat(args[0] as i32, args[1], args[2], args[3] as u32)),
        nr::FACCESSAT => flat(fs::sys_faccessat2(args[0] as i32, args[1], args[2] as u32, 0)),
        nr::CLOCK_GETTIME => time::sys_clock_gettime(args[0], args[1]),
        nr::CLOCK_SETTIME => time::sys_clock_settime(args[0] as u32, args[1]),
        nr::ADJTIMEX => time::sys_adjtimex(args[0]),
        nr::CLOCK_ADJTIME => time::sys_clock_adjtime(args[0] as u32, args[1]),
        nr::FACCESSAT2 => flat(fs::sys_faccessat2(args[0] as i32, args[1], args[2] as u32, args[3] as u32)),
        nr::WAIT4 => proc::sys_wait4(args[0] as i32, args[1], args[2] as i32, args[3]),
        nr::WAITID => proc::sys_waitid(args[0] as u32, args[1] as u32, args[2], args[3] as i32),
        nr::SET_TPIDR_EL0 => proc::sys_set_tpidr_el0(args[0]),
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
        nr::UTIMENSAT => flat(fs::sys_utimensat(args[0] as i32, args[1], args[2], args[3] as u32)),
        nr::FDATASYNC => 0,
        nr::FSYNC => 0,
        nr::FCHMOD => fs::sys_fchmod(args[0] as u32, args[1] as u32),
        nr::FCHMODAT => flat(fs::sys_fchmodat(args[0] as i32, args[1], args[2] as u32)),
        nr::FCHOWNAT => 0,
        nr::FCHOWN => 0,
        nr::TRUNCATE => flat(fs::sys_truncate(args[0], args[1] as i64)),
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
            if akuma_config::SYSCALL_ENOSYS_DIAG {
                akuma_primitives::tprint!(96, "[ENOSYS] nr=270 (process_vm_readv) pid={}\n",
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
            if cpusetsize >= CPU_SET_WORD_BYTES && validate_user_ptr(mask_ptr as u64, cpusetsize) {
                let mut kernel_mask = alloc::vec![0u8; cpusetsize];
                // CPUs the process may run on. On the real shared-kernel SMP
                // build that is the DTB-reported core count (BSP + secondaries,
                // all online after `bringup_secondaries`); single-core builds
                // report 1. The old code hardcoded `1`, so `busybox nproc` and
                // cargo's `num_cpus` always saw one CPU and `cargo build`
                // defaulted to `-j1` even on an SMP=2+ kernel.
                #[cfg(kernel_smp_shared)]
                let nr_cpus: usize = crate::hooks::probed_core_count();
                #[cfg(not(kernel_smp_shared))]
                let nr_cpus: usize = 1;
                let mask: u64 = if nr_cpus >= CPU_SET_BITS_PER_WORD {
                    u64::MAX
                } else {
                    (1u64 << nr_cpus).wrapping_sub(1)
                };
                // `kernel_mask` is a `Vec<u8>` (1-aligned), so the old
                // `ptr::write` through a `cast::<u64>()` was an aligned write to an
                // unaligned pointer. The `>= CPU_SET_WORD_BYTES` guard above is what
                // makes this slice in bounds.
                kernel_mask[..CPU_SET_WORD_BYTES].copy_from_slice(&mask.to_ne_bytes());
                let _ = copy_to_user(mask_ptr as u64, &kernel_mask);
                // Linux returns the number of bytes placed in the mask, and
                // musl's `sched_getaffinity` wrapper zeroes the remainder based
                // on this count (`if (r < size) memset(mask+r, 0, size-r)`).
                // Returning 0 made musl wipe the whole buffer, so `busybox
                // nproc`/cargo saw 0 CPUs and fell back to 1. The mask fits in
                // one u64 (≤64 CPUs; Akuma's SMP scope), so we wrote 8 bytes.
                cpusetsize.min(CPU_SET_WORD_BYTES) as u64
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
            if akuma_config::SYSCALL_DEBUG_NET_ENABLED && (args[4] != 0 || args[5] != 0) {
                akuma_primitives::safe_print!(128, "[epoll_pwait] sigmask=0x{:x} sigsetsize={}\n", args[4], args[5]);
            }
            poll::sys_epoll_pwait(args[0] as u32, args[1] as usize, args[2] as i32, args[3] as i32)
        }
        #[cfg(feature = "sc-timerfd")]
        nr::TIMERFD_CREATE => timerfd::sys_timerfd_create(args[0] as i32, args[1] as i32),
        #[cfg(feature = "sc-timerfd")]
        nr::TIMERFD_SETTIME => flat(timerfd::sys_timerfd_settime(args[0] as u32, args[1] as i32, args[2] as usize, args[3] as usize)),
        #[cfg(feature = "sc-timerfd")]
        nr::TIMERFD_GETTIME => timerfd::sys_timerfd_gettime(args[0], args[1]),
        nr::IO_URING_SETUP | nr::IO_URING_ENTER | nr::IO_URING_REGISTER => {
            // `io_uring_enter` is a *loop* call in any runtime that gets as far
            // as trying it, so this one especially must not print per call.
            if akuma_config::SYSCALL_ENOSYS_DIAG {
                akuma_primitives::tprint!(96, "[ENOSYS] nr={} (io_uring) pid={}\n", syscall_num,
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
            if akuma_config::SYSCALL_ENOSYS_DIAG {
                akuma_primitives::tprint!(128, "[ENOSYS] nr={} (inotify) pid={}\n", syscall_num,
                    akuma_exec::process::read_current_pid().unwrap_or(0));
            }
            ENOSYS
        }
        #[cfg(feature = "sc-containers")]
        nr::MOUNT => flat(container::sys_mount(args[0], args[1], args[2], args[3], args[4])),
        #[cfg(feature = "sc-containers")]
        nr::UMOUNT2 => flat(container::sys_umount2(args[0], args[1] as i32)),
        #[cfg(feature = "sc-containers")]
        nr::MOUNT_IN_NS => flat(container::sys_mount_in_ns(args[0], args[1], args[2] as usize, args[3], args[4] as usize, args[5])),
        _ => {
            if akuma_config::SYSCALL_ENOSYS_DIAG {
                akuma_primitives::safe_print!(128,
                    "[ENOSYS] nr={} pid={} args=[0x{:x}, 0x{:x}, 0x{:x}]\n",
                    syscall_num, akuma_exec::process::read_current_pid().unwrap_or(0),
                    args[0], args[1], args[2]);
            }
            ENOSYS
        }
        },
    };
    crate::utils::read_profile::floor_laps::lap(
        crate::utils::read_profile::F_LAP_DISP,
    );

    // The errno set the diagnostic below covers. It was `result == EFAULT` alone
    // — a cost workaround for an `EINVAL` flood from `readlinkat` probes during
    // cargo builds — and that narrowing silently made the block's `ENOSYS` and
    // `EINVAL` names, and its whole `mmap`-EINVAL decode, UNREACHABLE: `result`
    // could only ever be `EFAULT` inside it. The cost that forced the workaround
    // is gone (`SYSCALL_ERRNO_DIAG_ENABLED` is off by default now, measured at
    // ~250 µs per line — docs/archive/CONSOLE_LOG_COST.md), so the gate matches
    // what the code below claims to handle again. Turning the flag on now costs
    // the flood as well as the cost; that is the caller's trade, made knowingly.
    let epi = excursion.epilogue(
        u64::from(owner_pid),
        result == EFAULT || result == ENOSYS || result == EINVAL,
    );

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
    crate::utils::read_profile::floor_laps::lap(
        crate::utils::read_profile::F_LAP_EPI1,
    );

    if plan.need_timing {
        let elapsed = akuma_primitives::clock::uptime_us().saturating_sub(t0);
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
    crate::utils::read_profile::floor_laps::lap(
        crate::utils::read_profile::F_LAP_EPI2,
    );

    // Log when a syscall returns a dangerous negative error code.  Go's runtime may
    // not check the error and dereference the negative return value as a pointer,
    // causing a WILD-DA crash (FAR = the error code).
    //
    // Reachable for EFAULT, ENOSYS and EINVAL — see the `epilogue()` call above
    // for why that had silently collapsed to EFAULT alone, and what it cost.
    if epi.errno_diag {
        let owner_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
        let err_name = if result == EFAULT { "EFAULT" } else if result == ENOSYS { "ENOSYS" } else { "EINVAL" };

        if akuma_config::SYSCALL_ERRNO_DIAG_EXTRA {
            let tid = akuma_exec::threading::current_thread_id();
            let elr = akuma_exec::threading::current_trap_frame_elr();
            akuma_primitives::safe_print!(192,
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
                akuma_primitives::safe_print!(192,
                    "  [mmap-einval] reason={} addr={:#x} len={:#x} prot={:#x} flags={:#x}({})\n",
                    reason, addr, len, prot, flags, MmapFlagsFmt(flags));
            }
        } else {
            akuma_primitives::safe_print!(128,
                "[{}] nr={} pid={} args=[{:#x}, {:#x}, {:#x}, {:#x}]\n",
                err_name, syscall_num, owner_pid, args[0], args[1], args[2], args[3]);
        }
    }

    CURRENT_SYSCALL_NR.store(!0u64, Ordering::Relaxed);
    rp_span.end_handle_syscall(syscall_num);
    result
}

// The kernel's trace const and `akuma-syscalls`' `debug-info` feature must be the
// same answer. If they diverge, `FastPath::Leaf` becomes a lie: the prologue would
// skip the identity read for an AIO stub that then reads `ctx`, formats it and
// takes `AIO_CONTEXTS.lock()`. Both derive from the `syscall-debug-info` feature,
// so this can only fire if someone hand-edits one of them — which is the point.
const _: () = assert!(
    akuma_config::SYSCALL_DEBUG_INFO_ENABLED == akuma_syscalls::DEBUG_INFO,
    "config::SYSCALL_DEBUG_INFO_ENABLED disagrees with akuma-syscalls' debug-info \
     feature; FastPath::Leaf membership would be wrong for the AIO stubs"
);

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

// ===========================================================================
// What this crate still needs from the binary
// ===========================================================================

/// The seven things the syscall layer cannot reach from a crate.
///
/// Deliberately small, and it stayed small because each candidate was *resolved*
/// before a hook was written for it — `crate::audio` turned out to be a
/// re-export of `akuma_virtio::audio`, `crate::fs` and `crate::pmm` moved into
/// the crates that own what they wrap, and `crate::config` became a crate of
/// `const`s. What is left is genuinely binary-local: the rump sysproxy (whose
/// state lives in `src/rump_proxy.rs`), the wall clock (which needs the binary's
/// boot uptime to turn monotonic microseconds into UTC), and the DTB-probed core
/// count.
///
/// Unregistered is quiet, not fatal: every accessor below has a defined answer
/// with no hooks installed, so host tests and early boot get coherent behaviour
/// rather than a panic. Same contract as `akuma_primitives::console::print_str`.
#[derive(Clone, Copy)]
pub struct SyscallHooks {
    /// `rump_proxy::box_is_rump` — is this box served by a rump kernel?
    pub box_is_rump: fn(u64) -> bool,
    /// `rump_proxy::mark_box_rump`
    pub mark_box_rump: fn(u64),
    /// `rump_proxy::attach_server` — bind a box to its rump server process.
    pub attach_server: fn(u64, akuma_exec::process::Pid),
    /// `rump_proxy::intercept_box_syscall` — forward a call to the rump server.
    pub intercept_box_syscall: fn(u64, &[u64; 6]) -> Option<u64>,
    /// `rump_proxy::rump_socket_readable`
    pub rump_socket_readable: fn(i32) -> bool,
    /// `timer::utc_time_us` — wall clock, `None` before NTP/RTC sets it.
    pub utc_time_us: fn() -> Option<u64>,
    /// `smp_shared::probed_core_count` — DTB-probed core count.
    pub probed_core_count: fn() -> usize,
}

static HOOKS: akuma_primitives::OnceCopy<SyscallHooks> = akuma_primitives::OnceCopy::new();

/// Install the binary's callbacks. Idempotent, per `OnceCopy`.
pub fn set_hooks(h: SyscallHooks) {
    HOOKS.set(h);
}

/// Which of these has a caller depends on the feature set — the rump five need
/// `rump`, `probed_core_count` needs `smp-shared` — so the parity surface is
/// deliberately wider than any single configuration uses. Same argument the
/// virtio-audio stub carried: the syscall layer compiles unchanged either way.
#[allow(dead_code)]
pub(crate) mod hooks {
    use super::HOOKS;

    pub fn box_is_rump(box_id: u64) -> bool {
        HOOKS.get().is_some_and(|h| (h.box_is_rump)(box_id))
    }
    pub fn mark_box_rump(box_id: u64) {
        if let Some(h) = HOOKS.get() {
            (h.mark_box_rump)(box_id);
        }
    }
    pub fn attach_server(box_id: u64, server_pid: akuma_exec::process::Pid) {
        if let Some(h) = HOOKS.get() {
            (h.attach_server)(box_id, server_pid);
        }
    }
    pub fn intercept_box_syscall(syscall_num: u64, args: &[u64; 6]) -> Option<u64> {
        HOOKS.get().and_then(|h| (h.intercept_box_syscall)(syscall_num, args))
    }
    pub fn rump_socket_readable(rump_fd: i32) -> bool {
        HOOKS.get().is_some_and(|h| (h.rump_socket_readable)(rump_fd))
    }
    pub fn utc_time_us() -> Option<u64> {
        HOOKS.get().and_then(|h| (h.utc_time_us)())
    }
    pub fn probed_core_count() -> usize {
        HOOKS.get().map_or(1, |h| (h.probed_core_count)())
    }
}
