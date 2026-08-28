use super::*;
use crate::{FastPath, fast_path, needs_identity, takes_no_args};
use crate::slot::{
    ALL_OPS, EpilogueSource, Fault, Op, Policy, SlotState, Validation, World, search,
};

// ===========================================================================
// Differential test against the `handle_syscall` this machine was lifted from
// ===========================================================================

/// The prologue/epilogue decisions of `src/syscall/mod.rs::handle_syscall`
/// **as it shipped before the extraction** (tree at `1dd2def6`), transcribed
/// from the source rather than re-derived from it.
///
/// It is deliberately written in the original's shape — the long chain of
/// `!=` comparisons, the 22-arm `match` with its `inc_*` names, the
/// `track_time` / `need_timing` / `logging` locals computed in the original's
/// order — so a reader can diff it against the function it came from. **Do not
/// tidy it.** A tidied oracle proves the model agrees with a tidied oracle.
///
/// `akuma-net-yarn` carries the same thing against `wait_until` for the same
/// reason: a shape crate can pass its own tests and still be wrong.
#[expect(
    clippy::enum_variant_names,
    reason = "each variant is named after the `syscall_counters::inc_*` the \
              original arm called; renaming them would break the one property \
              that makes this readable as a transcription"
)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the oracle records one bool per decision the original made; \
              restructuring it is exactly the tidying its doc comment forbids"
)]
mod reference {
    use akuma_syscalls_linux::nr;

