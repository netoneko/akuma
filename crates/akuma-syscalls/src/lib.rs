// This crate is unsafe-free by design and stays that way: `forbid` (not
// `deny`) so no module can opt back in with a local `allow`. Same reasoning as
// `akuma-net-yarn`, and the same reason it is spelled here rather than in
// Cargo.toml — `[lints] workspace = true` and a crate-local `[lints]` table are
// mutually exclusive.
#![forbid(unsafe_code)]
//! The **shape** of a syscall excursion, extracted as decisions with the
//! effects left in the kernel.
//!
//! Crate 2 of the pair proposed in
//! [`AKUMA_EXTRACT_SYSCALLS.md`](../../../docs/archive/AKUMA_EXTRACT_SYSCALLS.md).
//! Crate 1 (`akuma-syscalls-linux`) is the ABI: numbers, flags, `repr(C)`
//! structs. This one is the **generic part of an excursion** — everything
//! `handle_syscall` does around the family dispatch, and nothing it dispatches
//! *to*.
//!
//! # What is here, and what is emphatically not
//!
//! Here: which counter bucket a number falls in, whether an excursion clears
//! the delivered-signal record, whether the debug-IO print is suppressed for
//! this number, which epilogue hooks run, and — the one that matters —
//! **which resolution of "who am I" the epilogue is allowed to write through**
//! ([`IdentitySource`]).
//!
//! Not here: the 16.5 k lines of `src/syscall/`. §7 of the proposal is explicit
//! about why. A crate holding the family implementations would depend on vfs,
//! ext2, net, exec, mm, pmm and terminal — a second kernel, whose tests need
//! all of that mocked. Families move out one at a time on the `akuma-time`
//! model, when a family has real pure logic worth testing.
//!
//! # The shape it follows: decisions, not injected effects
//!
//! `akuma-net-yarn` is the template, and the important thing about that
//! template is what it does *not* do: there is no `trait Effects`, no generic
//! parameter, no `dyn`. The caller performs the effects and calls pure methods
//! between them. That is not a stylistic choice here — it is the only shape
//! that survives the hot path.
//!
//! `handle_syscall` is the hottest function in the kernel; the audit took it
//! from 410 ns to 150 ns and §7 records the risk in one line: *an extraction
//! that adds an indirect call to the dispatch would eat the entire win.*
//! Everything public here is a plain-data struct or a `const fn` returning a
//! C-like enum, so the whole crate inlines into the caller and compiles away
//! into the same branches it replaced.
//!
//! Measured on this tree, `SMP=1`, `read_syscall_cost … 2000 5`, best of
//! 100 × 100: `getpid` **230 ns before, 230 ns after**; the `read-profile`
//! `wrap` control **167 ns before, 167 ns after** — see
//! `AKUMA_EXTRACT_SYSCALLS.md` §7.
//!
//! # Why an excursion is a state machine at all
//!
//! Because five values are computed in the prologue and consumed after an
//! open-ended dispatch — `cur`, `owner_pid`, `track_time`, `need_timing`,
//! `t0` — and *one of them is a pointer whose target can be freed while the
//! dispatch runs*. Carrying them as loose locals is what made that a hoisting
//! bug nobody had to name
//! ([`IDENTITY_CACHE_SMP_REVIEW.md`](../../../docs/archive/IDENTITY_CACHE_SMP_REVIEW.md)
//! Finding A). [`Excursion`] carries them as one value across the dispatch, and
//! makes the pointer question a named policy field with an answer this crate
//! can *prove* — see [`slot`].

#![cfg_attr(not(test), no_std)]

use akuma_syscalls_linux::nr;

pub mod slot;

/// The build's diagnostic gates, as data.
///
/// The kernel passes `src/config.rs`'s consts in; the tests pass whatever they
/// like. Taking them as a parameter rather than reading them is what makes the
/// gate combinations enumerable on the host — `PROCESS_SYSCALL_STATS` and
/// `PROC_SYSCALL_LOG_ENABLED` are `true` in every profile except
/// `kernel_profile_extreme`, so the off-arm is otherwise only reachable by
/// building a different kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one field per `src/config.rs` const, deliberately one-for-one: \
              this struct's job is to be the gates as data, and folding them \
              into enums would break the mapping a reader checks it against"
)]
pub struct HookConfig {
    /// `config::PROCESS_SYSCALL_STATS` — per-process `syscall_stats` counters
    /// and their `add_time_us` in the epilogue.
    pub process_stats: bool,
    /// `config::PROC_SYSCALL_LOG_ENABLED` — the `/proc/<pid>/syscalls` ring.
    pub proc_log: bool,
    /// `config::SYSCALL_DEBUG_IO_ENABLED` — the per-call `[SC] nr=…` print.
    pub debug_io: bool,
    /// `config::SYSCALL_ERRNO_DIAG_ENABLED` — the `[EFAULT] …` epilogue print.
    pub errno_diag: bool,
    /// `config::IDENTITY_AUDIT` — the epilogue's stale/moved identity counters.
    pub identity_audit: bool,
    /// Which resolution the epilogue writes through. See [`IdentitySource`].
    pub identity: IdentitySource,
}

