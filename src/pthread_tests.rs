//! pthread / threading-API conformance tests (boot self-tests)
//!
//! These exercise the kernel side of the POSIX threading + signal API the way a
//! real multi-threaded program (rustc/rayon) does, to catch the class of bug that
//! went undetected for a long time and only surfaced as an intermittent self-host
//! crash — the per-thread signal-mask regression (docs/AKUMA_SELF_HOSTING.md §7k.3,
//! docs/SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md §D).
//!
//! Coverage (kernel-testable subset; register-integrity-under-signal-storm and
//! handler/sigreturn round-trips need a real userspace process and live in
//! `userspace/forktest` / a `userspace/pthread_suite`):
//!   - per-thread signal mask: independence between siblings, the exact §7k.3
//!     "sibling unblock clears my block" scenario, fresh-slot starts empty,
//!     clone seeds from creator;
//!   - `rt_sigprocmask` semantics (BLOCK/UNBLOCK/SETMASK) + validation
//!     (sigsetsize, bad `how`, EFAULT, SIGKILL/SIGSTOP can't be blocked);
//!   - pending-signal targeting/masking (`tkill`/`tgkill`, `pend`/`take`),
//!     SIGKILL/SIGSTOP bypass the mask, lowest-numbered first;
//!   - `sigaltstack` per-thread isolation + validation;
//!   - `gettid` uniqueness, `rt_sigaction`/`rt_sigtimedwait` validation,
//!     MAX_SIGNALS=64 boundary.
//!
//! Like the other boot suites these call `handle_syscall(..)` directly with
//! `BYPASS_VALIDATION` so kernel-stack pointers pass the user-pointer check, and
//! use `assert!` so a regression halts the boot (the point: catch it up front).
//!
//! Cleanliness: every test restores the calling thread's mask to 0, clears its
//! pending signals, and disables its sigaltstack so the suite leaves the boot
//! thread (tid 0) — which SSH/networking run on — in a pristine state. A final
//! safety reset in `run_all_tests` enforces this regardless of test outcome.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use akuma_exec::threading;

use crate::console;

// --- Syscall numbers (mirror src/syscall/mod.rs::nr) -----------------------
const NR_RT_SIGACTION: u64 = 134;
const NR_RT_SIGPROCMASK: u64 = 135;
const NR_RT_SIGTIMEDWAIT: u64 = 137;
const NR_SIGALTSTACK: u64 = 132;
const NR_TKILL: u64 = 130;
const NR_TGKILL: u64 = 131;
const NR_GETTID: u64 = 178;

// --- rt_sigprocmask `how` ---------------------------------------------------
const SIG_BLOCK: u64 = 0;
const SIG_UNBLOCK: u64 = 1;
const SIG_SETMASK: u64 = 2;

// --- sigaltstack flags ------------------------------------------------------
const SS_DISABLE: i32 = 2;

// --- Signal numbers ---------------------------------------------------------
const SIGKILL: u32 = 9;
const SIGUSR1: u32 = 10;
const SIGSEGV: u32 = 11;
const SIGUSR2: u32 = 12;
const SIGTERM: u32 = 15;
const SIGSTOP: u32 = 19;

// --- errnos -----------------------------------------------------------------
const EINVAL: u64 = (-22i64) as u64;
const EFAULT: u64 = (-14i64) as u64;
const ENOMEM: u64 = (-12i64) as u64;
const ENOSYS: u64 = (-38i64) as u64;

/// `sigset` bit for a signal number (POSIX: signal N → bit N-1).
const fn bit(sig: u32) -> u64 {
    1u64 << (sig - 1)
}

fn set_bypass(v: bool) {
    crate::syscall::BYPASS_VALIDATION.store(v, Ordering::Release);
}

// ============================================================================
// Thread-coordination helpers
// ============================================================================

/// Spin yielding until `flag` is set or `max_iters` elapse. Returns the final
/// value (so callers can assert it became true within the bound).
fn spin_until(flag: &AtomicBool, max_iters: usize) -> bool {
    for _ in 0..max_iters {
        if flag.load(Ordering::Acquire) {
            return true;
        }
        threading::yield_now();
    }
    flag.load(Ordering::Acquire)
}

/// In-thread wrappers around the real `rt_sigprocmask` syscall path. Crucially
/// the thread id is resolved *inside* the syscall (`thread_signal_mask()` →
/// `current_thread_id()`), so calling these from a spawned thread operates on
/// that thread's own mask slot — exactly the path the §7k.3 bug lived on.
fn sys_sigprocmask(how: u64, new_mask: u64) -> u64 {
    let set = new_mask;
    crate::syscall::handle_syscall(
        NR_RT_SIGPROCMASK,
        &[how, &raw const set as u64, 0, 8, 0, 0],
    )
}

/// Query the calling thread's current mask via the syscall (oldset only).
fn sys_get_mask() -> u64 {
    let mut old: u64 = 0;
    crate::syscall::handle_syscall(
        NR_RT_SIGPROCMASK,
        &[0, 0, &raw mut old as u64, 8, 0, 0],
    );
    old
}

// ============================================================================
// Group A — rt_sigprocmask semantics & validation (single-threaded)
// ============================================================================

