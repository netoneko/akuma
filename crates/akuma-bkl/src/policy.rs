//! BKL policy: the runtime toggles that decide whether a path takes the BKL.
//!
//! Five `no-bkl-*` phase toggles plus the per-syscall opt-out bitmap, moved out
//! of `src/smp_shared.rs` on 2026-09-01. Two more (`no-bkl-irq` and the
//! BKL-free EL0-preempt scheduler) went with the SGI scheduler's IRQ path, their
//! only reader, on 2026-11-11 (docs/archive/SGI_SCHEDULER_RESIDUE.md). They are pure policy state — a relaxed
//! atomic load each, with a paired setter that exists so a *same-binary* A/B can
//! flip the phase at runtime — so they belong with the protocol they gate rather
//! than with the SMP bring-up code they happened to be written next to.
//!
//! # Why the setters are not test-only
//!
//! Every `set_*` here is an A/B measurement handle and a runtime kill switch.
//! `docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md` runs each phase as a
//! source-toggled A/B on a byte-identical feature set; deleting a setter because
//! production never calls it would delete the ability to measure the phase.
//!
//! # What deliberately stayed behind
//!
//! `process_bkl_drop_enabled` — its atomic lives in
//! `akuma_exec::process::bkl_guard` because the guard is constructed inside
//! `fork_process`. `akuma-exec` depends on this crate, so pulling that toggle
//! down here would invert the edge. `src/smp_shared.rs` keeps it as a forwarder,
//! which is what makes "every BKL toggle is reachable from one module" still true
//! at the call sites.
//!
//! # The latching rule these are read under
//!
//! Each toggle is read ONCE at the entry of an excursion and latched for its
//! duration — the exit path must use the entry's decision, never a re-read. A
//! guard that re-reads can close a window it never opened. That rule lives at the
//! call sites (`VfsBklGuard`, `MmBklGuard`, `rust_sync_el0_handler`); this module
//! only owns the state.

use akuma_syscalls_linux::nr;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Runtime toggle (default on) for the M5b Stage 4a optimization: drop the BKL around a
/// file-backed fault's block-I/O fill pass so peer cores can enter the kernel while this
/// core waits on disk. Exposed so a boot self-test can A/B-measure its effect on
/// cross-core BKL contention (see `test_smp_shared_fault_parallelism`).
static FAULT_BKL_DROP_ENABLED: AtomicBool = AtomicBool::new(true);

/// Whether the file-fault block-I/O BKL-drop (M5b Stage 4a) is currently enabled.
#[inline]
pub fn fault_bkl_drop_enabled() -> bool {
    FAULT_BKL_DROP_ENABLED.load(Ordering::Relaxed)
}

/// Enable/disable the file-fault block-I/O BKL-drop at runtime (A/B measurement only).
pub fn set_fault_bkl_drop_enabled(on: bool) {
    FAULT_BKL_DROP_ENABLED.store(on, Ordering::Relaxed);
}

/// Runtime toggle (default **on**) for the M5c hold-shortening optimization: drop the BKL
/// around execve's whole-file ELF read (`fs::read_file` in `do_execve`) so peer cores can
/// enter the kernel while this core waits on disk. That read runs BEFORE the process image
/// is touched and goes only through the VFS mount lookup (released before I/O), the ext2
/// block cache, and the block device — all their own locks, none BKL-protected — the same
/// lock profile the proven file-fault BKL-drop relies on. Exposed for A/B measurement.
static EXEC_BKL_DROP_ENABLED: AtomicBool = AtomicBool::new(true);

/// Whether the execve ELF-read BKL-drop (M5c hold-shortening) is currently enabled.
#[inline]
pub fn exec_bkl_drop_enabled() -> bool {
    EXEC_BKL_DROP_ENABLED.load(Ordering::Relaxed)
}

