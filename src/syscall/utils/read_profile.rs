//! `[READPROF]` — where the per-`read(2)` fixed cost actually goes.
//!
//! **Measurement builds only** (`read-profile`), like [`crate::bkl_profile`] and
//! [`crate::nic_profile`]. Off by default and a ZST when off: every type and
//! method below compiles to nothing without the feature, so the hot path keeps
//! its shape.
//!
//! # The question this exists to answer
//!
//! After the user-copy widening (`docs/archive/USER_COPY_BYTE_LOOP.md`) a warm
//! `seq_read` stopped being byte-bound and became **fixed-cost-bound**: 2 MB in
//! 256 reads of 8 KB costs ~5 ms, of which ~4.4 ms is 256 x ~17 us of per-call
//! overhead and only ~1.2 ms is bytes. Removing heap allocations has been
//! measured twice at this scale and was invisible both times, so the rule for
//! this subsystem is now **instrument first**. That is what this module is.
//!
//! # What it measures
//!
//! Three nested spans, each timed by the function that owns it — no state is
//! passed between them, so nothing can go stale across a preemption:
//!
//! ```text
//!   rust_sync_el0_handler   ... SPAN_EXC   (BKL entry, tripwires, dispatch, deferred kill)
//!     handle_syscall        ... SPAN_HS    (+ pid lookup, counters, timing hooks)
//!       sys_read            ... SPAN_SR    (+ the stage breakdown below)
//! ```
//!
//! Subtracting adjacent spans at dump time attributes the two wrappers without
//! either of them having to know where the other starts. Inside `sys_read` the
//! [`Rec`] laps name the suspects individually ([`S_VALIDATE`] .. [`S_POS`]),
//! and `resid` is whatever the laps did not name — a large `resid` means the
//! stage list is wrong, not that the cost vanished.
//!
//! # Reading the numbers
//!
//! `cal` is the cost of one `mrs cntvct_el0` pair, sampled the same way as
//! everything else. Every lap pays it, so a stage smaller than `cal` is not a
//! measurement. Subtract `laps x cal` from `sr` before believing the total.
//!
//! `n`, `n_hs` and `n_exc` must agree. They can only diverge if a `read(2)` on a
//! real file left the kernel by a path that skipped a wrapper's epilogue (signal
//! delivery, deferred kill), which would also mean the spans on that call were
//! attributed to the wrong syscall.
//!
//! # The floor arm
//!
//! `getpid` (`FLOOR_NR`) is timed with the *same two wrapper spans*, so the part
//! of the cost that every syscall pays can be subtracted rather than guessed.
//! This is what makes the read-path stages falsifiable: `pro_epi` measured
//! 500 ns while a whole undisturbed `getpid` round trip measured 440 ns, and a
//! stage cannot cost more than the syscall containing it. Drive it with the
//! probe's floor arm (`userspace/ext2probe/c/read_syscall_cost.c`).
//!
//! Note the resolution floor: the counter ticks at 41.7 ns on this machine, so
//! every `min` is a multiple of that and a lever worth less cannot be resolved
//! by one — only bounded.
//!
//! # Never read wall-clock throughput off this build
//!
//! [`dump`] writes ~14 lines to the serial console per window, which on QEMU
//! HVF is about **55 ms** — by far the most expensive thing this module does,
//! and it lands inside a `read(2)`. A `dd` or a probe loop running on a
//! `read-profile` kernel therefore reports a per-read cost dominated by the
//! measurement (219 us/read against a real ~5 us). The per-stage numbers are
//! unaffected — they are closed before the dump — but the wall clock is not.
//! Take throughput from a plain `--release` build.
//!
//! # Single core only
//!
//! The wrapper spans are handed up by one `PENDING` flag, so two cores in
//! `read(2)` at once would cross-attribute. Measure with `SMP=1`; the counts
//! above are the check that you did.

#[cfg(feature = "read-profile")]
pub use enabled::*;

#[cfg(not(feature = "read-profile"))]
pub use disabled::*;

/// Stage index: `validate_user_ptr` — the user-buffer range check, one page
/// table walk per page of the request.
pub const S_VALIDATE: usize = 0;
/// Stage index: `current_process_shared` + `get_fd` — process-table lookup and
/// the `FileDescriptor` clone (which clones the path `String` and bumps two
/// `InodePin` atomics).
pub const S_FD: usize = 1;
/// Stage index: `VfsBklGuard::new`.
pub const S_BKL: usize = 2;
/// Stage index: `vec![0u8; to_read]` — the staging buffer's `alloc_zeroed`,
/// i.e. a talc allocation plus a memset of bytes overwritten on the next line.
pub const S_ALLOC: usize = 3;
/// Stage index: `read_at_open_file` — ext2 by inode, served from the write-back
/// cache on a warm read.
pub const S_FS: usize = 4;
/// Stage index: `copy_to_user`.
pub const S_COPY: usize = 5;
/// Stage index: `update_fd` — advancing `file.position`.
pub const S_POS: usize = 6;
/// Number of named stages.
#[cfg_attr(not(feature = "read-profile"), allow(dead_code))]
pub const N_STAGES: usize = 7;