/// SETMASK installs exactly the given set; BLOCK ORs; UNBLOCK clears.
fn test_sigprocmask_block_unblock_setmask() {
    set_bypass(true);

    // SETMASK replaces wholesale.
    assert!(sys_sigprocmask(SIG_SETMASK, bit(SIGUSR1) | bit(SIGUSR2)) == 0);
    assert!(
        sys_get_mask() == bit(SIGUSR1) | bit(SIGUSR2),
        "SETMASK did not install the requested set"
    );

    // BLOCK adds SIGTERM without disturbing the rest.
    assert!(sys_sigprocmask(SIG_BLOCK, bit(SIGTERM)) == 0);
    assert!(
        sys_get_mask() == bit(SIGUSR1) | bit(SIGUSR2) | bit(SIGTERM),
        "BLOCK did not OR the new bit"
    );

    // UNBLOCK clears just SIGUSR1.
    assert!(sys_sigprocmask(SIG_UNBLOCK, bit(SIGUSR1)) == 0);
    assert!(
        sys_get_mask() == bit(SIGUSR2) | bit(SIGTERM),
        "UNBLOCK did not clear exactly the requested bit"
    );

    // Restore.
    assert!(sys_sigprocmask(SIG_SETMASK, 0) == 0);
    set_bypass(false);
    console::print("  [PASS] test_sigprocmask_block_unblock_setmask\n");
}

/// SIGKILL (9) and SIGSTOP (19) can never be blocked, even via SETMASK.
fn test_sigprocmask_cannot_block_kill_stop() {
    set_bypass(true);

    assert!(sys_sigprocmask(SIG_SETMASK, bit(SIGKILL) | bit(SIGSTOP) | bit(SIGUSR1)) == 0);
    let m = sys_get_mask();
    assert!(m & bit(SIGKILL) == 0, "SIGKILL must not be blockable");
    assert!(m & bit(SIGSTOP) == 0, "SIGSTOP must not be blockable");
    assert!(m & bit(SIGUSR1) != 0, "blockable signal was lost");

    // BLOCK of KILL/STOP is also a no-op for those bits.
    assert!(sys_sigprocmask(SIG_BLOCK, bit(SIGKILL) | bit(SIGSTOP)) == 0);
    let m = sys_get_mask();
    assert!(m & (bit(SIGKILL) | bit(SIGSTOP)) == 0);

    assert!(sys_sigprocmask(SIG_SETMASK, 0) == 0);
    set_bypass(false);
    console::print("  [PASS] test_sigprocmask_cannot_block_kill_stop\n");
}

/// sigsetsize != 8 and an invalid `how` (with a set provided) are EINVAL.
fn test_sigprocmask_validation() {
    set_bypass(true);

    let set: u64 = bit(SIGUSR1);
    // Wrong sigsetsize.
    let r = crate::syscall::handle_syscall(
        NR_RT_SIGPROCMASK,
        &[SIG_SETMASK, &raw const set as u64, 0, 4, 0, 0],
    );
    assert!(r == EINVAL, "bad sigsetsize: expected EINVAL got {r:#x}");

    // Invalid `how` with a set supplied.
    let r = crate::syscall::handle_syscall(
        NR_RT_SIGPROCMASK,
        &[99, &raw const set as u64, 0, 8, 0, 0],
    );
    assert!(r == EINVAL, "bad how: expected EINVAL got {r:#x}");

    set_bypass(false);

    // EFAULT: a non-null but unmapped set pointer with validation ON. Use the
    // top of the user VA range — RAM-independent: a fixed low address like
    // 0xdead_0000 lands *inside* the identity-mapped RAM at large MEMORY (≥4 GB)
    // and would validate as mapped, so the test must not hardcode below RAM.
    let bad_ptr = crate::syscall::user_va_limit_value() - 0x1000;
    let r = crate::syscall::handle_syscall(
        NR_RT_SIGPROCMASK,
        &[SIG_SETMASK, bad_ptr, 0, 8, 0, 0],
    );
    assert!(r == EFAULT, "bad set ptr: expected EFAULT got {r:#x}");

    console::print("  [PASS] test_sigprocmask_validation\n");
}

// ============================================================================
// Group B — per-thread signal mask (the §7k.3 regression class)
// ============================================================================