/// Enable/disable the execve ELF-read BKL-drop at runtime. Used by the A/B measurement
/// self-test `test_smp_shared_exec_parallelism`; also handy for interactive A/B.
pub fn set_exec_bkl_drop_enabled(on: bool) {
    EXEC_BKL_DROP_ENABLED.store(on, Ordering::Relaxed);
}

/// Runtime toggle (default **on**) for `no-bkl-vfs` (Phase 4 of
/// docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md) — drop the BKL for the whole duration
/// of every fs syscall (`sys_read`/`sys_write`/`sys_openat`/`sys_close`/...), relying on
/// the existing fine-grained `MOUNT_TABLE` / `Ext2Filesystem::state` (RwSpinlock) /
/// `block_cache` / `BLOCK_DEVICE` / `proc.fds.table` spinlocks for cross-core mutual
/// exclusion. The `VfsBklGuard` reads this at construct/drop time so an `smp-shared` boot
/// with the feature compiled in can still A/B against the BKL-held path without a
/// rebuild. Defaults **on** to match `no-bkl-network`'s post-validation default and the
/// other fs BKL-drop toggles (`FAULT_BKL_DROP_ENABLED`, `EXEC_BKL_DROP_ENABLED`); flip
/// off via `set_vfs_bkl_drop_enabled(false)` at boot for a measurement window.
static VFS_BKL_DROP_ENABLED: AtomicBool = AtomicBool::new(true);

/// Whether the VFS-syscall BKL-drop (`no-bkl-vfs`) is currently enabled.
#[inline]
pub fn vfs_bkl_drop_enabled() -> bool {
    VFS_BKL_DROP_ENABLED.load(Ordering::Relaxed)
}

/// Enable/disable the VFS-syscall BKL-drop at runtime. Used by A/B measurement; also
/// serves as a runtime kill-switch if a regression surfaces (the VFS BKL-drop, unlike
/// `no-bkl-network`, has no equivalent of the SSH-stall watchdog to self-detect).
pub fn set_vfs_bkl_drop_enabled(on: bool) {
    VFS_BKL_DROP_ENABLED.store(on, Ordering::Relaxed);
}

/// Runtime toggle (default **on**) for `no-bkl-mm` (Phase 5 of
/// docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md) — drop the BKL for the whole
/// duration of `sys_mprotect`/`sys_madvise`/`sys_munmap`/`sys_mremap`/`sys_mmap`,
/// relying on `Process::as_lock` (page tables), `Process::vm_lock` (`mmap_regions`
/// and the mmap free-list), `Process::lazy_regions`, PMM/`FRAME_TRACKER`, and
/// `SHARED_FILE_MAPPINGS` for cross-core mutual exclusion instead. `MmBklGuard`
/// reads this at construct/drop time (latched, same discipline as
/// `VFS_BKL_DROP_ENABLED`) so an `smp-shared` boot with the feature compiled in can
/// still A/B against the BKL-held path without a rebuild.
static MM_BKL_DROP_ENABLED: AtomicBool = AtomicBool::new(true);

/// Whether the mm-syscall BKL-drop (`no-bkl-mm`) is currently enabled.
#[inline]
pub fn mm_bkl_drop_enabled() -> bool {
    MM_BKL_DROP_ENABLED.load(Ordering::Relaxed)
}

/// Enable/disable the mm-syscall BKL-drop at runtime. Used by A/B measurement; also
/// serves as a runtime kill-switch, same as `set_vfs_bkl_drop_enabled`.
pub fn set_mm_bkl_drop_enabled(on: bool) {
    MM_BKL_DROP_ENABLED.store(on, Ordering::Relaxed);
}