/// Short label per stage, in `S_*` order.
#[cfg_attr(not(feature = "read-profile"), allow(dead_code))]
pub const STAGE_NAMES: [&str; N_STAGES] =
    ["validate", "fd", "bkl", "alloc", "fs", "copy", "pos"];

#[cfg(not(feature = "read-profile"))]
#[allow(
    clippy::unused_self,
    clippy::needless_pass_by_ref_mut,
    reason = "the shape has to match the instrumented one exactly"
)]
mod disabled {
    /// Per-call stage recorder — a ZST without `read-profile`.
    pub struct Rec;

    impl Rec {
        #[inline(always)]
        #[must_use]
        pub fn new() -> Self {
            Self
        }
        #[inline(always)]
        pub fn lap(&mut self, _stage: usize) {}
        #[inline(always)]
        pub fn commit(self, _bytes: usize) {}
    }

    impl Default for Rec {
        #[inline(always)]
        fn default() -> Self {
            Self::new()
        }
    }

    /// Wrapper-span recorder — a ZST without `read-profile`.
    pub struct Span;

    impl Span {
        #[inline(always)]
        #[must_use]
        pub fn new() -> Self {
            Self
        }
        #[inline(always)]
        pub fn end_handle_syscall(self, _nr: u64) {}
        /// Kept for shape parity with the enabled arm; the exception path now
        /// closes its span through [`exception_span_end`] (a no-op here), so
        /// nothing calls this directly.
        #[allow(dead_code)]
        #[inline(always)]
        pub fn end_exception(self) {}
    }

    impl Default for Span {
        #[inline(always)]
        fn default() -> Self {
            Self::new()
        }
    }

    /// Floor-arm lap markers — no-ops without `read-profile`. The enabled
    /// half lives in [`floor_laps`]; both exist so `handle_syscall` can call
    /// them unconditionally.
    pub mod floor_laps {
        #[inline(always)]
        pub fn start(_nr: u64) {}
        #[inline(always)]
        pub fn lap(_stage: usize) {}
    }

    /// The EL0 handler's outer span as plain functions over a raw start tick —
    /// the exception path's `ExceptionHooks` cannot name `Span`, so the two
    /// halves travel separately and reassemble here. Both compile to nothing
    /// without the feature. (Shared doc with the enabled arm below.)
    #[inline(always)]
    #[must_use]
    pub fn exception_span_start() -> u64 {
        0
    }

    #[inline(always)]
    pub fn exception_span_end(_start: u64) {}
}

/// Floor-arm lap indices: the suspects inside `handle_syscall`'s prologue and
/// epilogue, timed only on the `getpid` floor arm (`FLOOR_NR`). Public so
/// `handle_syscall` can name its lap boundaries; the statics stay private to
/// the enabled module.
pub const F_LAP_IDENT: usize = 0;
pub const F_LAP_INTRPT: usize = 1;
pub const F_LAP_HOOKS: usize = 2;
pub const F_LAP_DISP: usize = 3;
pub const F_LAP_EPI1: usize = 4;
pub const F_LAP_EPI2: usize = 5;
#[cfg_attr(not(feature = "read-profile"), allow(dead_code))]
pub const N_FLOOR_LAPS: usize = 6;
#[cfg_attr(not(feature = "read-profile"), allow(dead_code))]
pub const FLOOR_LAP_NAMES: [&str; N_FLOOR_LAPS] =
    ["ident", "intrpt", "hooks", "disp", "epi1", "epi2"];

#[cfg(feature = "read-profile")]
mod enabled {
    use core::sync::atomic::{AtomicU64, Ordering};

    use super::{FLOOR_LAP_NAMES, N_STAGES, STAGE_NAMES};

    /// Reads per dump window.
    ///
    /// 256 is `ext2probe`'s whole `seq_read` phase (2 MB in `SEQ_CHUNK` = 8 KB
    /// reads), which is the workload every published `seq_read` number in
    /// `docs/archive/` was measured on — so one window is exactly one of those
    /// numbers and the two can be compared directly instead of by proportion.
    /// A `dd` pass over the 8 MB fixture fills 8 windows at `bs=4096`.
    const DUMP_EVERY: u64 = 256;