/// Two siblings install different masks; neither sees the other's. This is the
/// core POSIX "signal masks are per-thread" invariant that the per-process mask
/// bug violated.
fn test_per_thread_mask_independent() {
    static A_SET: AtomicBool = AtomicBool::new(false);
    static B_SET: AtomicBool = AtomicBool::new(false);
    static A_RESULT: AtomicU64 = AtomicU64::new(0);
    static B_RESULT: AtomicU64 = AtomicU64::new(0);
    static A_DONE: AtomicBool = AtomicBool::new(false);
    static B_DONE: AtomicBool = AtomicBool::new(false);

    A_SET.store(false, Ordering::SeqCst);
    B_SET.store(false, Ordering::SeqCst);
    A_RESULT.store(0, Ordering::SeqCst);
    B_RESULT.store(0, Ordering::SeqCst);
    A_DONE.store(false, Ordering::SeqCst);
    B_DONE.store(false, Ordering::SeqCst);

    let mask_a = bit(SIGUSR1) | bit(SIGUSR2);
    let mask_b = bit(SIGTERM);

    threading::spawn_fn(move || {
        set_bypass(true);
        sys_sigprocmask(SIG_SETMASK, mask_a);
        A_SET.store(true, Ordering::Release);
        spin_until(&B_SET, 5000); // wait until B has installed its mask
        // Re-read AFTER the sibling changed its own mask: must be unchanged.
        A_RESULT.store(sys_get_mask(), Ordering::Release);
        sys_sigprocmask(SIG_SETMASK, 0);
        set_bypass(false);
        A_DONE.store(true, Ordering::Release);
        threading::mark_current_terminated();
        loop { threading::yield_now(); }
    })
    .expect("spawn A failed");

    threading::spawn_fn(move || {
        set_bypass(true);
        spin_until(&A_SET, 5000);
        sys_sigprocmask(SIG_SETMASK, mask_b);
        B_SET.store(true, Ordering::Release);
        // Give A a chance to re-read while our mask is installed.
        for _ in 0..50 { threading::yield_now(); }
        B_RESULT.store(sys_get_mask(), Ordering::Release);
        sys_sigprocmask(SIG_SETMASK, 0);
        set_bypass(false);
        B_DONE.store(true, Ordering::Release);
        threading::mark_current_terminated();
        loop { threading::yield_now(); }
    })
    .expect("spawn B failed");

    assert!(spin_until(&A_DONE, 20000), "thread A never finished");
    assert!(spin_until(&B_DONE, 20000), "thread B never finished");

    assert!(
        A_RESULT.load(Ordering::Acquire) == mask_a,
        "thread A's mask was clobbered by sibling B (per-process mask bug!)"
    );
    assert!(
        B_RESULT.load(Ordering::Acquire) == mask_b,
        "thread B's mask was clobbered by sibling A (per-process mask bug!)"
    );
    console::print("  [PASS] test_per_thread_mask_independent\n");
}

/// The exact §7k.3 scenario: thread A BLOCKs SIGUSR1; sibling B issues an
/// UNBLOCK of SIGUSR1 on its *own* mask. A's block must survive. Under the old
/// shared-mask bug, B's unblock cleared A's block → SIGUSR1 delivered into A's
/// critical section.
fn test_sibling_unblock_does_not_clear_my_block() {
    static A_BLOCKED: AtomicBool = AtomicBool::new(false);
    static B_UNBLOCKED: AtomicBool = AtomicBool::new(false);
    static A_RESULT: AtomicU64 = AtomicU64::new(0);
    static A_DONE: AtomicBool = AtomicBool::new(false);
    static B_DONE: AtomicBool = AtomicBool::new(false);

    A_BLOCKED.store(false, Ordering::SeqCst);
    B_UNBLOCKED.store(false, Ordering::SeqCst);
    A_RESULT.store(0, Ordering::SeqCst);
    A_DONE.store(false, Ordering::SeqCst);
    B_DONE.store(false, Ordering::SeqCst);

    threading::spawn_fn(|| {
        set_bypass(true);
        sys_sigprocmask(SIG_BLOCK, bit(SIGUSR1));
        A_BLOCKED.store(true, Ordering::Release);
        spin_until(&B_UNBLOCKED, 5000);
        A_RESULT.store(sys_get_mask(), Ordering::Release);
        sys_sigprocmask(SIG_SETMASK, 0);
        set_bypass(false);
        A_DONE.store(true, Ordering::Release);
        threading::mark_current_terminated();
        loop { threading::yield_now(); }
    })
    .expect("spawn A failed");

    threading::spawn_fn(|| {
        set_bypass(true);
        spin_until(&A_BLOCKED, 5000);
        // B never blocked SIGUSR1; unblocking it should affect only B's mask.
        sys_sigprocmask(SIG_UNBLOCK, bit(SIGUSR1));
        set_bypass(false);
        B_UNBLOCKED.store(true, Ordering::Release);
        B_DONE.store(true, Ordering::Release);
        threading::mark_current_terminated();
        loop { threading::yield_now(); }
    })
    .expect("spawn B failed");

    assert!(spin_until(&A_DONE, 20000), "thread A never finished");
    assert!(spin_until(&B_DONE, 20000), "thread B never finished");

    assert!(
        A_RESULT.load(Ordering::Acquire) & bit(SIGUSR1) != 0,
        "sibling's UNBLOCK cleared my SIGUSR1 block — the §7k.3 shared-mask bug"
    );
    console::print("  [PASS] test_sibling_unblock_does_not_clear_my_block\n");
}