impl HookConfig {
    /// What every profile except `kernel_profile_extreme` ships: stats on, log
    /// on, debug-IO off, errno diag on, audit off.
    ///
    /// `identity` is left to the caller precisely because it is the open
    /// question — there is no "the default" to hide it behind.
    #[must_use]
    pub const fn shipping(identity: IdentitySource) -> Self {
        Self {
            process_stats: true,
            proc_log: true,
            debug_io: false,
            errno_diag: true,
            identity_audit: false,
            identity,
        }
    }

    /// `kernel_profile_extreme`: both recording hooks compiled out.
    #[must_use]
    pub const fn extreme(identity: IdentitySource) -> Self {
        Self {
            process_stats: false,
            proc_log: false,
            debug_io: false,
            errno_diag: false,
            identity_audit: false,
            identity,
        }
    }
}

/// Which resolution of "who am I" the **epilogue** writes through.
///
/// This is the field this crate exists to make nameable. The two variants are
/// the two sides of `IDENTITY_CACHE_SMP_REVIEW.md` Finding A, and
/// [`slot::search`] decides between them by enumeration rather than by
/// stress-testing:
///
/// - [`Self::Prologue`] has a witness interleaving. It is a use-after-free.
/// - [`Self::Reresolve`] has none, over the whole bounded search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentitySource {
    /// Reuse the `&'static Process` the prologue resolved.
    ///
    /// **Unsound**, and shipping as of 2026-08-28. The dispatch between the two
    /// is open-ended — a `ppoll` / futex / blocking `read` — and
    /// `kill_thread_group` retires a sibling's process *while that sibling is
    /// still executing kernel code*, after which any idle core's reclaim drain
    /// frees it 10 ms later. The epilogue then writes two atomics into a freed
    /// and very likely reallocated block.
    ///
    /// The pre-cache epilogue re-did the lookup and skipped its writes on
    /// `None`; that lookup *was* the guard, and hoisting it away was the bug.
    Prologue,
    /// Read the identity cache again after the dispatch, and skip the epilogue
    /// writes when it misses.
    ///
    /// Restores the pre-cache behaviour exactly — a retired slot yields `None`
    /// and the writes are skipped — at the cost of one more cache read
    /// (a validated slot-state load plus a generation load), not the lock +
    /// map walk + masked table scan the pre-cache epilogue paid twice.
    Reresolve,
}

/// Which `syscall_counters::inc_*` a number belongs to.
///
/// Pure classification. The kernel matches on this and calls the counter; the
/// arms are not `fn` pointers on purpose — an indirect call here is exactly the
/// abstraction cost §7 warns about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Counter {
    /// `inc_mmap(pages)` — the only arm that carries a payload; the kernel
    /// derives `pages` from `args[1]`, which this crate never sees.
    Mmap,
    Munmap,
    Brk,
    Read,
    Write,
    Openat,
    Close,
    Mprotect,
    Futex,
    SigProcMask,
    SigAction,
    Clock,
    Ioctl,
    Fstat,
    Yield,
    Madvise,
    Mremap,
    Lseek,
    Getrandom,
    Getpid,
    Fcntl,
    /// `inc_other(nr)` — everything unbucketed, which also records the number.
    Other,
}