    /// Smallest request that enters a window.
    ///
    /// Not a performance gate — a correctness one for `min`. Every file read in
    /// the system lands here, including sshd's and busybox's few-byte reads of
    /// small files, and one 32-byte read in the window makes every `min` below
    /// the cost of *that* read rather than of the workload's. The first run
    /// instrumented this way reported `copy: min=0ns` for an 8 KB copy — 82
    /// GB/s — which is how the pollution was found. 4096 keeps `dd bs>=4096`
    /// and drops the housekeeping; `bytes/read` in the dump is the check that
    /// the window really was homogeneous.
    const MIN_BYTES: usize = 4096;

    /// Per-stage tick totals for the current window.
    #[allow(clippy::declare_interior_mutable_const, reason = "array initialiser")]
    const ZERO: AtomicU64 = AtomicU64::new(0);
    static STAGE_TICKS: [AtomicU64; N_STAGES] = [ZERO; N_STAGES];
    /// Per-stage **minimum** for the window.
    ///
    /// The mean is not usable on its own here: a timer tick landing inside a
    /// stage adds the whole scheduling excursion to that one sample, and ~25
    /// ticks across a 1024-read window move a stage's mean by microseconds —
    /// which is the same size as the thing being measured. The first window
    /// measured this way put `alloc` at 352 ns and the next at 6282 ns from the
    /// same binary on the same file, purely by where the ticks landed.
    ///
    /// The minimum is immune to that: interference can only ever make a sample
    /// larger, so `min` is what the stage costs when nothing interrupts it.
    /// Read `min` for "what does this cost", and `mean - min` for "how much
    /// interference did this window take".
    #[allow(clippy::declare_interior_mutable_const, reason = "array initialiser")]
    const MAXV: AtomicU64 = AtomicU64::new(u64::MAX);
    static STAGE_MIN: [AtomicU64; N_STAGES] = [MAXV; N_STAGES];
    static SR_MIN: AtomicU64 = AtomicU64::new(u64::MAX);
    static HS_MIN: AtomicU64 = AtomicU64::new(u64::MAX);
    static EXC_MIN: AtomicU64 = AtomicU64::new(u64::MAX);
    /// Whole-`sys_read` ticks, calibration ticks, and the two wrapper spans.
    static SR_TICKS: AtomicU64 = AtomicU64::new(0);
    static CAL_TICKS: AtomicU64 = AtomicU64::new(0);
    static HS_TICKS: AtomicU64 = AtomicU64::new(0);
    static EXC_TICKS: AtomicU64 = AtomicU64::new(0);
    /// Calls counted at each of the three levels. They must agree; see the
    /// module docs.
    static N_SR: AtomicU64 = AtomicU64::new(0);
    static N_HS: AtomicU64 = AtomicU64::new(0);
    static N_EXC: AtomicU64 = AtomicU64::new(0);
    /// Set by [`Rec::commit`], consumed by the wrappers: "the call now unwinding
    /// was a `read(2)` on a real file, so your span belongs in this window".
    static PENDING: AtomicU64 = AtomicU64::new(0);
    /// Exit timestamp of the previous profiled `read(2)`, and the accumulated
    /// gap between that exit and the next one's entry.
    ///
    /// This is the arm that closes the accounting. Every span above measures
    /// time *inside* `rust_sync_el0_handler`, so all of them together still say
    /// nothing about the cost of getting in and out — the vector asm's register
    /// save/restore, the `eret`, and whatever userspace does between two calls.
    /// A bare `read(2)` loop measured from EL0 costs several microseconds per
    /// call on this kernel while `exc` reports ~1.6 us, and only a measurement
    /// that spans the boundary can say which side of it the difference is on.
    ///
    /// Meaningful only when consecutive profiled reads really are consecutive
    /// syscalls — a `dd` puts a `write(2)` in between and inflates this
    /// legitimately. Use a read-only loop (`userspace/ext2probe/c/read_syscall_cost.c`
    /// with `--read-mode`) to read it as "cost outside the kernel".
    static LAST_EXIT: AtomicU64 = AtomicU64::new(0);
    static GAP_TICKS: AtomicU64 = AtomicU64::new(0);
    static GAP_MIN: AtomicU64 = AtomicU64::new(u64::MAX);
    static N_GAP: AtomicU64 = AtomicU64::new(0);
    /// Same log2-microsecond buckets as [`EXC_HIST`], over the EL0-side gap.
    /// The mean gap is meaningless on its own — one read seconds after the last
    /// one puts a whole second into it — so the shape is what says whether the
    /// window really was a back-to-back loop.
    static GAP_HIST: [AtomicU64; HIST_BUCKETS] = [ZERO; HIST_BUCKETS];
    /// What [`Rec::commit`]'s own bookkeeping costs, and how often it ran.
    ///
    /// `commit` closes the `sr` span and *then* does ~20 atomic
    /// read-modify-writes across cold static cache lines. All of that lands
    /// after `sr` ends and before `hs` ends, so it is charged to `pro_epi` —
    /// which is how `pro_epi` came to read 500 ns inside a syscall whose entire
    /// undisturbed round trip is 440 ns. A stage cannot cost more than the
    /// syscall containing it, so the excess had to be the instrument.
    /// Subtract this from `pro_epi` to get the kernel's own prologue/epilogue.
    static COMMIT_TICKS: AtomicU64 = AtomicU64::new(0);
    static COMMIT_MIN: AtomicU64 = AtomicU64::new(u64::MAX);