/// A freshly spawned thread always starts with an empty mask, regardless of what
/// the thread that previously held its (recycled) slot had blocked.
fn test_fresh_thread_starts_with_empty_mask() {
    static FIRST_DONE: AtomicBool = AtomicBool::new(false);
    static SECOND_MASK: AtomicU64 = AtomicU64::new(u64::MAX); // sentinel
    static SECOND_DONE: AtomicBool = AtomicBool::new(false);

    FIRST_DONE.store(false, Ordering::SeqCst);
    SECOND_MASK.store(u64::MAX, Ordering::SeqCst);
    SECOND_DONE.store(false, Ordering::SeqCst);

    // First thread dirties its slot's mask, then exits (slot becomes recyclable).
    threading::spawn_fn(|| {
        set_bypass(true);
        sys_sigprocmask(SIG_SETMASK, bit(SIGUSR1) | bit(SIGUSR2) | bit(SIGTERM));
        set_bypass(false);
        FIRST_DONE.store(true, Ordering::Release);
        threading::mark_current_terminated();
        loop { threading::yield_now(); }
    })
    .expect("spawn first failed");

    assert!(spin_until(&FIRST_DONE, 20000), "first thread never finished");
    // Let it actually terminate/recycle.
    for _ in 0..200 { threading::yield_now(); }

    // Second thread records the mask it sees *before touching it*.
    threading::spawn_fn(|| {
        let m = threading::thread_signal_mask();
        SECOND_MASK.store(m, Ordering::Release);
        SECOND_DONE.store(true, Ordering::Release);
        threading::mark_current_terminated();
        loop { threading::yield_now(); }
    })
    .expect("spawn second failed");

    assert!(spin_until(&SECOND_DONE, 20000), "second thread never finished");
    assert!(
        SECOND_MASK.load(Ordering::Acquire) == 0,
        "a freshly spawned thread inherited a stale signal mask"
    );
    console::print("  [PASS] test_fresh_thread_starts_with_empty_mask\n");
}

/// The clone path seeds the child's mask from the creator (POSIX inheritance).
/// We exercise the exact mechanism `src/syscall/proc.rs` uses at
/// `clone(CLONE_THREAD|CLONE_VM)` — `seed_thread_signal_mask` + the per-thread
/// readback — on the calling thread's own slot so no live sibling is disturbed.
fn test_clone_seed_inherits_mask() {
    let slot = threading::current_thread_id();
    let saved = threading::thread_signal_mask();

    let seeded = bit(SIGUSR1) | bit(SIGTERM);
    threading::seed_thread_signal_mask(slot, seeded);
    assert!(
        threading::thread_signal_mask_of(slot) == seeded,
        "seed_thread_signal_mask did not take"
    );
    assert!(
        threading::thread_signal_mask() == seeded,
        "current thread did not observe the seeded mask"
    );

    threading::seed_thread_signal_mask(slot, saved); // restore
    console::print("  [PASS] test_clone_seed_inherits_mask\n");
}

// ============================================================================
// Group C — pending-signal targeting, masking, ordering
// ============================================================================

/// `pend_signal_for_thread` deposits on the targeted slot only.
fn test_pending_signal_targets_correct_thread() {
    static A_TID: AtomicU32 = AtomicU32::new(u32::MAX);
    static B_TID: AtomicU32 = AtomicU32::new(u32::MAX);
    static A_READY: AtomicBool = AtomicBool::new(false);
    static B_READY: AtomicBool = AtomicBool::new(false);
    static FINISH: AtomicBool = AtomicBool::new(false);

    A_TID.store(u32::MAX, Ordering::SeqCst);
    B_TID.store(u32::MAX, Ordering::SeqCst);
    A_READY.store(false, Ordering::SeqCst);
    B_READY.store(false, Ordering::SeqCst);
    FINISH.store(false, Ordering::SeqCst);

    let parker = |tid_slot: &'static AtomicU32, ready: &'static AtomicBool| {
        tid_slot.store(threading::current_thread_id() as u32, Ordering::Release);
        ready.store(true, Ordering::Release);
        // Park until the test is done (bounded so a failure can't wedge a slot).
        for _ in 0..200_000 {
            if FINISH.load(Ordering::Acquire) { break; }
            threading::yield_now();
        }
        threading::mark_current_terminated();
        loop { threading::yield_now(); }
    };

    threading::spawn_fn(move || parker(&A_TID, &A_READY)).expect("spawn A failed");
    threading::spawn_fn(move || parker(&B_TID, &B_READY)).expect("spawn B failed");

    assert!(spin_until(&A_READY, 20000) && spin_until(&B_READY, 20000), "parkers not ready");
    let a = A_TID.load(Ordering::Acquire) as usize;
    let b = B_TID.load(Ordering::Acquire) as usize;
    assert!(a != b, "two threads share a slot id");

    threading::pend_signal_for_thread(a, SIGUSR1);
    assert!(
        threading::peek_pending_signal(a) == SIGUSR1,
        "signal not pending on the targeted thread"
    );
    assert!(
        threading::peek_pending_signal(b) == 0,
        "signal leaked onto a non-targeted sibling"
    );

    // Cleanup.
    threading::pend_signal_for_thread(a, 0);
    FINISH.store(true, Ordering::Release);
    for _ in 0..400 { threading::yield_now(); }
    console::print("  [PASS] test_pending_signal_targets_correct_thread\n");
}