    /// The original's inline counter `match`, arm for arm, naming the
    /// `syscall_counters::inc_*` each one called.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RefCounter {
        IncMmap,
        IncMunmap,
        IncBrk,
        IncRead,
        IncWrite,
        IncOpenat,
        IncClose,
        IncMprotect,
        IncFutex,
        IncSigprocmask,
        IncSigaction,
        IncClock,
        IncIoctl,
        IncFstat,
        IncYield,
        IncMadvise,
        IncMremap,
        IncLseek,
        IncGetrandom,
        IncGetpid,
        IncFcntl,
        IncOther,
    }

    /// Every decision the original prologue made, in the order it made them.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RefPrologue {
        pub clears_signals: bool,
        pub prints_debug: bool,
        pub counter: RefCounter,
        pub bumps_stats: bool,
        pub need_timing: bool,
    }

    /// Every decision the original epilogue made.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RefEpilogue {
        pub audits: bool,
        pub clears_current: bool,
        pub adds_time: bool,
        pub logs: bool,
        pub prints_errno: bool,
    }

    /// `handle_syscall`'s prologue, lines 381-470 of the pre-extraction file.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "one parameter per config const the original read; collapsing \
                  them into a struct would be the tidying this oracle refuses"
    )]
    pub fn prologue(
        syscall_num: u64,
        cur_is_some: bool,
        syscall_debug_io_enabled: bool,
        process_syscall_stats: bool,
        proc_syscall_log_enabled: bool,
    ) -> RefPrologue {
        // `if syscall_num != nr::RT_SIGRETURN { clear_delivered_signals();
        //   clear_sigframe_active(); }`
        let clears_signals = syscall_num != nr::RT_SIGRETURN;

        let prints_debug = syscall_debug_io_enabled
            && syscall_num != nr::WRITE
            && syscall_num != nr::READ
            && syscall_num != nr::READV
            && syscall_num != nr::WRITEV
            && syscall_num != nr::IOCTL
            && syscall_num != nr::PSELECT6
            && syscall_num != nr::PPOLL
            && syscall_num != nr::BRK
            && syscall_num != nr::MMAP
            && syscall_num != nr::MUNMAP
            && syscall_num != nr::MREMAP
            && syscall_num != nr::CLOSE
            && syscall_num != nr::FSTAT
            && syscall_num != nr::LSEEK
            && syscall_num != nr::RT_SIGPROCMASK
            && syscall_num != nr::NANOSLEEP
            && syscall_num != nr::WAITPID
            && syscall_num != nr::UPTIME
            && syscall_num != nr::FUTEX
            && syscall_num != nr::MEMBARRIER
            && syscall_num != nr::RT_SIGACTION
            && syscall_num != nr::SCHED_SETAFFINITY
            && syscall_num != nr::SCHED_GETAFFINITY;

        let counter = match syscall_num {
            nr::MMAP => RefCounter::IncMmap,
            nr::MUNMAP => RefCounter::IncMunmap,
            nr::BRK => RefCounter::IncBrk,
            nr::READ | nr::READV | nr::PREAD64 | nr::PREADV | nr::PREADV2 => RefCounter::IncRead,
            nr::WRITE | nr::WRITEV | nr::PWRITE64 | nr::PWRITEV | nr::PWRITEV2 => {
                RefCounter::IncWrite
            }
            nr::OPENAT => RefCounter::IncOpenat,
            nr::CLOSE => RefCounter::IncClose,
            nr::MPROTECT => RefCounter::IncMprotect,
            nr::FUTEX => RefCounter::IncFutex,
            nr::RT_SIGPROCMASK => RefCounter::IncSigprocmask,
            nr::RT_SIGACTION => RefCounter::IncSigaction,
            nr::CLOCK_GETTIME => RefCounter::IncClock,
            nr::IOCTL => RefCounter::IncIoctl,
            nr::FSTAT | nr::NEWFSTATAT => RefCounter::IncFstat,
            nr::SCHED_YIELD => RefCounter::IncYield,
            nr::MADVISE => RefCounter::IncMadvise,
            nr::MREMAP => RefCounter::IncMremap,
            nr::LSEEK => RefCounter::IncLseek,
            nr::GETRANDOM => RefCounter::IncGetrandom,
            nr::GETPID => RefCounter::IncGetpid,
            nr::FCNTL => RefCounter::IncFcntl,
            _ => RefCounter::IncOther,
        };

        // let track_time = crate::config::PROCESS_SYSCALL_STATS;
        // if track_time && let Some((_, proc)) = cur { proc.syscall_stats.inc(nr); }
        let track_time = process_syscall_stats;
        let bumps_stats = track_time && cur_is_some;

        // let need_timing = track_time || crate::config::PROC_SYSCALL_LOG_ENABLED;
        let need_timing = track_time || proc_syscall_log_enabled;

        RefPrologue { clears_signals, prints_debug, counter, bumps_stats, need_timing }
    }

    /// `handle_syscall`'s epilogue, lines 880-980 of the pre-extraction file.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "same as `prologue`: the original read these as separate consts"
    )]
    pub fn epilogue(
        owner_pid: u64,
        result_is_efault: bool,
        cur_is_some: bool,
        identity_audit: bool,
        process_syscall_stats: bool,
        proc_syscall_log_enabled: bool,
        syscall_errno_diag_enabled: bool,
    ) -> RefEpilogue {
        // if crate::config::IDENTITY_AUDIT && let Some((pid, p)) = cur { .. }
        let audits = identity_audit && cur_is_some;

        // set_thread_current_syscall(!0);
        // if let Some((_, proc)) = cur { proc.current_syscall.store(!0); }
        let clears_current = true;

        let track_time = process_syscall_stats;
        let need_timing = track_time || proc_syscall_log_enabled;

        let mut adds_time = false;
        let mut logs = false;
        if need_timing {
            let logging = proc_syscall_log_enabled && owner_pid != 0;
            if track_time && cur_is_some {
                adds_time = true;
            }
            if logging {
                logs = true;
            }
        }

        // if SYSCALL_ERRNO_DIAG_ENABLED && result == EFAULT { .. }
        let prints_errno = syscall_errno_diag_enabled && result_is_efault;

        RefEpilogue { audits, clears_current, adds_time, logs, prints_errno }
    }
}

/// Every syscall number the model and the oracle should be compared over.
///
/// The whole ABI range plus the two bands the dispatcher treats specially — a
/// number above 500 is the stale-I-cache JIT band, not a syscall, and it still
/// reaches this prologue.
fn probe_numbers() -> impl Iterator<Item = u64> {
    (0..512).chain([600, 1024, 4095, u64::MAX])
}

/// Every combination of the four gates the excursion reads.
fn gate_matrix() -> impl Iterator<Item = (bool, bool, bool, bool)> {
    (0..16u8).map(|m| (m & 1 != 0, m & 2 != 0, m & 4 != 0, m & 8 != 0))
}