    /// The floor arm: the same two spans, around a syscall that does nothing.
    ///
    /// `read`'s `pro_epi` measures 500 ns, and the probe measures a whole
    /// undisturbed `getpid` round trip at 440-520 ns. Both cannot be right —
    /// `getpid` runs the same `handle_syscall` prologue and epilogue. Rather
    /// than reason about which, instrument `getpid` with the identical spans and
    /// read the difference off. `nr` 172 is `getpid` on aarch64; the probe's
    /// floor arm drives it (`userspace/ext2probe/c/read_syscall_cost.c`).
    const FLOOR_NR: u64 = 172;
    static FLOOR_EXC: AtomicU64 = AtomicU64::new(0);
    static FLOOR_EXC_MIN: AtomicU64 = AtomicU64::new(u64::MAX);
    static FLOOR_HS: AtomicU64 = AtomicU64::new(0);
    static FLOOR_HS_MIN: AtomicU64 = AtomicU64::new(u64::MAX);
    static N_FLOOR: AtomicU64 = AtomicU64::new(0);
    /// Set by `end_handle_syscall` when this excursion was the floor syscall, so
    /// `end_exception` knows to close its outer span too. Same handoff as
    /// `PENDING`, and mutually exclusive with it.
    static FLOOR_PENDING: AtomicU64 = AtomicU64::new(0);

    /// Floor-arm laps (see the `F_LAP_*` constants above): per-lap tick totals,
    /// minima, the lap-boundary timestamps for the call in flight, and whether
    /// one is in flight. SMP=1-only like everything here: a floor call
    /// preempted mid-flight by another thread's `getpid` interleaves laps, which
    /// is why `n_lap` is printed — it must equal the floor `n=`.
    #[allow(clippy::declare_interior_mutable_const, reason = "array initialiser")]
    const FZERO: AtomicU64 = AtomicU64::new(0);
    static FLOOR_LAP_TICKS: [AtomicU64; super::N_FLOOR_LAPS] = [FZERO; super::N_FLOOR_LAPS];
    static FLOOR_LAP_MIN: [AtomicU64; super::N_FLOOR_LAPS] = [MAXV; super::N_FLOOR_LAPS];
    static FLOOR_LAP_T: [AtomicU64; super::N_FLOOR_LAPS] = [FZERO; super::N_FLOOR_LAPS];
    static FLOOR_LAP_ACTIVE: AtomicU64 = AtomicU64::new(0);
    static N_FLOOR_LAPS_SEEN: AtomicU64 = AtomicU64::new(0);

    /// Floor-lap recording, driven from `handle_syscall`. `start` arms the lap
    /// clock on the floor syscall only; `lap` closes one named boundary. Each
    /// boundary is an `isb; mrs` pair (~`cal`), so a lap reads ~`cal` high —
    /// subtract `N_FLOOR_LAPS x cal` from the lap sum before comparing it with
    /// `hs`.
    pub mod floor_laps {
        use super::super::N_FLOOR_LAPS;
        use super::{now, FLOOR_LAP_ACTIVE, FLOOR_LAP_MIN, FLOOR_LAP_T, FLOOR_LAP_TICKS};
        use super::{FLOOR_NR, N_FLOOR_LAPS_SEEN};
        use core::sync::atomic::Ordering;

        #[inline]
        pub fn start(nr: u64) {
            if nr == FLOOR_NR {
                FLOOR_LAP_ACTIVE.store(1, Ordering::Relaxed);
                FLOOR_LAP_T[0].store(now(), Ordering::Relaxed);
            }
        }

        #[inline]
        pub fn lap(stage: usize) {
            if stage >= N_FLOOR_LAPS || FLOOR_LAP_ACTIVE.load(Ordering::Relaxed) == 0 {
                return;
            }
            let t = now();
            let d = t.wrapping_sub(FLOOR_LAP_T[stage].load(Ordering::Relaxed));
            if stage + 1 < N_FLOOR_LAPS {
                FLOOR_LAP_T[stage + 1].store(t, Ordering::Relaxed);
            } else {
                FLOOR_LAP_ACTIVE.store(0, Ordering::Relaxed);
                N_FLOOR_LAPS_SEEN.fetch_add(1, Ordering::Relaxed);
            }
            FLOOR_LAP_TICKS[stage].fetch_add(d, Ordering::Relaxed);
            FLOOR_LAP_MIN[stage].fetch_min(d, Ordering::Relaxed);
        }
    }