/// `take_pending_signal` honours the mask; SIGKILL/SIGSTOP bypass it; the
/// lowest-numbered deliverable signal comes out first.
fn test_take_pending_respects_mask_and_order() {
    let slot = threading::current_thread_id();
    threading::pend_signal_for_thread(slot, 0); // clear

    // Masked signal is not delivered, but stays pending.
    threading::pend_signal_for_thread(slot, SIGUSR1);
    assert!(
        threading::take_pending_signal(bit(SIGUSR1)).is_none(),
        "a masked signal was delivered"
    );
    assert!(
        threading::take_pending_signal(0) == Some(SIGUSR1),
        "an unmasked pending signal was not delivered"
    );

    // SIGKILL bypasses a full mask.
    threading::pend_signal_for_thread(slot, SIGKILL);
    assert!(
        threading::take_pending_signal(u64::MAX) == Some(SIGKILL),
        "SIGKILL must bypass the signal mask"
    );

    // Lowest-numbered first.
    threading::pend_signal_for_thread(slot, SIGUSR2); // 12
    threading::pend_signal_for_thread(slot, SIGUSR1); // 10
    assert!(threading::take_pending_signal(0) == Some(SIGUSR1));
    assert!(threading::take_pending_signal(0) == Some(SIGUSR2));
    assert!(threading::take_pending_signal(0).is_none());

    threading::pend_signal_for_thread(slot, 0); // cleanup
    console::print("  [PASS] test_take_pending_respects_mask_and_order\n");
}

/// `tkill`/`tgkill` argument validation and the MAX_SIGNALS=64 boundary.
fn test_tkill_validation_and_signal_boundary() {
    let slot = threading::current_thread_id() as u64;

    // sig 0 is a no-op success (existence probe).
    assert!(crate::syscall::handle_syscall(NR_TKILL, &[slot, 0, 0, 0, 0, 0]) == 0);
    // Out-of-range signal → EINVAL.
    assert!(
        crate::syscall::handle_syscall(NR_TKILL, &[slot, 65, 0, 0, 0, 0]) == EINVAL,
        "sig 65 must be EINVAL"
    );
    // Signal 64 is the top valid signal — must NOT be EINVAL (guards the
    // MAX_SIGNALS boundary / off-by-one). With a Default handler and no fatal
    // disposition it returns 0.
    assert!(
        crate::syscall::handle_syscall(NR_TKILL, &[slot, 64, 0, 0, 0, 0]) == 0,
        "sig 64 must be accepted (MAX_SIGNALS boundary)"
    );
    // tgkill forwards with the same validation.
    assert!(crate::syscall::handle_syscall(NR_TGKILL, &[0, slot, 0, 0, 0, 0]) == 0);
    assert!(crate::syscall::handle_syscall(NR_TGKILL, &[0, slot, 65, 0, 0, 0]) == EINVAL);

    threading::pend_signal_for_thread(slot as usize, 0); // clear any stray pend
    console::print("  [PASS] test_tkill_validation_and_signal_boundary\n");
}

/// A fatal-by-default signal that is currently *blocked* on the target thread
/// must be pended (delivered later when unmasked), not dropped or acted on now.
/// Exercises the `sys_tkill` Default-handler + blocked branch on a parked
/// sibling (so a misfire can't kill the boot thread).
fn test_tkill_blocked_fatal_is_pended() {
    static TID: AtomicU32 = AtomicU32::new(u32::MAX);
    static READY: AtomicBool = AtomicBool::new(false);
    static FINISH: AtomicBool = AtomicBool::new(false);

    TID.store(u32::MAX, Ordering::SeqCst);
    READY.store(false, Ordering::SeqCst);
    FINISH.store(false, Ordering::SeqCst);

    threading::spawn_fn(|| {
        let me = threading::current_thread_id();
        // Block SIGTERM on *this* thread so the tkill below pends rather than
        // killing the group.
        threading::seed_thread_signal_mask(me, bit(SIGTERM));
        TID.store(me as u32, Ordering::Release);
        READY.store(true, Ordering::Release);
        for _ in 0..200_000 {
            if FINISH.load(Ordering::Acquire) { break; }
            threading::yield_now();
        }
        threading::seed_thread_signal_mask(me, 0); // restore before recycle
        threading::mark_current_terminated();
        loop { threading::yield_now(); }
    })
    .expect("spawn failed");

    assert!(spin_until(&READY, 20000), "target thread not ready");
    let tid = TID.load(Ordering::Acquire) as usize;
    // Safety: confirm the block is installed before sending a fatal signal.
    assert!(
        threading::thread_signal_mask_of(tid) & bit(SIGTERM) != 0,
        "precondition: SIGTERM not blocked on target — refusing to send"
    );

    let r = crate::syscall::handle_syscall(NR_TKILL, &[tid as u64, u64::from(SIGTERM), 0, 0, 0, 0]);
    assert!(r == 0, "tkill returned {r:#x}");
    assert!(
        threading::peek_pending_signal(tid) == SIGTERM,
        "a blocked fatal signal was not pended for later delivery"
    );

    threading::pend_signal_for_thread(tid, 0);
    FINISH.store(true, Ordering::Release);
    for _ in 0..400 { threading::yield_now(); }
    console::print("  [PASS] test_tkill_blocked_fatal_is_pended\n");
}

// NOTE: gap-2 (adding SIGUSR1/SIGUSR2/RT signals to signal_is_fatal_default)
// was REVERTED — making those fatal-by-default kills the in-VM rustc self-host
// build, which storms SIGUSR1 (~10,400 tkills). See report_known_gaps + §7k.5.
// `test_tkill_blocked_fatal_is_pended` above already covers the genuinely-fatal
// case (SIGTERM) for the pend-when-blocked path.