/// The extracted prologue must make **the same decisions** as the one it
/// replaced, for every syscall number and every gate combination.
///
/// [`FastPath::Leaf`] numbers are excluded here and tested separately, because
/// they are a deliberate behaviour change and this oracle models the behaviour
/// before it. Excluding them would be a hole if that were the end of it — so
/// `leaf_diverges_from_the_oracle_in_exactly_the_documented_places` pins both
/// halves: which fields the fast path may change, and which it must not.
#[test]
fn prologue_matches_the_shipped_handle_syscall() {
    for nr_val in probe_numbers().filter(|n| fast_path(*n) == FastPath::Full) {
        for (debug_io, stats, proc_log, resolved) in gate_matrix() {
            let cfg = HookConfig {
                process_stats: stats,
                proc_log,
                debug_io,
                errno_diag: true,
                identity_audit: false,
                identity: IdentitySource::Reresolve,
            };
            let got = Excursion::new(nr_val, cfg).prologue();
            let want = reference::prologue(nr_val, resolved, debug_io, stats, proc_log);

            let ctx = format!("nr={nr_val} gates=({debug_io},{stats},{proc_log},{resolved})");
            assert_eq!(
                got.clear_signal_state, want.clears_signals,
                "signal-state clear diverged — {ctx}"
            );
            assert_eq!(got.debug_print, want.prints_debug, "debug print diverged — {ctx}");
            assert_eq!(
                got.record_stats && resolved,
                want.bumps_stats,
                "stats bump diverged — {ctx}"
            );
            assert_eq!(got.need_timing, want.need_timing, "timing need diverged — {ctx}");
            assert_eq!(
                counter_name(got.counter),
                ref_counter_name(want.counter),
                "counter bucket diverged — {ctx}"
            );
        }
    }
}

/// The extracted epilogue must make the same decisions as the one it replaced,
/// over every gate combination, both `owner_pid` cases and both result cases.
#[test]
fn epilogue_matches_the_shipped_handle_syscall() {
    assert_eq!(fast_path(nr::READ), FastPath::Full, "this test's nr must be a full excursion");
    for (errno_diag, stats, proc_log, audit) in gate_matrix() {
        for owner_pid in [0u64, 42] {
            for is_efault in [false, true] {
                // The prologue resolving and `owner_pid` being nonzero are the
                // same fact in `handle_syscall`: `owner_pid` is
                // `cur.map_or(0, |(pid, _)| pid)`.
                let resolved = owner_pid != 0;
                let cfg = HookConfig {
                    process_stats: stats,
                    proc_log,
                    debug_io: false,
                    errno_diag,
                    identity_audit: audit,
                    identity: IdentitySource::Prologue,
                };
                let got = Excursion::new(nr::READ, cfg).epilogue(owner_pid, is_efault);
                let want = reference::epilogue(
                    owner_pid, is_efault, resolved, audit, stats, proc_log, errno_diag,
                );

                let ctx = format!(
                    "gates=({errno_diag},{stats},{proc_log},{audit}) pid={owner_pid} \
                     efault={is_efault}"
                );
                // The audit arm is `IDENTITY_AUDIT && cur.is_some()`; the plan
                // carries only the gate, because the kernel's `if let` supplies
                // the second half. Compare the conjunction the kernel forms.
                assert_eq!(
                    got.audit_identity && resolved,
                    want.audits,
                    "identity audit diverged — {ctx}"
                );
                assert_eq!(
                    got.clear_current_syscall, want.clears_current,
                    "current_syscall clear diverged — {ctx}"
                );
                assert_eq!(
                    got.record_time && resolved,
                    want.adds_time,
                    "stats add_time diverged — {ctx}"
                );
                assert_eq!(got.log, want.logs, "proc log diverged — {ctx}");
                assert_eq!(got.errno_diag, want.prints_errno, "errno diag diverged — {ctx}");
            }
        }
    }
}