    /// Windows printed so far — the `w=` field, so a truncated log still says
    /// which pass it came from.
    static WINDOW: AtomicU64 = AtomicU64::new(0);
    /// Bytes requested across the window, and the smallest/largest single
    /// request in it. `min_bytes == max_bytes` is what makes the per-stage
    /// numbers comparable to each other.
    static BYTES: AtomicU64 = AtomicU64::new(0);
    static BYTES_MIN: AtomicU64 = AtomicU64::new(u64::MAX);
    static BYTES_MAX: AtomicU64 = AtomicU64::new(0);

    /// Log2-microsecond histogram of the whole `read(2)` excursion.
    ///
    /// The reason this exists: in the first clean window `min` was 1.25 us and
    /// `mean` was 10.6 us for the *same* 1024 reads of the same 4096 bytes. Two
    /// summary statistics that far apart do not describe one population, and
    /// they support opposite conclusions — "every read costs 10 us of fixed
    /// work" (fix the read path) versus "990 reads cost 1.3 us and 30 of them
    /// were preempted for 300 us" (fix nothing in the read path; the cost is
    /// scheduling). Only the distribution separates those.
    ///
    /// Bucket `i` is `[2^(i-1), 2^i)` microseconds, with bucket 0 for sub-1 us
    /// and the last bucket saturating.
    const HIST_BUCKETS: usize = 10;
    static EXC_HIST: [AtomicU64; HIST_BUCKETS] = [ZERO; HIST_BUCKETS];

    /// Read the virtual counter **with the pipeline synchronised first**.
    ///
    /// Not `akuma_timer::read_counter`, which is a bare `mrs cntvct_el0`. That
    /// is right for deadlines and wrong for timing: `mrs` is not a serialising
    /// instruction, so an out-of-order core executes it as soon as its operands
    /// are ready — which for a lap boundary means *before* the memcpy it is
    /// supposed to be timing has finished. The reorder window is the whole ROB,
    /// and a 4 KB widened user copy is only ~400 instructions, so it fits
    /// entirely inside it.
    ///
    /// This was not a theory. The first sweep run with a bare `mrs` reported a
    /// 4096-byte `copy_to_user` at **66 ns** (62 GB/s) and a 65536-byte one at
    /// `min=0ns`, against a same-machine, same-syscall measured rate of
    /// 0.56 ns/byte — i.e. the copy stage read as free because the timestamp
    /// after it had already executed. `isb` is what Linux's
    /// `arch_timer_read_counter` uses for the same reason.
    ///
    /// The `isb` costs a pipeline flush per lap. That cost is *measured*, not
    /// assumed: it is exactly what `cal=` in the dump reports.
    #[inline(always)]
    fn now() -> u64 {
        // `_ordered`, not the bare read: an unbarriered `mrs cntvct_el0` may
        // issue before the work being timed (see its doc comment).
        akuma_cpu::sysreg::cntvct_el0_ordered()
    }

    /// Per-call stage recorder. Created at the top of `sys_read`, lapped at each
    /// stage boundary, and committed only on the `File` arm — a read on a pipe
    /// or socket drops it and contributes nothing.
    pub struct Rec {
        start: u64,
        last: u64,
        cal: u64,
        stage: [u64; N_STAGES],
    }

    impl Rec {
        #[inline]
        #[must_use]
        pub fn new() -> Self {
            // Calibration first: two back-to-back counter reads, the same pair
            // every lap pays. Sampled per call rather than once at boot so it
            // tracks whatever the host is doing to this vCPU right now.
            let c0 = now();
            let c1 = now();
            Self {
                start: c1,
                last: c1,
                cal: c1.wrapping_sub(c0),
                stage: [0; N_STAGES],
            }
        }

        /// Close the stage that ends here.
        #[inline]
        pub fn lap(&mut self, stage: usize) {
            let t = now();
            self.stage[stage] = t.wrapping_sub(self.last);
            self.last = t;
        }