/// Runtime toggle (default **on**) for `no-bkl-drivers` (Phase 6 of
/// docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md) — drop the BKL for the
/// device-driver syscall paths (`sys_getrandom`, `sys_read`/`sys_pread64` on
/// `/dev/urandom`, and `sys_write` on `/dev/dsp`), relying on each driver's own
/// fine-grained Spinlock — `RNG_DEVICE`, `SOUND_DEVICE` — for cross-core mutual
/// exclusion instead.
/// `DriverBklGuard` reads this at construct/drop time (latched, same discipline
/// as `VfsBklGuard` / `MmBklGuard`) so an `smp-shared` boot with the feature
/// compiled in can still A/B against the BKL-held path without a rebuild.
/// The block device (`BLOCK_DEVICE`) and network device (`NETWORK`) are already
/// BKL-free via `no-bkl-vfs` / `no-bkl-network`; this toggle covers the
/// remaining drivers only.
static DRIVERS_BKL_DROP_ENABLED: AtomicBool = AtomicBool::new(true);

/// Whether the device-driver-syscall BKL-drop (`no-bkl-drivers`) is currently enabled.
#[inline]
pub fn drivers_bkl_drop_enabled() -> bool {
    DRIVERS_BKL_DROP_ENABLED.load(Ordering::Relaxed)
}

/// Enable/disable the device-driver-syscall BKL-drop at runtime. Used by A/B
/// measurement; also serves as a runtime kill-switch, same as the other phases.
pub fn set_drivers_bkl_drop_enabled(on: bool) {
    DRIVERS_BKL_DROP_ENABLED.store(on, Ordering::Relaxed);
}

// --- The per-syscall BKL opt-out list (Phase 7f) ---------------------------------
//
// docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md §7.3: instead of removing the BKL from
// syscall entry in one step, `rust_sync_el0_handler` acquires it UNLESS the trapped
// syscall's number is on this list. A listed syscall's whole EL0→EL1 excursion runs as
// ONE open dropped-BKL window (opened without an acquire, closed without a re-acquire —
// see `akuma_exec::bkl::dropped_window_close_no_reacquire`), relying on the same inner
// locks its carve-out guard already proved sufficient; the guard itself stays in the
// syscall body and self-neutralizes to a nested (depth-2) window, so REMOVING a syscall
// from this list at runtime restores today's guard-scoped behaviour exactly.
//
// The compile-time seed starts EMPTY: an empty-list boot is behaviour-identical to one
// without the mechanism (every syscall takes the `enter_kernel`/`leave_kernel` path,
// verified by an A/B boot when this landed). Syscalls move onto the seed one at a time
// under the standing A/B + digest discipline, each individually revertible at runtime
// via `set_syscall_bkl_optout` (the per-syscall kill switch every prior phase leaned on).