/// The fast path is a deliberate divergence from the oracle. This pins it on
/// both sides: exactly which fields it may change, and — the half that actually
/// protects anything — which it must leave alone.
///
/// Getting the second half wrong is how a "harmless" fast path drops a
/// signal-state clear or a counter bump and nothing notices for months.
#[test]
fn leaf_diverges_from_the_oracle_in_exactly_the_documented_places() {
    let leaves: Vec<u64> = probe_numbers().filter(|n| fast_path(*n) == FastPath::Leaf).collect();
    assert_eq!(
        leaves,
        vec![nr::UPTIME, nr::AKUMA_GET_VERSION],
        "the Leaf membership changed — every entry needs the four admission \
         criteria checked against its arm, not assumed"
    );

    for nr_val in leaves {
        for (debug_io, stats, proc_log, _) in gate_matrix() {
            let cfg = HookConfig {
                process_stats: stats,
                proc_log,
                debug_io,
                errno_diag: true,
                identity_audit: true,
                identity: IdentitySource::Reresolve,
            };
            let ex = Excursion::new(nr_val, cfg);
            let got = ex.prologue();
            let want = reference::prologue(nr_val, true, debug_io, stats, proc_log);
            let ctx = format!("nr={nr_val} gates=({debug_io},{stats},{proc_log})");

            // MUST NOT change. These are correctness, not bookkeeping.
            assert_eq!(
                got.clear_signal_state, want.clears_signals,
                "a leaf must still clear signal state — {ctx}"
            );
            assert_eq!(
                counter_name(got.counter),
                ref_counter_name(want.counter),
                "a leaf must still land in its counter bucket — {ctx}"
            );
            assert_eq!(
                got.debug_print, want.prints_debug,
                "a leaf must still honour the debug-IO gate — {ctx}"
            );

            // MAY change, and must, in this exact direction: everything that
            // needs an identity is off, whatever the gates say.
            assert!(!got.resolve_identity, "leaf must not resolve identity — {ctx}");
            assert!(!got.record_stats, "leaf must not record per-process stats — {ctx}");
            assert!(!got.need_timing, "leaf must read no clock — {ctx}");

            let epi = ex.epilogue(42, true);
            assert!(!epi.clear_current_syscall, "leaf stamped nothing to clear — {ctx}");
            assert!(!epi.record_time, "leaf must not add time — {ctx}");
            assert!(!epi.log, "leaf must not log — {ctx}");
            assert!(!epi.audit_identity, "leaf has no identity to audit — {ctx}");
            // The errno diagnostic is NOT identity work — it must survive.
            assert!(epi.errno_diag, "leaf must still report EFAULT — {ctx}");
        }
    }
}

/// The two predicates must genuinely cross, or one of them is redundant and the
/// pair is a more complicated way of writing a single flag.
#[test]
fn the_two_fast_path_predicates_are_independent() {
    // Takes no arguments, but is entirely about identity.
    assert!(takes_no_args(nr::GETPID));
    assert!(needs_identity(nr::GETPID));
    assert_eq!(fast_path(nr::GETPID), FastPath::Full);

    // Takes no arguments and needs no identity.
    assert!(takes_no_args(nr::AKUMA_GET_VERSION));
    assert!(!needs_identity(nr::AKUMA_GET_VERSION));
    assert_eq!(fast_path(nr::AKUMA_GET_VERSION), FastPath::Leaf);

    // Reads arguments and needs identity.
    assert!(!takes_no_args(nr::READ));
    assert!(needs_identity(nr::READ));
    assert_eq!(fast_path(nr::READ), FastPath::Full);

    // `sched_yield` takes no arguments and is deliberately NOT a leaf: it
    // reaches the scheduler, which is about the current thread.
    assert!(takes_no_args(nr::SCHED_YIELD));
    assert!(needs_identity(nr::SCHED_YIELD));
    assert_eq!(fast_path(nr::SCHED_YIELD), FastPath::Full);
}

fn counter_name(c: Counter) -> &'static str {
    match c {
        Counter::Mmap => "mmap",
        Counter::Munmap => "munmap",
        Counter::Brk => "brk",
        Counter::Read => "read",
        Counter::Write => "write",
        Counter::Openat => "openat",
        Counter::Close => "close",
        Counter::Mprotect => "mprotect",
        Counter::Futex => "futex",
        Counter::SigProcMask => "sigprocmask",
        Counter::SigAction => "sigaction",
        Counter::Clock => "clock",
        Counter::Ioctl => "ioctl",
        Counter::Fstat => "fstat",
        Counter::Yield => "yield",
        Counter::Madvise => "madvise",
        Counter::Mremap => "mremap",
        Counter::Lseek => "lseek",
        Counter::Getrandom => "getrandom",
        Counter::Getpid => "getpid",
        Counter::Fcntl => "fcntl",
        Counter::Other => "other",
    }
}