        /// Fold this call into the window and arm the wrapper spans.
        ///
        /// `bytes` is the request size (`to_read`), not the return value: it is
        /// what the stages below were sized by. Requests under [`MIN_BYTES`] are
        /// dropped without arming the wrappers, so a filtered call contributes
        /// nothing anywhere and the three counts stay equal.
        #[inline]
        pub fn commit(self, bytes: usize) {
            if bytes < MIN_BYTES {
                return;
            }
            let total = now().wrapping_sub(self.start);
            for (i, d) in self.stage.iter().enumerate() {
                STAGE_TICKS[i].fetch_add(*d, Ordering::Relaxed);
                STAGE_MIN[i].fetch_min(*d, Ordering::Relaxed);
            }
            SR_TICKS.fetch_add(total, Ordering::Relaxed);
            SR_MIN.fetch_min(total, Ordering::Relaxed);
            CAL_TICKS.fetch_add(self.cal, Ordering::Relaxed);
            N_SR.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
            BYTES_MIN.fetch_min(bytes as u64, Ordering::Relaxed);
            BYTES_MAX.fetch_max(bytes as u64, Ordering::Relaxed);
            PENDING.store(1, Ordering::Relaxed);
            // Everything above happened after `total` was stamped, so it is
            // invisible to `sr` and charged to `pro_epi`. Measure it. The two
            // atomics below are themselves unmeasured — a floor of ~2 RMWs on
            // a number that exists to expose ~20 of them.
            let after = now().wrapping_sub(self.start).wrapping_sub(total);
            COMMIT_TICKS.fetch_add(after, Ordering::Relaxed);
            COMMIT_MIN.fetch_min(after, Ordering::Relaxed);
        }
    }

    impl Default for Rec {
        #[inline]
        fn default() -> Self {
            Self::new()
        }
    }

    /// One wrapper's span. Both wrappers create it unconditionally — it is two
    /// counter reads and a `PENDING` load, and it must start before the wrapper
    /// knows which syscall it is about to run.
    pub struct Span {
        start: u64,
    }

    impl Span {
        #[inline]
        #[must_use]
        pub fn new() -> Self {
            Self { start: now() }
        }

        /// Close `handle_syscall`'s span. Leaves `PENDING` set for the
        /// exception handler above.
        ///
        /// `nr` selects the floor arm: a syscall that does no work still pays
        /// this function's prologue and epilogue, so timing it with the same
        /// span is what makes `pro_epi` falsifiable.
        #[inline]
        pub fn end_handle_syscall(self, nr: u64) {
            if PENDING.load(Ordering::Relaxed) != 0 {
                let d = now().wrapping_sub(self.start);
                HS_TICKS.fetch_add(d, Ordering::Relaxed);
                HS_MIN.fetch_min(d, Ordering::Relaxed);
                N_HS.fetch_add(1, Ordering::Relaxed);
            } else if nr == FLOOR_NR {
                let d = now().wrapping_sub(self.start);
                FLOOR_HS.fetch_add(d, Ordering::Relaxed);
                FLOOR_HS_MIN.fetch_min(d, Ordering::Relaxed);
                N_FLOOR.fetch_add(1, Ordering::Relaxed);
                FLOOR_PENDING.store(1, Ordering::Relaxed);
            }
        }

        /// Close the EL0 handler's span, clear `PENDING`, and dump the window
        /// when it is full. Dumping here rather than in `commit` keeps the
        /// console write out of `sys_read`'s own measured span and off the VFS
        /// BKL.
        #[inline]
        pub fn end_exception(self) {
            if PENDING.swap(0, Ordering::Relaxed) == 0 {
                if FLOOR_PENDING.swap(0, Ordering::Relaxed) != 0 {
                    let d = now().wrapping_sub(self.start);
                    FLOOR_EXC.fetch_add(d, Ordering::Relaxed);
                    FLOOR_EXC_MIN.fetch_min(d, Ordering::Relaxed);
                    // Floor-only workloads never open a read window, so without
                    // this the floor block below would only ever print when a
                    // `read(2)` happened to dump. Self-dump on the same cadence
                    // (`N_FLOOR` was already counted by `end_handle_syscall`).
                    if N_FLOOR.load(Ordering::Relaxed) % DUMP_EVERY == 0 {
                        dump();
                    }
                }
                return;
            }
            let prev_exit = LAST_EXIT.load(Ordering::Relaxed);
            if prev_exit != 0 {
                let gap = self.start.wrapping_sub(prev_exit);
                GAP_TICKS.fetch_add(gap, Ordering::Relaxed);
                GAP_MIN.fetch_min(gap, Ordering::Relaxed);
                GAP_HIST[bucket_of(gap)].fetch_add(1, Ordering::Relaxed);
                N_GAP.fetch_add(1, Ordering::Relaxed);
            }
            let end = now();
            let d = end.wrapping_sub(self.start);
            EXC_TICKS.fetch_add(d, Ordering::Relaxed);
            EXC_MIN.fetch_min(d, Ordering::Relaxed);
            EXC_HIST[bucket_of(d)].fetch_add(1, Ordering::Relaxed);
            if N_EXC.fetch_add(1, Ordering::Relaxed) + 1 >= DUMP_EVERY {
                dump();
            }
            // AFTER the dump, not before. `dump()` writes ~14 lines to the
            // serial console, which is milliseconds of MMIO; stamping the exit
            // before it charged that whole console write to the NEXT read's
            // gap. One such gap per window was enough to take the mean gap from
            // ~1.7 us to ~315 us and made the instrumented `read(2)` arm look
            // 15x worse than the uninstrumented `pread(2)` one — a difference
            // that was entirely this line.
            LAST_EXIT.store(now(), Ordering::Relaxed);
        }
    }