/// §7k.5 gap-3 fix: `rt_sigsuspend` validates its arguments.
fn test_rt_sigsuspend_validation() {
    set_bypass(true);
    let mask: u64 = 0;
    // Wrong sigsetsize → EINVAL (checked before the pointer).
    let r = crate::syscall::handle_syscall(133, &[&raw const mask as u64, 4, 0, 0, 0, 0]);
    assert!(r == EINVAL, "bad sigsetsize: expected EINVAL got {r:#x}");
    set_bypass(false);
    // Bad pointer with validation ON → EFAULT. Top of user VA (RAM-independent;
    // see the note in test_sigprocmask_validation).
    let bad_ptr = crate::syscall::user_va_limit_value() - 0x1000;
    let r = crate::syscall::handle_syscall(133, &[bad_ptr, 8, 0, 0, 0, 0]);
    assert!(r == EFAULT, "bad mask ptr: expected EFAULT got {r:#x}");
    console::print("  [PASS] test_rt_sigsuspend_validation\n");
}

/// §7k.5 gap-3 fix: `rt_sigsuspend` blocks until a signal arrives, then returns
/// −EINTR (never 0, the old stub's bug). A parked thread suspends with an
/// all-but-nothing mask; the main thread pends a signal to wake it.
fn test_rt_sigsuspend_blocks_then_eintr() {
    static ENTERED: AtomicBool = AtomicBool::new(false);
    static RET: AtomicU64 = AtomicU64::new(0);
    static DONE: AtomicBool = AtomicBool::new(false);
    static TID: AtomicU32 = AtomicU32::new(u32::MAX);

    ENTERED.store(false, Ordering::SeqCst);
    RET.store(0, Ordering::SeqCst);
    DONE.store(false, Ordering::SeqCst);
    TID.store(u32::MAX, Ordering::SeqCst);

    threading::spawn_fn(|| {
        set_bypass(true);
        TID.store(threading::current_thread_id() as u32, Ordering::Release);
        ENTERED.store(true, Ordering::Release);
        // Suspend mask = 0 (block nothing): any pending signal wakes us.
        let mask: u64 = 0;
        let r = crate::syscall::handle_syscall(133, &[&raw const mask as u64, 8, 0, 0, 0, 0]);
        RET.store(r, Ordering::Release);
        // Clean up any state the suspend left on this (recyclable) slot.
        let me = threading::current_thread_id();
        threading::set_thread_signal_mask(0);
        threading::pend_signal_for_thread(me, 0);
        let _ = threading::take_restore_sigmask();
        set_bypass(false);
        DONE.store(true, Ordering::Release);
        threading::mark_current_terminated();
        loop { threading::yield_now(); }
    })
    .expect("spawn failed");

    assert!(spin_until(&ENTERED, 20000), "suspender never entered");
    let tid = TID.load(Ordering::Acquire) as usize;
    // Give it a moment to actually be parked in the suspend loop, and confirm it
    // has NOT returned yet (it must block, unlike the old `=> 0` stub).
    for _ in 0..50 { threading::yield_now(); }
    assert!(!DONE.load(Ordering::Acquire), "rt_sigsuspend returned without blocking");

    // Wake it with a pending signal.
    threading::pend_signal_for_thread(tid, SIGUSR1);

    assert!(spin_until(&DONE, 20000), "rt_sigsuspend never woke");
    let r = RET.load(Ordering::Acquire);
    assert!(
        r == (-4i64) as u64, // EINTR
        "rt_sigsuspend must return -EINTR, got {r:#x}"
    );
    console::print("  [PASS] test_rt_sigsuspend_blocks_then_eintr\n");
}

// ============================================================================
// Group D — sigaltstack
// ============================================================================

#[repr(C)]
struct StackT {
    sp: u64,
    flags: i32,
    _pad: i32,
    size: u64,
}

/// sigaltstack: default disabled, set/get round-trip, min-size, and disable.
fn test_sigaltstack_lifecycle() {
    set_bypass(true);
    let slot = threading::current_thread_id();
    // Snapshot to restore afterwards (boot thread might have one configured).
    let (saved_sp, saved_size, saved_flags) = threading::get_sigaltstack(slot);

    // Start from a known disabled state.
    threading::set_sigaltstack(slot, 0, 0, SS_DISABLE);
    let mut out = StackT { sp: 0, flags: 0, _pad: 0, size: 0 };
    assert!(
        crate::syscall::handle_syscall(NR_SIGALTSTACK, &[0, &raw mut out as u64, 0, 0, 0, 0]) == 0
    );
    assert!(out.flags == SS_DISABLE && out.sp == 0, "default sigaltstack not disabled");

    // Set a valid stack.
    let mut backing = [0u8; 8192];
    let ss = StackT { sp: backing.as_mut_ptr() as u64, flags: 0, _pad: 0, size: 8192 };
    assert!(
        crate::syscall::handle_syscall(NR_SIGALTSTACK, &[&raw const ss as u64, 0, 0, 0, 0, 0]) == 0
    );
    let mut got = StackT { sp: 0, flags: 0, _pad: 0, size: 0 };
    crate::syscall::handle_syscall(NR_SIGALTSTACK, &[0, &raw mut got as u64, 0, 0, 0, 0]);
    assert!(got.sp == ss.sp && got.size == 8192, "sigaltstack set/get round-trip failed");

    // Too-small stack → ENOMEM.
    let small = StackT { sp: backing.as_mut_ptr() as u64, flags: 0, _pad: 0, size: 1024 };
    assert!(
        crate::syscall::handle_syscall(NR_SIGALTSTACK, &[&raw const small as u64, 0, 0, 0, 0, 0]) == ENOMEM,
        "undersized sigaltstack must be ENOMEM"
    );

    // Disable.
    let dis = StackT { sp: 0, flags: SS_DISABLE, _pad: 0, size: 0 };
    crate::syscall::handle_syscall(NR_SIGALTSTACK, &[&raw const dis as u64, 0, 0, 0, 0, 0]);
    let (_, _, f) = threading::get_sigaltstack(slot);
    assert!(f == SS_DISABLE, "SS_DISABLE was not honoured");

    threading::set_sigaltstack(slot, saved_sp, saved_size, saved_flags); // restore
    set_bypass(false);
    console::print("  [PASS] test_sigaltstack_lifecycle\n");
}