fn ref_counter_name(c: reference::RefCounter) -> &'static str {
    use reference::RefCounter as R;
    match c {
        R::IncMmap => "mmap",
        R::IncMunmap => "munmap",
        R::IncBrk => "brk",
        R::IncRead => "read",
        R::IncWrite => "write",
        R::IncOpenat => "openat",
        R::IncClose => "close",
        R::IncMprotect => "mprotect",
        R::IncFutex => "futex",
        R::IncSigprocmask => "sigprocmask",
        R::IncSigaction => "sigaction",
        R::IncClock => "clock",
        R::IncIoctl => "ioctl",
        R::IncFstat => "fstat",
        R::IncYield => "yield",
        R::IncMadvise => "madvise",
        R::IncMremap => "mremap",
        R::IncLseek => "lseek",
        R::IncGetrandom => "getrandom",
        R::IncGetpid => "getpid",
        R::IncFcntl => "fcntl",
        R::IncOther => "other",
    }
}

/// A differential test that cannot fail is worse than no test. This one asserts
/// the comparison above actually discriminates: perturb one arm of the model
/// and the oracle must reject it.
#[test]
fn the_differential_would_catch_a_wrong_bucket() {
    // `pread64` shares the `read` bucket, and that is the kind of arm a tidy-up
    // silently drops. If the classifier ever returned `Other` for it, the
    // comparison above must fail — assert the two sides disagree when they
    // should, using the oracle itself as the judge.
    let want = reference::prologue(nr::PREAD64, true, false, true, true);
    assert_eq!(ref_counter_name(want.counter), "read");
    assert_ne!(ref_counter_name(want.counter), counter_name(Counter::Other));
    assert_eq!(counter_name(counter_for(nr::PREAD64)), ref_counter_name(want.counter));
}

/// `rt_sigreturn` is the one exemption, and it is exempt from **both** clears.
#[test]
fn rt_sigreturn_is_the_only_signal_state_exemption() {
    assert!(!clears_signal_state(nr::RT_SIGRETURN));
    for n in probe_numbers().filter(|n| *n != nr::RT_SIGRETURN) {
        assert!(clears_signal_state(n), "nr={n} should clear signal state");
    }
}

/// The debug-IO suppression list is only reachable with a flag no shipping
/// profile sets, so nothing else notices if it drifts. Pin it.
#[test]
fn debug_io_suppresses_exactly_the_high_rate_numbers() {
    let suppressed: Vec<u64> = probe_numbers().filter(|n| debug_io_suppressed(*n)).collect();
    assert_eq!(suppressed.len(), 23, "suppression list changed: {suppressed:?}");
    for n in [nr::READ, nr::WRITE, nr::FUTEX, nr::CLOCK_GETTIME] {
        // clock_gettime is deliberately NOT on the list even though it is a
        // high-rate call; this asserts the list as it is, not as it might be.
        assert_eq!(debug_io_suppressed(n), n != nr::CLOCK_GETTIME, "nr={n}");
    }
}

/// One clock read serves both hooks: if either is on, the prologue samples
/// `t0`, and if neither is, nothing in the epilogue reads a clock.
#[test]
fn timing_is_needed_exactly_when_a_hook_consumes_it() {
    for (stats, proc_log) in [(false, false), (true, false), (false, true), (true, true)] {
        let cfg = HookConfig {
            process_stats: stats,
            proc_log,
            debug_io: false,
            errno_diag: false,
            identity_audit: false,
            identity: IdentitySource::Reresolve,
        };
        let ex = Excursion::new(nr::GETPID, cfg);
        let pro = ex.prologue();
        let epi = ex.epilogue(1, false);
        assert_eq!(pro.need_timing, stats || proc_log);
        assert!(
            pro.need_timing || !(epi.record_time || epi.log),
            "epilogue consumes a clock the prologue never sampled: {stats} {proc_log}"
        );
    }
}