/// Which counter bucket `nr` falls in.
///
/// Transcribed from the 22-arm `match` that was inline in `handle_syscall`.
/// Being a `const fn` over a C-like enum, it inlines into the caller's own
/// `match` and the pair compiles back to one dispatch — verified by
/// measurement, not by assumption (see the crate docs).
#[must_use]
pub const fn counter_for(nr: u64) -> Counter {
    match nr {
        nr::MMAP => Counter::Mmap,
        nr::MUNMAP => Counter::Munmap,
        nr::BRK => Counter::Brk,
        nr::READ | nr::READV | nr::PREAD64 | nr::PREADV | nr::PREADV2 => Counter::Read,
        nr::WRITE | nr::WRITEV | nr::PWRITE64 | nr::PWRITEV | nr::PWRITEV2 => Counter::Write,
        nr::OPENAT => Counter::Openat,
        nr::CLOSE => Counter::Close,
        nr::MPROTECT => Counter::Mprotect,
        nr::FUTEX => Counter::Futex,
        nr::RT_SIGPROCMASK => Counter::SigProcMask,
        nr::RT_SIGACTION => Counter::SigAction,
        nr::CLOCK_GETTIME => Counter::Clock,
        nr::IOCTL => Counter::Ioctl,
        nr::FSTAT | nr::NEWFSTATAT => Counter::Fstat,
        nr::SCHED_YIELD => Counter::Yield,
        nr::MADVISE => Counter::Madvise,
        nr::MREMAP => Counter::Mremap,
        nr::LSEEK => Counter::Lseek,
        nr::GETRANDOM => Counter::Getrandom,
        nr::GETPID => Counter::Getpid,
        nr::FCNTL => Counter::Fcntl,
        _ => Counter::Other,
    }
}

/// Does a fresh excursion on `nr` clear this thread's delivered-signal and
/// sigframe-active records?
///
/// Every number does **except** `rt_sigreturn`, and that exemption is
/// load-bearing in both directions:
///
/// - Clearing is what stops one delivery fabricating an `EINTR` in an unrelated
///   later syscall.
/// - `rt_sigreturn` is exempt because the handler returns *through* it, so
///   clearing there would erase the record belonging to the blocking syscall
///   about to resume — the exact starvation the mask was added to fix
///   (`PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md`). By the same token userspace
///   has not run yet at that point, so the sigframe-active re-arm must not fire
///   either.
///
/// One comparison, and it is a `const fn` so a caller with a literal `nr` folds
/// it away entirely.
#[must_use]
pub const fn clears_signal_state(nr: u64) -> bool {
    nr != nr::RT_SIGRETURN
}

/// Is `nr` on the debug-IO print's suppression list?
///
/// `SYSCALL_DEBUG_IO_ENABLED` prints one line per syscall, so the high-rate
/// numbers are excluded or the console becomes the workload. The list is the
/// chain of `!=` comparisons that was inline in `handle_syscall`; it is only
/// reachable when the flag is on, which no shipping profile does — which is
/// exactly why it was worth moving somewhere a test can reach it. A number
/// silently added to or dropped from that chain changes nothing observable
/// until someone turns the flag on to debug something else.
#[must_use]
pub const fn debug_io_suppressed(nr: u64) -> bool {
    matches!(
        nr,
        nr::WRITE
            | nr::READ
            | nr::READV
            | nr::WRITEV
            | nr::IOCTL
            | nr::PSELECT6
            | nr::PPOLL
            | nr::BRK
            | nr::MMAP
            | nr::MUNMAP
            | nr::MREMAP
            | nr::CLOSE
            | nr::FSTAT
            | nr::LSEEK
            | nr::RT_SIGPROCMASK
            | nr::NANOSLEEP
            | nr::WAITPID
            | nr::UPTIME
            | nr::FUTEX
            | nr::MEMBARRIER
            | nr::RT_SIGACTION
            | nr::SCHED_SETAFFINITY
            | nr::SCHED_GETAFFINITY
    )
}

/// The prologue's decisions, and the state the epilogue needs.
///
/// Built once per excursion, before the dispatch, and consumed after it. The
/// point of the struct is the *after*: `track_time`, `need_timing` and the
/// identity policy are decided on the way in and read on the way out, so the
/// two halves cannot drift apart the way five loose locals could.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Excursion {
    nr: u64,
    cfg: HookConfig,
}