    impl Default for Span {
        #[inline]
        fn default() -> Self {
            Self::new()
        }
    }

    /// The EL0 handler's outer span as plain functions over a raw start tick —
    /// the exception path's `ExceptionHooks` cannot name `Span`, so the two
    /// halves travel separately and reassemble here. The enabled pair costs
    /// exactly what `Span::new`/`end_exception` always cost: one `now()` and
    /// the `end_exception` body on the same `start` value. (Shared doc with
    /// the no-op pair in the `disabled` arm above.)
    #[inline]
    #[must_use]
    pub fn exception_span_start() -> u64 {
        now()
    }

    #[inline]
    pub fn exception_span_end(start: u64) {
        Span { start }.end_exception()
    }

    /// Which log2-microsecond bucket a tick delta falls in. Uses the counter
    /// frequency directly rather than converting to nanoseconds first: this runs
    /// on every read.
    #[inline]
    fn bucket_of(ticks: u64) -> usize {
        let freq = akuma_timer::read_frequency();
        if freq == 0 {
            return 0;
        }
        let us = ticks.saturating_mul(1_000_000) / freq;
        if us == 0 {
            0
        } else {
            ((64 - us.leading_zeros()) as usize).min(HIST_BUCKETS - 1)
        }
    }

    /// Ticks -> nanoseconds. `u128` intermediate: a window's tick total times
    /// `1e9` overflows `u64` at ~18 seconds of counter.
    fn ns(ticks: u64, freq: u64) -> u64 {
        if freq == 0 {
            return 0;
        }
        ((u128::from(ticks) * 1_000_000_000) / u128::from(freq)) as u64
    }