/// Compile-time seed for the opt-out list: syscall numbers whose whole excursion is
/// BKL-free from boot. Keep this list SHORT-COMMENTED per entry with the phase that
/// validated it. Entries must already run their entire body under a whole-fn
/// carve-out guard (so conversion only deletes the entry/exit lock round-trip) —
/// audit the code outside the guard before adding one.
//
// Spelled with `nr::` constants rather than bare numbers. It used to be numbers
// with the name in a trailing comment, which is a comment that can drift from
// its value with nothing checking — the exact failure
// `docs/archive/AKUMA_EXTRACT_SYSCALLS.md` was written to end ("the table lived
// somewhere the other caller could not reach"). Since crate 1 landed there is
// not even a historical excuse: `akuma_syscalls_linux::nr` is a public crate and
// its consts are usable in this `const` context.
const SYSCALL_BKL_OPTOUT_SEED: &[u64] = &[
    // Phase 7f tranche 1: the `no-bkl-network` family — whole-fn `NetBklGuard`
    // since Phase 2, so the body already runs BKL-free; nothing outside the guard
    // but the dispatch-arm casts. sendto/recvfrom/sendmsg/recvmsg are deliberately
    // ABSENT: their unix-socket routing arm must stay BKL-held (AB-BA, locking.md).
    nr::SOCKET,
    nr::BIND,
    nr::LISTEN,
    nr::ACCEPT,
    nr::CONNECT,
    nr::GETSOCKNAME,
    nr::GETPEERNAME,
    nr::SETSOCKOPT,
    nr::GETSOCKOPT,
    nr::ACCEPT4,
    // getrandom (no-bkl-drivers family): DriverBklGuard covered everything but
    // validate_user_ptr, which now also runs BKL-free — same exposure it already has
    // inside every whole-fn net/vfs window (see the archive doc's follow-up note on
    // folding ensure_user_pages_mapped under as_lock).
    nr::GETRANDOM,
    nr::RESOLVE_HOST,
    // Phase 7f tranche 2a: syscalls whose whole body was audited to touch no
    // plain `Process` field and no process-table state beyond a bounded lookup.
    // `rt_sigprocmask`: the POSIX mask is PER-THREAD (`threading::
    // thread_signal_mask`/`set_thread_signal_mask`, plain atomics — the fix from
    // docs/SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md §D), so the body touches no
    // process-table state at all. Only `validate_user_ptr` + two user copies sit
    // outside it, the same exposure class `getrandom` already joined in tranche 1
    // (and now folded under `as_lock` by this tranche's pre-flight).
    nr::RT_SIGPROCMASK,
    // Phase 7f tranche 2b: the first conversion cleared by the blocking-window
    // analysis (archive doc §4). `nanosleep`'s window spans `schedule_blocking`,
    // but it carries NO `Process`-derived reference across the wait — the loop is
    // `uptime_us()` / `is_current_interrupted()` / `schedule_blocking()`, and
    // `is_current_interrupted` re-looks-up each iteration and clones an
    // `Arc<ProcessChannel>` whose lifetime is refcount-independent of the slot.
    // Every lookup-then-use is bounded far inside the 10ms reclaim cooldown. The
    // BKL was never held across the wait anyway (§4.1): the lock is per-core and
    // follows whichever thread the core resumes.
    nr::NANOSLEEP,
    // Phase 7f tranche 3: `futex`, the second blocking conversion, cleared once its
    // named prerequisite landed. The archive doc's §4.3 verdict was
    // "cleared-in-principle, BLOCKED on the second gate" — `FUTEX_WAITERS` was a bare
    // `Spinlock` with zero IRQ-masked sites, so a BKL-free window holding it plus a
    // nested IRQ hard-spinning for the BKL deadlocks AB-BA against a peer that holds
    // the BKL inside `futex_do_wake`. Every access now masks IRQs (`syscall/sync.rs`).
    //
    // Gate 1 (nothing `Process`-derived across the wait) holds: the loop carries only
    // `key = (tgid, uaddr)` and `deadline`, both plain values, and `futex_key_tgid`
    // returns a `u32` read out of the TTBR0-resident ProcessInfo page, not the table.
    //
    // The rest of the body is the audited surface: `validate_user_ptr` (folded under
    // `as_lock` by tranche 2's pre-flight), per-thread atomics (`peek_pending_signal`,
    // `current_thread_id`), lock-free timer reads (`uptime_us`; `utc_time_us` became an
    // atomic in this tranche for exactly this reason), and `get_waker_for_thread`, which
    // builds a `RawWaker` from a thread id and takes no lock.
    nr::FUTEX,
    // Phase 7f tranche 4, and the easiest entry this list will ever get:
    // `akuma_get_version` returns a compile-time constant. It reads no
    // arguments, touches no user memory, resolves no process and cannot block,
    // so there is no shared state for the BKL to be protecting — the audit that
    // every other entry needed is one line here.
    //
    // It is also the floor control for the syscall boundary
    // (docs/archive/AKUMA_SYSCALL_PERFORMANCE_AUDIT.md), which makes it the one
    // number where the opt-out's own cost is directly visible: the BKL
    // enter/leave pair is inside the 167 ns `wrap` span, and this is the only
    // arm with nothing else in it to hide that.
    nr::AKUMA_GET_VERSION,
];