/// Each thread's alternate signal stack is independent.
fn test_sigaltstack_per_thread_isolation() {
    static A_TID: AtomicU32 = AtomicU32::new(u32::MAX);
    static B_TID: AtomicU32 = AtomicU32::new(u32::MAX);
    static A_DONE: AtomicBool = AtomicBool::new(false);
    static B_DONE: AtomicBool = AtomicBool::new(false);

    A_TID.store(u32::MAX, Ordering::SeqCst);
    B_TID.store(u32::MAX, Ordering::SeqCst);
    A_DONE.store(false, Ordering::SeqCst);
    B_DONE.store(false, Ordering::SeqCst);

    let worker = |tid_out: &'static AtomicU32, done: &'static AtomicBool, sp: u64| {
        let me = threading::current_thread_id();
        threading::set_sigaltstack(me, sp, 8192, 0);
        tid_out.store(me as u32, Ordering::Release);
        done.store(true, Ordering::Release);
        threading::mark_current_terminated();
        loop { threading::yield_now(); }
    };

    threading::spawn_fn(move || worker(&A_TID, &A_DONE, 0x4000_0000))
        .expect("spawn A failed");
    threading::spawn_fn(move || worker(&B_TID, &B_DONE, 0x5000_0000))
        .expect("spawn B failed");

    assert!(spin_until(&A_DONE, 20000) && spin_until(&B_DONE, 20000), "workers not done");
    let a = A_TID.load(Ordering::Acquire) as usize;
    let b = B_TID.load(Ordering::Acquire) as usize;
    let (sp_a, _, _) = threading::get_sigaltstack(a);
    let (sp_b, _, _) = threading::get_sigaltstack(b);
    assert!(sp_a == 0x4000_0000, "thread A's sigaltstack was wrong/clobbered");
    assert!(sp_b == 0x5000_0000, "thread B's sigaltstack was wrong/clobbered");

    // Cleanup: the slots will reset on recycle, but disable explicitly.
    threading::set_sigaltstack(a, 0, 0, SS_DISABLE);
    threading::set_sigaltstack(b, 0, 0, SS_DISABLE);
    console::print("  [PASS] test_sigaltstack_per_thread_isolation\n");
}

// ============================================================================
// Group E — identity & remaining validation
// ============================================================================

/// `gettid` is unique per thread and equals `current_thread_id`.
fn test_gettid_unique_per_thread() {
    static A_TID: AtomicU32 = AtomicU32::new(u32::MAX);
    static B_TID: AtomicU32 = AtomicU32::new(u32::MAX);
    static A_DONE: AtomicBool = AtomicBool::new(false);
    static B_DONE: AtomicBool = AtomicBool::new(false);

    A_TID.store(u32::MAX, Ordering::SeqCst);
    B_TID.store(u32::MAX, Ordering::SeqCst);
    A_DONE.store(false, Ordering::SeqCst);
    B_DONE.store(false, Ordering::SeqCst);

    let worker = |out: &'static AtomicU32, done: &'static AtomicBool| {
        let tid = crate::syscall::handle_syscall(NR_GETTID, &[0; 6]);
        assert!(
            tid == threading::current_thread_id() as u64,
            "gettid != current_thread_id"
        );
        out.store(tid as u32, Ordering::Release);
        done.store(true, Ordering::Release);
        threading::mark_current_terminated();
        loop { threading::yield_now(); }
    };

    threading::spawn_fn(move || worker(&A_TID, &A_DONE)).expect("spawn A failed");
    threading::spawn_fn(move || worker(&B_TID, &B_DONE)).expect("spawn B failed");

    assert!(spin_until(&A_DONE, 20000) && spin_until(&B_DONE, 20000), "workers not done");
    let main_tid = crate::syscall::handle_syscall(NR_GETTID, &[0; 6]) as u32;
    let a = A_TID.load(Ordering::Acquire);
    let b = B_TID.load(Ordering::Acquire);
    assert!(a != b && a != main_tid && b != main_tid, "thread ids are not unique");
    console::print("  [PASS] test_gettid_unique_per_thread\n");
}