    /// Print the window and reset it. Deltas, not totals, for the same reason
    /// `bkl_profile` prints deltas: a window has to belong to the workload that
    /// ran during it.
    fn dump() {
        let freq = akuma_timer::read_frequency();
        let n = N_SR.swap(0, Ordering::Relaxed).max(1);
        let n_hs = N_HS.swap(0, Ordering::Relaxed);
        let n_exc = N_EXC.swap(0, Ordering::Relaxed);
        let sr = SR_TICKS.swap(0, Ordering::Relaxed);
        let hs = HS_TICKS.swap(0, Ordering::Relaxed);
        let exc = EXC_TICKS.swap(0, Ordering::Relaxed);
        let cal = CAL_TICKS.swap(0, Ordering::Relaxed);
        let w = WINDOW.fetch_add(1, Ordering::Relaxed);

        let per = |t: u64| ns(t, freq) / n;
        let one = |t: u64| ns(if t == u64::MAX { 0 } else { t }, freq);
        let one_u = |v: u64| if v == u64::MAX { 0 } else { v };
        let sr_ns = per(sr);
        let hs_ns = per(hs);
        let exc_ns = per(exc);
        let sr_min = one(SR_MIN.swap(u64::MAX, Ordering::Relaxed));
        let hs_min = one(HS_MIN.swap(u64::MAX, Ordering::Relaxed));
        let exc_min = one(EXC_MIN.swap(u64::MAX, Ordering::Relaxed));

        // Whatever the laps did not name. Saturating: a preempted call can make
        // one stage larger than the total it was measured inside.
        let named: u64 = STAGE_TICKS.iter().map(|s| per(s.load(Ordering::Relaxed))).sum();
        let named_min: u64 = STAGE_MIN.iter().map(|s| one(s.load(Ordering::Relaxed))).sum();

        crate::safe_print!(
            192,
            "[READPROF] w={} n={} n_hs={} n_exc={} bytes={}/{}..{} freq={} cal={}ns/lap\n",
            w,
            n,
            n_hs,
            n_exc,
            BYTES.swap(0, Ordering::Relaxed) / n,
            one_u(BYTES_MIN.swap(u64::MAX, Ordering::Relaxed)),
            BYTES_MAX.swap(0, Ordering::Relaxed),
            freq,
            per(cal),
        );
        crate::safe_print!(
            192,
            "[READPROF] w={} mean exc={}ns hs={}ns sr={}ns  wrap={}ns pro_epi={}ns resid={}ns\n",
            w,
            exc_ns,
            hs_ns,
            sr_ns,
            exc_ns.saturating_sub(hs_ns),
            hs_ns.saturating_sub(sr_ns),
            sr_ns.saturating_sub(named),
        );
        {
            let ng = N_GAP.swap(0, Ordering::Relaxed).max(1);
            let gt = GAP_TICKS.swap(0, Ordering::Relaxed);
            crate::safe_print!(
                192,
                "[READPROF] w={} gap  mean={}ns min={}ns n={}  (EL0 side: asm epilogue + user + asm prologue)\n",
                w,
                ns(gt, freq) / ng,
                one(GAP_MIN.swap(u64::MAX, Ordering::Relaxed)),
                ng,
            );
            let g = |i: usize| GAP_HIST[i].swap(0, Ordering::Relaxed);
            crate::safe_print!(
                192,
                "[READPROF] w={} gap_us <1:{} 1-2:{} 2-4:{} 4-8:{} 8-16:{} 16+:{}\n",
                w, g(0), g(1), g(2), g(3), g(4),
                g(5) + g(6) + g(7) + g(8) + g(9),
            );
        }
        {
            let nf = N_FLOOR.swap(0, Ordering::Relaxed);
            crate::safe_print!(
                192,
                "[READPROF] w={} floor nr={} n={} exc: min={}ns mean={}ns  hs: min={}ns mean={}ns\n",
                w,
                FLOOR_NR,
                nf,
                one(FLOOR_EXC_MIN.swap(u64::MAX, Ordering::Relaxed)),
                ns(FLOOR_EXC.swap(0, Ordering::Relaxed), freq) / nf.max(1),
                one(FLOOR_HS_MIN.swap(u64::MAX, Ordering::Relaxed)),
                ns(FLOOR_HS.swap(0, Ordering::Relaxed), freq) / nf.max(1),
            );
        }
        // Floor laps: what `hs` is made of. One line per lap for the same
        // fixed-buffer reason as the read stages. `n_lap` must equal the floor
        // `n=` above — a smaller value means laps were lost (no floor `start`
        // reached), a larger one means interleaving.
        {
            let nl = N_FLOOR_LAPS_SEEN.swap(0, Ordering::Relaxed).max(1);
            crate::safe_print!(
                160,
                "[READPROF] w={} floorlaps n_lap={} cal={}ns (subtract per lap)\n",
                w,
                nl,
                per(CAL_TICKS.load(Ordering::Relaxed)),
            );
            for (i, name) in FLOOR_LAP_NAMES.iter().enumerate() {
                crate::safe_print!(
                    128,
                    "[READPROF] w={} flap {}: min={}ns mean={}ns\n",
                    w,
                    name,
                    one(FLOOR_LAP_MIN[i].swap(u64::MAX, Ordering::Relaxed)),
                    ns(FLOOR_LAP_TICKS[i].swap(0, Ordering::Relaxed), freq) / nl,
                );
            }
        }
        crate::safe_print!(
            192,
            "[READPROF] w={} commit mean={}ns min={}ns  (instrument overhead inside pro_epi)\n",
            w,
            ns(COMMIT_TICKS.swap(0, Ordering::Relaxed), freq) / n,
            one(COMMIT_MIN.swap(u64::MAX, Ordering::Relaxed)),
        );
        crate::safe_print!(
            192,
            "[READPROF] w={} min  exc={}ns hs={}ns sr={}ns  wrap={}ns pro_epi={}ns resid={}ns\n",
            w,
            exc_min,
            hs_min,
            sr_min,
            exc_min.saturating_sub(hs_min),
            hs_min.saturating_sub(sr_min),
            sr_min.saturating_sub(named_min),
        );
        // The distribution behind `mean` and `min`. Printed in two halves for
        // the same fixed-buffer reason as the per-stage lines below.
        let h = |i: usize| EXC_HIST[i].swap(0, Ordering::Relaxed);
        crate::safe_print!(
            192,
            "[READPROF] w={} exc_us <1:{} 1-2:{} 2-4:{} 4-8:{} 8-16:{}\n",
            w, h(0), h(1), h(2), h(3), h(4),
        );
        crate::safe_print!(
            192,
            "[READPROF] w={} exc_us 16-32:{} 32-64:{} 64-128:{} 128-256:{} 256+:{}\n",
            w, h(5), h(6), h(7), h(8), h(9),
        );
        // One line per stage rather than a single formatted row: `safe_print!`
        // takes a fixed stack buffer, and seven names plus seven numbers in one
        // call is exactly the kind of variable-width row that truncates.
        for (i, name) in STAGE_NAMES.iter().enumerate() {
            crate::safe_print!(
                128,
                "[READPROF] w={} {}: min={}ns mean={}ns\n",
                w,
                name,
                one(STAGE_MIN[i].swap(u64::MAX, Ordering::Relaxed)),
                per(STAGE_TICKS[i].swap(0, Ordering::Relaxed)),
            );
        }
    }
}