/// `kernel_profile_extreme` drops both recorders, and the timing read with
/// them — the one config where the excursion touches no clock at all.
#[test]
fn extreme_profile_reads_no_clock() {
    let ex = Excursion::new(nr::READ, HookConfig::extreme(IdentitySource::Reresolve));
    assert!(!ex.prologue().need_timing);
    let epi = ex.epilogue(7, true);
    assert!(!epi.record_time);
    assert!(!epi.log);
    assert!(!epi.errno_diag);
}

// ===========================================================================
// The slot-lifecycle enumeration
// ===========================================================================

/// **Finding A, decided.** Writing through the prologue's pointer after an
/// open-ended dispatch has a witness, and the witness is two peer operations
/// deep: retire the slot, reclaim it.
///
/// This is the result `IDENTITY_CACHE_SMP_REVIEW.md` could not get from a soak.
/// The same run that found `epi_stale=0` under SMP=4 thread churn is consistent
/// with this: the window is narrow, not absent, and a search says which.
#[test]
fn finding_a_hoisted_pointer_has_a_witness() {
    let policy = Policy {
        source: EpilogueSource::Hoisted,
        validation: Validation::Generation, // unused by `Hoisted` — nothing is checked
    };
    let w = search(policy, slot::MAX_DEPTH).expect("hoisting the pointer must have a witness");
    assert_eq!(w.fault, Fault::WriteAfterFree);
    assert_eq!(w.depth(), 2, "expected the minimal retire+reclaim witness");
    assert_eq!(w.steps().collect::<Vec<_>>(), vec![Op::Retire(0), Op::Reclaim(0)]);
}

/// **Finding A's fix, decided.** Re-reading the cache in the epilogue has no
/// witness anywhere in the search — the generation check turns the retired slot
/// into a miss and the writes are skipped, exactly as the pre-cache epilogue's
/// `lookup_process_shared` returning `None` did.
#[test]
fn finding_a_rereading_the_cache_has_none() {
    let policy = Policy { source: EpilogueSource::Reread, validation: Validation::Generation };
    assert_eq!(search(policy, slot::MAX_DEPTH), None);
}

/// **Finding B, decided.** `ACTIVE`-only validation — what `identity_get` did
/// when the review was written — has a witness: retire, reclaim, and the slot
/// is claimed again, at which point `ACTIVE` is true of a different process.
#[test]
fn finding_b_active_only_validation_has_a_witness() {
    let policy = Policy { source: EpilogueSource::Reread, validation: Validation::ActiveOnly };
    let w = search(policy, slot::MAX_DEPTH).expect("ACTIVE-only must have a witness");
    assert_eq!(w.depth(), 3);
    assert_eq!(
        w.steps().collect::<Vec<_>>(),
        vec![Op::Retire(0), Op::Reclaim(0), Op::Claim(0)]
    );
    // The recycled address makes this the silent kind: a live stranger's
    // `Process`, not free memory.
    assert_eq!(w.fault, Fault::WriteToWrongOccupant);
}

/// **The scheme that shipped is sound in the model.** No interleaving reaches a
/// bad write under `state == ACTIVE && SLOT_GEN == stamp`.
#[test]
fn generation_validation_has_no_witness() {
    let policy = Policy { source: EpilogueSource::Reread, validation: Validation::Generation };
    assert_eq!(search(policy, slot::MAX_DEPTH), None);
}

/// **The cheap-looking fix is unsound, mechanically.** The review argues
/// pointer-equality fails because `Process` is a fixed-size allocation and the
/// allocator can hand the same address to the next occupant. The search
/// produces that interleaving instead of arguing for it.
#[test]
fn pointer_only_validation_has_a_witness() {
    let policy = Policy { source: EpilogueSource::Reread, validation: Validation::PointerOnly };
    let w = search(policy, slot::MAX_DEPTH).expect("pointer-only must have a witness");
    assert_eq!(w.fault, Fault::WriteToWrongOccupant);
    assert_eq!(
        w.steps().collect::<Vec<_>>(),
        vec![Op::Retire(0), Op::Reclaim(0), Op::Claim(0)]
    );
}