/// `rt_sigaction` rejects signal 0, SIGKILL/SIGSTOP, and out-of-range numbers;
/// signal 64 is in range (returns ENOSYS in boot context, not EINVAL).
fn test_sigaction_validation() {
    // sig 0
    assert!(crate::syscall::handle_syscall(NR_RT_SIGACTION, &[0, 0, 0, 8, 0, 0]) == EINVAL);
    // SIGKILL / SIGSTOP can't be caught.
    assert!(
        crate::syscall::handle_syscall(NR_RT_SIGACTION, &[u64::from(SIGKILL), 0, 0, 8, 0, 0]) == EINVAL
    );
    assert!(
        crate::syscall::handle_syscall(NR_RT_SIGACTION, &[u64::from(SIGSTOP), 0, 0, 8, 0, 0]) == EINVAL
    );
    // Out of range.
    assert!(crate::syscall::handle_syscall(NR_RT_SIGACTION, &[65, 0, 0, 8, 0, 0]) == EINVAL);
    // Top valid signal: in range, so it passes validation and only then fails
    // for lack of a current process in boot context — ENOSYS, NOT EINVAL.
    let r = crate::syscall::handle_syscall(NR_RT_SIGACTION, &[64, 0, 0, 8, 0, 0]);
    assert!(r == ENOSYS || r == 0, "sig 64 wrongly rejected as out-of-range: {r:#x}");
    console::print("  [PASS] test_sigaction_validation\n");
}

/// `rt_sigtimedwait` rejects a wrong sigsetsize before anything else.
fn test_rt_sigtimedwait_validation() {
    set_bypass(true);
    let set: u64 = bit(SIGUSR1);
    let r = crate::syscall::handle_syscall(
        NR_RT_SIGTIMEDWAIT,
        &[&raw const set as u64, 0, 0, 4, 0, 0],
    );
    set_bypass(false);
    assert!(r == EINVAL, "bad sigsetsize: expected EINVAL got {r:#x}");
    console::print("  [PASS] test_rt_sigtimedwait_validation\n");
}

// ============================================================================
// Known non-compliances (informational — does not assert/halt)
// ============================================================================

/// Document, at boot, the conformance gaps the suite has *not* turned into hard
/// failures because they are accepted limitations today. Keeps them visible so
/// they aren't silently forgotten (see docs/AKUMA_SELF_HOSTING.md §7k.5).
fn report_known_gaps() {
    let slot = threading::current_thread_id();
    threading::pend_signal_for_thread(slot, 0);

    // RT signals are coalesced (bitset, not a queue): two pends of the same RT
    // signal collapse to one delivery and carry no siginfo payload.
    threading::pend_signal_for_thread(slot, 40);
    threading::pend_signal_for_thread(slot, 40);
    let first = threading::take_pending_signal(0);
    let second = threading::take_pending_signal(0);
    threading::pend_signal_for_thread(slot, 0);
    if first == Some(40) && second.is_none() {
        console::print("  [GAP ] RT-signal queuing: same-number signals coalesced (no siginfo queue) — feature, not yet implemented\n");
    }
    console::print("  [GAP ] signal_is_fatal_default stays conservative: SIGUSR1/2 + RT NOT fatal-by-default (making them fatal kills the rustc self-host build's SIGUSR1 storm; real fix is tkill handler attribution) — §7k.5\n");
    // Fixed this session (§7k.5): rt_sigsuspend block+EINTR (gap 3),
    // tgkill tgid/ESRCH check (gap 4). gap-2 reverted (see above). gap-1 open.
    let _ = SIGSEGV; // referenced for documentation symmetry
}

// ============================================================================
// Runner
// ============================================================================

pub fn run_all_tests() {
    console::print("\n=== pthread / threading-API conformance tests ===\n");

    // Group A: rt_sigprocmask semantics & validation
    test_sigprocmask_block_unblock_setmask();
    test_sigprocmask_cannot_block_kill_stop();
    test_sigprocmask_validation();

    // Group B: per-thread mask (the §7k.3 regression class)
    test_per_thread_mask_independent();
    test_sibling_unblock_does_not_clear_my_block();
    test_fresh_thread_starts_with_empty_mask();
    test_clone_seed_inherits_mask();

    // Group C: pending-signal targeting / masking / ordering
    test_pending_signal_targets_correct_thread();
    test_take_pending_respects_mask_and_order();
    test_tkill_validation_and_signal_boundary();
    test_tkill_blocked_fatal_is_pended();
    test_rt_sigsuspend_validation();
    test_rt_sigsuspend_blocks_then_eintr();

    // Group D: sigaltstack
    test_sigaltstack_lifecycle();
    test_sigaltstack_per_thread_isolation();

    // Group E: identity & remaining validation
    test_gettid_unique_per_thread();
    test_sigaction_validation();
    test_rt_sigtimedwait_validation();

    // Known gaps (informational)
    report_known_gaps();

    // Safety: leave the calling (boot) thread pristine for SSH/networking.
    let slot = threading::current_thread_id();
    set_bypass(true);
    sys_sigprocmask(SIG_SETMASK, 0);
    set_bypass(false);
    threading::pend_signal_for_thread(slot, 0);
    threading::set_sigaltstack(slot, 0, 0, SS_DISABLE);

    console::print("=== pthread tests complete ===\n");
}