/// What the kernel must do before dispatching.
///
/// Plain booleans and one enum — no closures, no callbacks. The kernel reads
/// the fields and performs the effects itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "a plan of independent yes/no effects the kernel then performs; \
              they are not states of one machine and enum-ing them would \
              invent a hierarchy the code does not have"
)]
pub struct ProloguePlan {
    /// Clear the delivered-signal and sigframe-active records for this thread.
    /// See [`clears_signal_state`].
    pub clear_signal_state: bool,
    /// Emit the `[SC] nr=… a0=… a1=… a2=…` line.
    pub debug_print: bool,
    /// Which `syscall_counters::inc_*` to bump, after the unconditional
    /// `inc_total()`.
    pub counter: Counter,
    /// Bump the per-process `syscall_stats` count for this number.
    ///
    /// The gate only. There is nothing to bump when the identity cache did not
    /// answer, and the kernel expresses that half with its own `if let` — see
    /// [`Excursion::prologue`].
    pub record_stats: bool,
    /// Sample `uptime_us()` into `t0`. False means the epilogue reads no clock
    /// either, which is the whole point of computing it once here.
    pub need_timing: bool,
}

/// What the kernel must do after the dispatch returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "a plan of independent yes/no effects the kernel then performs; \
              they are not states of one machine and enum-ing them would \
              invent a hierarchy the code does not have"
)]
pub struct EpiloguePlan {
    /// Compare the prologue's identity against a fresh lookup and bump
    /// `EPILOGUE_STALE_IDENTITY` / `EPILOGUE_IDENTITY_MOVED`. Diagnostic only.
    pub audit_identity: bool,
    /// Which resolution the writes below go through. [`IdentitySource::Prologue`]
    /// reuses the pointer from before the dispatch; [`IdentitySource::Reresolve`]
    /// reads the cache again and skips every write below on a miss.
    pub identity: IdentitySource,
    /// Store `!0` into `Process::current_syscall`. Unconditional in the kernel
    /// today — which is what makes [`IdentitySource::Prologue`] a
    /// use-after-free on the *shipping* build and not only under the
    /// default-on diagnostic flags.
    pub clear_current_syscall: bool,
    /// Fold the elapsed time into `syscall_stats`.
    pub record_time: bool,
    /// Append to the `/proc/<pid>/syscalls` ring.
    pub log: bool,
    /// Emit the `[EFAULT] …` diagnostic line.
    pub errno_diag: bool,
}

impl Excursion {
    /// Open an excursion on `nr` under `cfg`.
    #[must_use]
    pub const fn new(nr: u64, cfg: HookConfig) -> Self {
        Self { nr, cfg }
    }

    /// The syscall number this excursion is for.
    #[must_use]
    pub const fn nr(self) -> u64 {
        self.nr
    }

    /// The gates in force.
    #[must_use]
    pub const fn config(self) -> HookConfig {
        self.cfg
    }

    /// Everything the kernel does before the dispatch.
    ///
    /// Buildable **before** the identity is resolved, which is why
    /// [`ProloguePlan::record_stats`] is the gate alone and not the gate
    /// conjoined with "an identity was resolved": the plan has to be in hand at
    /// the signal-state clear, which happens first. The kernel supplies the
    /// second half of the conjunction with the `if let` it already writes —
    /// same shape as [`EpiloguePlan::audit_identity`].
    #[must_use]
    pub const fn prologue(self) -> ProloguePlan {
        ProloguePlan {
            clear_signal_state: clears_signal_state(self.nr),
            debug_print: self.cfg.debug_io && !debug_io_suppressed(self.nr),
            counter: counter_for(self.nr),
            record_stats: self.cfg.process_stats,
            // One clock read serves both hooks, so the union is computed here
            // and the epilogue re-reads the same decision rather than its own.
            need_timing: self.cfg.process_stats || self.cfg.proc_log,
        }
    }

    /// Everything the kernel does after the dispatch returns.
    ///
    /// `owner_pid` is the prologue's tgid (0 when it did not resolve) and
    /// `is_efault` is `result == EFAULT`. Both are values the kernel already
    /// holds; passing them keeps errno constants and process state out of a
    /// crate that must depend on neither.
    #[must_use]
    pub const fn epilogue(self, owner_pid: u64, is_efault: bool) -> EpiloguePlan {
        let need_timing = self.cfg.process_stats || self.cfg.proc_log;
        EpiloguePlan {
            audit_identity: self.cfg.identity_audit,
            identity: self.cfg.identity,
            clear_current_syscall: true,
            record_time: need_timing && self.cfg.process_stats,
            // `owner_pid == 0` means the prologue never resolved an identity,
            // and the ring is keyed by pid — there is nothing to file it under.
            log: need_timing && self.cfg.proc_log && owner_pid != 0,
            errno_diag: self.cfg.errno_diag && is_efault,
        }
    }
}

#[cfg(test)]
mod tests;