/// Syscalls that must NEVER be opted out, mechanism-level (not merely "not yet
/// audited"): `exit` (93) and `exit_group` (94) never return through the opt-out exit
/// path and their teardown must run BKL-held (`return_to_kernel`'s ledger reset is a
/// safety net, not a design), and `rt_sigreturn` (139) is dispatched from the trap
/// prologue before the opt-out window's assumptions about the excursion shape hold.
const SYSCALL_BKL_OPTOUT_DENIED: &[u64] = &[nr::EXIT, nr::EXIT_GROUP, nr::RT_SIGRETURN];

/// Bitmap of syscall numbers (0..512 covers every Linux + Akuma-private number in
/// `syscall::nr`) currently opted out of the syscall-entry BKL.
static SYSCALL_BKL_OPTOUT: [AtomicU64; 8] = seeded_syscall_bkl_optout();

const fn seeded_syscall_bkl_optout() -> [AtomicU64; 8] {
    let mut words = [0u64; 8];
    let mut i = 0;
    while i < SYSCALL_BKL_OPTOUT_SEED.len() {
        let nr = SYSCALL_BKL_OPTOUT_SEED[i];
        assert!(nr < 512, "opt-out seed entry out of bitmap range");
        let mut j = 0;
        while j < SYSCALL_BKL_OPTOUT_DENIED.len() {
            assert!(
                nr != SYSCALL_BKL_OPTOUT_DENIED[j],
                "opt-out seed entry is on the structural deny list"
            );
            j += 1;
        }
        words[(nr / 64) as usize] |= 1u64 << (nr % 64);
        i += 1;
    }
    [
        AtomicU64::new(words[0]),
        AtomicU64::new(words[1]),
        AtomicU64::new(words[2]),
        AtomicU64::new(words[3]),
        AtomicU64::new(words[4]),
        AtomicU64::new(words[5]),
        AtomicU64::new(words[6]),
        AtomicU64::new(words[7]),
    ]
}

/// Whether syscall `nr` is currently opted out of the syscall-entry BKL. Read ONCE at
/// trap entry and latched for the excursion (locking.md's guard-latching rule) — the
/// exit path must use the entry's decision, never a re-read.
#[inline]
pub fn syscall_bkl_optout(nr: u64) -> bool {
    if nr >= 512 {
        return false;
    }
    SYSCALL_BKL_OPTOUT[(nr / 64) as usize].load(Ordering::Relaxed) & (1u64 << (nr % 64)) != 0
}