/// **The alternative is sound too**, which is what makes it an alternative
/// rather than a mistake: adding the pid check to the pointer check closes it,
/// at the cost of a load on the `Process` cache line the generation scheme
/// never touches.
#[test]
fn pointer_and_pid_validation_has_no_witness() {
    let policy = Policy { source: EpilogueSource::Reread, validation: Validation::PointerAndPid };
    assert_eq!(search(policy, slot::MAX_DEPTH), None);
}

/// The model's allocator must actually recycle addresses, or every "sound"
/// verdict above is vacuous — `PointerOnly` would pass for the wrong reason.
#[test]
fn the_model_allocator_really_reuses_addresses() {
    let mut w = World::with_active(0);
    let first = w.occupant(0).expect("claimed");
    w.apply(Op::Retire(0));
    w.apply(Op::Reclaim(0));
    w.apply(Op::Claim(0));
    let second = w.occupant(0).expect("re-claimed");
    assert_eq!(second.addr, first.addr, "address must be handed straight back");
    assert_ne!(second.pid, first.pid, "pid must be fresh");
    assert_ne!(second.id, first.id, "and it must be a different incarnation");
}

/// The search must be capable of failing: give it a policy that is obviously
/// broken and confirm it reports a fault, and give the checker a safe write and
/// confirm it does not. A search that returns `None` for everything would make
/// every soundness test above meaningless.
#[test]
fn the_search_discriminates() {
    // Broken: no validation at all, and the slot does get freed.
    assert!(
        search(
            Policy { source: EpilogueSource::Hoisted, validation: Validation::PointerAndPid },
            slot::MAX_DEPTH,
        )
        .is_some(),
        "an unvalidated write must be reported"
    );
    // And a write through a live occupant is not a fault.
    let w = World::with_active(0);
    let o = w.occupant(0).expect("claimed");
    assert!(w.write_is_safe(o.addr, o.id));
    assert!(!w.write_is_safe(o.addr, o.id + 99), "wrong incarnation must not be safe");
}

/// The generation is bumped while the slot is RETIRED, which is the ordering
/// the whole scheme rests on: a reader that sees `ACTIVE` can never be looking
/// at a stamp from before the bump.
#[test]
fn the_generation_bump_happens_under_a_non_active_state() {
    let mut w = World::with_active(0);
    let stamp = w.stamp(0).expect("claimed");
    w.apply(Op::Retire(0));
    assert_eq!(w.state(0), SlotState::Retired);
    // Still ACTIVE-free, so the reader misses on state alone.
    assert_eq!(w.validate(stamp, Validation::Generation), None);
    w.apply(Op::Reclaim(0));
    assert_eq!(w.state(0), SlotState::Free);
    w.apply(Op::Claim(0));
    assert_eq!(w.state(0), SlotState::Active);
    // Now ACTIVE again — and only the generation separates the two occupants.
    assert_eq!(w.validate(stamp, Validation::ActiveOnly), Some((stamp.addr, stamp.pid)));
    assert_eq!(w.validate(stamp, Validation::Generation), None);
}

/// Enumeration bounds a claim; it does not measure one. Pin the search space so
/// a later "no witness" result cannot quietly come from a shrunken search.
#[test]
fn the_search_space_is_what_it_claims_to_be() {
    assert_eq!(ALL_OPS.len(), slot::SLOTS * 3);
    assert_eq!(slot::MAX_DEPTH, 6);
    // Every op is enabled in some reachable state, so none of them is dead
    // weight padding the branching factor.
    for op in ALL_OPS {
        let mut w = World::with_active(0);
        let reachable = match op {
            Op::Claim(s) => {
                if s == 0 {
                    w.apply(Op::Retire(0));
                    w.apply(Op::Reclaim(0));
                }
                w.enabled(op)
            }
            Op::Retire(s) => {
                if s == 1 {
                    w.apply(Op::Claim(1));
                }
                w.enabled(op)
            }
            Op::Reclaim(s) => {
                if s == 1 {
                    w.apply(Op::Claim(1));
                }
                w.apply(Op::Retire(s));
                w.enabled(op)
            }
        };
        assert!(reachable, "{op:?} is never enabled — the search space is smaller than it looks");
    }
}