/// Add/remove one syscall from the opt-out list at runtime — the per-syscall kill
/// switch and the same-binary A/B handle. Returns `false` (and does nothing) for
/// numbers outside the bitmap or on the structural deny list. In-flight excursions are
/// unaffected either way: the entry handler latched its decision.
pub fn set_syscall_bkl_optout(nr: u64, on: bool) -> bool {
    if nr >= 512 || SYSCALL_BKL_OPTOUT_DENIED.contains(&nr) {
        return false;
    }
    let word = &SYSCALL_BKL_OPTOUT[(nr / 64) as usize];
    let bit = 1u64 << (nr % 64);
    if on {
        word.fetch_or(bit, Ordering::Relaxed);
    } else {
        word.fetch_and(!bit, Ordering::Relaxed);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every mutating test below owns a syscall number that is on neither
    // `SYSCALL_BKL_OPTOUT_SEED` nor `SYSCALL_BKL_OPTOUT_DENIED`, and that no other
    // test touches. The bitmap is a `static`, so `cargo test`'s thread pool would
    // otherwise let two tests race on one word. Verified free at extraction:
    // 63, 64, 127, 128, 301, 364, 447, 448, 511 (300 is seeded — it looks free and
    // is not).

    #[test]
    fn out_of_range_reads_false_and_refuses_the_write() {
        // The bitmap is 8 words = 512 bits. Anything at or past that must not index
        // it — this bound is the only thing between a bad syscall number and an
        // out-of-bounds load on the trap-entry hot path.
        for nr in [512u64, 513, 1024, u64::MAX] {
            assert!(!syscall_bkl_optout(nr), "nr={nr} must read false");
            assert!(!set_syscall_bkl_optout(nr, true), "nr={nr} must refuse the write");
            assert!(!syscall_bkl_optout(nr), "nr={nr} must still read false");
        }
    }

    #[test]
    fn structural_deny_list_cannot_be_opted_out() {
        // exit/exit_group never return through the opt-out exit path, and
        // rt_sigreturn is dispatched before the window's assumptions hold. A
        // successful write here is a hang or a corrupted ticket FIFO, not a
        // slowdown, which is why the setter refuses rather than warns.
        for nr in [nr::EXIT, nr::EXIT_GROUP, nr::RT_SIGRETURN] {
            assert!(!set_syscall_bkl_optout(nr, true), "nr={nr} must refuse");
            assert!(!syscall_bkl_optout(nr), "nr={nr} must stay opted in");
        }
    }

    #[test]
    fn round_trip_sets_then_clears() {
        const NR: u64 = 301;
        assert!(!syscall_bkl_optout(NR), "test number must start clear");
        assert!(set_syscall_bkl_optout(NR, true));
        assert!(syscall_bkl_optout(NR));
        assert!(set_syscall_bkl_optout(NR, false));
        assert!(!syscall_bkl_optout(NR));
    }

    #[test]
    fn word_boundary_bits_do_not_alias() {
        // 63 is word 0 bit 63; 64 is word 1 bit 0. An `nr / 64` / `1 << (nr % 64)`
        // that used `<<` on the wrong operand, or shifted by `nr` instead of
        // `nr % 64`, aliases these two — and 64 is `sys_pipe2`-adjacent territory,
        // so the failure would be a real syscall silently inheriting another's
        // BKL policy.
        assert!(set_syscall_bkl_optout(63, true));
        assert!(syscall_bkl_optout(63));
        assert!(!syscall_bkl_optout(64), "bit 63 must not leak into the next word");
        assert!(set_syscall_bkl_optout(63, false));
    }

    #[test]
    fn top_word_and_last_valid_bit() {
        // 447/448 straddle words 6 and 7; 511 is the highest number the bitmap can
        // hold, and the one an off-by-one in the `nr >= 512` bound would drop.
        assert!(set_syscall_bkl_optout(448, true));
        assert!(syscall_bkl_optout(448));
        assert!(!syscall_bkl_optout(447), "word 7 bit 0 must not leak into word 6");
        assert!(set_syscall_bkl_optout(511, true));
        assert!(syscall_bkl_optout(511));
        assert!(set_syscall_bkl_optout(448, false));
        assert!(set_syscall_bkl_optout(511, false));
    }

    #[test]
    fn seed_is_applied_at_const_time() {
        // `seeded_syscall_bkl_optout()` runs in a `const` context; if it silently
        // produced zeros the kernel would boot correct-but-slow, with no symptom
        // to notice. `nr::SOCKET` is Phase 7f tranche 1.
        assert!(syscall_bkl_optout(nr::SOCKET), "seeded entry must be live at boot");
        assert!(syscall_bkl_optout(nr::FUTEX), "seeded entry must be live at boot");
    }

    #[test]
    fn phase_toggles_default_on() {
        // Read-only on purpose: no test here mutates these, so this cannot race.
        // All five ship enabled; a default flipping to `false` unnoticed is a
        // silent whole-phase revert.
        assert!(fault_bkl_drop_enabled());
        assert!(exec_bkl_drop_enabled());
        assert!(vfs_bkl_drop_enabled());
        assert!(mm_bkl_drop_enabled());
        assert!(drivers_bkl_drop_enabled());
    }
}
