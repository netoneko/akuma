//! Real (shared-kernel) SMP — classic symmetric multiprocessing.
//!
//! This is the INVERSE of the multikernel in [`crate::smp`]. There, every core runs
//! a private copy of the kernel over a disjoint RAM partition, with `.data`/`.bss`
//! replicated into per-core pages; cores share nothing but a message ring. Here,
//! ALL cores execute ONE shared kernel image over ONE set of page tables, ONE PMM,
//! ONE heap, and (in later milestones) ONE global run queue, coordinated by real
//! cross-core locks. A `static` in this build is genuinely shared across cores — not
//! replicated — which is the whole point.
//!
//! Everything here is behind `cfg(kernel_smp_shared)` (the `smp-shared` feature);
//! the default and multikernel (`smp`) builds compile none of it, and build.rs makes
//! `smp` and `smp-shared` mutually exclusive.
//!
//! **Milestone M0 (this file):** bring N cores up on the shared boot page tables and
//! the single shared PMM/heap — no isolation, no partitions. Each secondary sets up
//! nothing beyond its own stack, reports itself online in a shared counter, prints
//! over the shared UART (identity-mapped as device via the boot L1[0] block), and
//! parks in `WFE`. The shared scheduler, per-core GIC/timer, and the Big Kernel Lock
//! arrive in later milestones. See docs/archive/SMP_SHARED.md for the running log and
//! docs/reference/subsystems/smp-shared.md for the current-state design.

// The M2c/M3/M4 demo workers + self-test accessors are exercised only by the boot
// self-tests in `process_tests` (compiled out under `no-tests` / `size`). In those
// runtime-only builds (e.g. devbox-smoltcp) they are intentionally unused — suppress the
// dead-code lint there. Test builds still lint dead code normally.
#![cfg_attr(not(kernel_tests), allow(dead_code))]

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
// The syscall number table (crate 1). The BKL opt-out seed below names its
// entries rather than spelling their numbers — see the note above the seed.
use akuma_syscalls_linux::nr;

/// Maximum cores we bring up. Comfortably under the `aff0 < 16` single-cluster limit
/// on QEMU `virt`.
const MAX_CORES: usize = 8;

/// Per-core boot/idle stack size shift (64 KiB). This stack backs the secondary's
/// idle thread, so it must hold the full IRQ excursion — the 832-byte trap frame plus
/// the shared `timer_irq_handler` (kernel-timer alarm processing, preemption watchdog,
/// logging) and the scheduler — which is far deeper than M0's WFE park. 16 KiB
/// overflowed here (fault-in-fault while holding the BKL); 64 KiB is ample.
const STACK_SHIFT: usize = 16;

// PSCI (matches crate::smp — QEMU `virt` exposes PSCI over the DTB-declared conduit).
const PSCI_CPU_ON: u64 = 0xC400_0003;
// SYSTEM_OFF/SYSTEM_RESET function IDs live in akuma_boot alongside the rest of
// the reboot ABI (`sc-reboot` only) — nothing hardware-specific about a constant.

// --- DTB-probed topology, stashed before the heap can overwrite the DTB ------------
static PROBED: AtomicBool = AtomicBool::new(false);
static NUM_CORES: AtomicUsize = AtomicUsize::new(1);
static USE_HVC: AtomicBool = AtomicBool::new(true);
static PROBED_MPIDRS: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];

/// Count of secondaries that have reached their online barrier. Genuinely shared
/// (not replicated) — a plain kernel static IS cross-core state in this build.
static ONLINE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Bitmask of cores currently halted in their idle loop (bit `aff0` set = idle). Used
/// by [`wake_remote_idle`] to nudge an idle core to pick up a just-woken thread
/// promptly (M4 cross-core wakeup) instead of waiting for its ~10 ms timer tick.
static CORE_IDLE_MASK: AtomicU32 = AtomicU32::new(0);

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

/// Runtime toggle (default **on**) for `no-bkl-process` (Phase 3 of
/// docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md) — drop the BKL for `fork_process`'s
/// CoW share/demote pass, relying on the address space's own `as_lock` (the same lock
/// the CoW fault handler takes BKL-free) held in bounded chunks around every
/// parent-page-table access, plus the existing `COW_REFCOUNTS` / PMM / allocator
/// spinlocks. `ProcessBklGuard` latches this at construction so an ON→OFF flip
/// mid-fork cannot unbalance the ticket FIFO.
///
/// Unlike the toggles above, the atomic itself lives in `akuma_exec::process::bkl_guard`
/// — the guard is constructed inside `fork_process`, which cannot name bin-crate items.
/// These are thin re-exports so every BKL toggle stays reachable from one module.
#[inline]
pub fn process_bkl_drop_enabled() -> bool {
    akuma_exec::process::process_bkl_drop_enabled()
}

/// Enable/disable the fork page-copy BKL-drop at runtime. Used by the boot self-test
/// `test_fork_bkl_drop` for its A/B phase; also the kill switch and the handle for an
/// interactive same-binary A/B under `bkl-profile`.
pub fn set_process_bkl_drop_enabled(on: bool) {
    akuma_exec::process::set_process_bkl_drop_enabled(on);
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

/// Runtime toggle (default **on**) for `no-bkl-irq` (Phase 7a of
/// docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md §7) — dispatch the timer IRQ (27) in
/// `rust_irq_handler_with_sp` without acquiring the BKL at all, relying on the alarm
/// queue's own `Spinlock` (`akuma_exec::alarms::ALARM_QUEUE`) and the lock-free preemption
/// watchdog / GIC MMIO the handler otherwise touches. Unlike `VfsBklGuard` and friends
/// this is not a dropped-BKL "window" latched per-call — there is no `enter_kernel` to
/// balance on this path in the first place — so the read happens once, directly in
/// `rust_irq_handler_with_sp`, with no latch-at-construction discipline needed.
///
/// Included in `smp-shared`'s default bundle since 2026-08-01, same as
/// `no-bkl-mm`/`no-bkl-drivers` above — a plain `smp-shared` build always reaches the
/// call site in `rust_irq_handler_with_sp` that reads this.
///
/// A/B'd 2026-08-01 on the SMP=4 contention regimen (source-toggled, byte-identical
/// feature set): `irq/sched` 24.7% (off, matches the pre-Phase-7a ~23.5% baseline) →
/// 10.2% (on), 6/6 digests exact both sides, 0 stuck/RECOVERED/PANIC/WILD/SPURIOUS —
/// see docs/archive/BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md §5.
static IRQ_BKL_DROP_ENABLED: AtomicBool = AtomicBool::new(true);

/// Whether the timer-IRQ BKL-drop (`no-bkl-irq`) is currently enabled.
#[inline]
pub fn irq_bkl_drop_enabled() -> bool {
    IRQ_BKL_DROP_ENABLED.load(Ordering::Relaxed)
}

/// Enable/disable the timer-IRQ BKL-drop at runtime. Used by A/B measurement; also
/// serves as a runtime kill-switch, same as the other phases.
pub fn set_irq_bkl_drop_enabled(on: bool) {
    IRQ_BKL_DROP_ENABLED.store(on, Ordering::Relaxed);
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
    // BKL was never held across the wait anyway (§4.1): `reconcile_for_spsr`
    // re-points the per-core lock at whichever thread the core resumes.
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

/// Runtime toggle (default **off**) for the M5c optimization: run the scheduler SGI
/// BKL-free when it preempted EL0 (userspace, no BKL held), so peer cores' timer ticks
/// don't serialize on the BKL. Correct at SMP=2. Left **off** because at SMP≥4 it opens a
/// **correctness deadlock** (see below) — not merely because it is premature.
///
/// ROOT CAUSE of the SMP≥4 hang (re-root-caused 2026-07-20 with lldb over the QEMU
/// gdbstub; full evidence in docs/runbooks/debug-smp.md §"M5c step-2"). It is a hard
/// **cross-core circular deadlock**, NOT the fairness/monopoly story the earlier note
/// claimed. Live state at the wedge:
///
/// - The BSP (core 0) holds the BKL and is spinning in a kernel thread's cooperative
///   wait-loop — `exec_with_io_cwd`'s `while !child.has_exited() { yield_now() }` — waiting
///   for an EL0 child it spawned. Because the loop runs entirely in EL1, the BKL is never
///   reconciled-to-EL0-released; the BSP holds it the whole time.
/// - The child is state `RUNNING`, "owned" by a secondary (its `TPIDRRO_EL0`), but that
///   secondary is frozen in `enter_kernel`'s BKL-acquire spin (`rust_irq_handler_with_sp`)
///   — it took a syscall/device IRQ while running the child and now waits on the BKL the
///   BSP holds.
/// - The BSP's `schedule_indices` only ever selects a **READY, non-idle** thread. The child
///   is `RUNNING` (skipped) and the only READY thread is a per-core idle (skipped), so the
///   scheduler returns `None` and the BSP spins its yield loop forever holding the BKL.
///
/// Why this toggle is what triggers it: the BKL-free EL0-preempt path lets a secondary
/// **claim a READY thread and mark it `RUNNING` without acquiring the BKL**, while the BSP
/// is holding the BKL in a cooperative wait for that very thread. Once the child is
/// `RUNNING` on a secondary, (a) the BSP won't touch it (it migrates only READY threads)
/// and (b) the secondary freezes the instant the child needs EL1. With the toggle OFF a
/// secondary can only claim a thread by first acquiring the BKL — which it can't while the
/// BSP holds it — so the child stays READY and the **BSP itself** runs it, reconciling to
/// EL0 and releasing the BKL. No deadlock.
///
/// The fix is TWO parts, both required — and both now DONE (validated at SMP=4 with the
/// 40-iteration exec-and-wait stress `test_smp_shared_cooperative_wait`, 3/3 clean where the
/// unfixed build hung ~100%):
///
/// 1. "A kernel thread must not hold the BKL across a cooperative wait-loop": drop +
///    re-acquire the BKL around `yield_now` waits. Done for `exec_with_io_cwd` via
///    `idle_halt` (`crate::process::exec::exec_with_io_cwd`). Alone this cut the hang from
///    ~100% to a ~25% residual.
/// 2. A FAIR / queued BKL. The residual ~25% was a livelock: after the waiter dropped the
///    BKL, the unfair test-and-set let peers (and the waiter re-grabbing on its next tick)
///    starve the one secondary holding the BKL-free-stolen child. Done: `KernelLock` is now
///    a FIFO ticket lock (`akuma_exec::sync`), which removes the residual.
///
/// The two-part fix above makes this toggle safe against the *cooperative-wait* deadlock it
/// was created for. It was ALSO briefly enabled by default (2026-07-20) and reverted the same
/// day for a separate bug: under heavy fork/exec churn at SMP≥4 the BKL-free EL0-preempt path
/// leaked a ticket in the fair `KernelLock` — it is the only path that acquires the BKL via
/// `reconcile_for_spsr` WITHOUT a paired `enter_kernel` (`exceptions.rs`, the
/// `sched_bklfree_el0_enabled()` branch), so a `next_ticket` advance had no matching
/// `now_serving` advance; once the counters drifted the lock hard-deadlocked with `owner==0`
/// (unowned) and every core spun in the ticket wait. lldb-confirmed 2026-07-20 (core spinning
/// at `rust_irq_handler_with_sp` on `now_serving != my_ticket`, `owner==0`).
///
/// **Fixed 2026-07-24** (commit "more smp fixes"): the reconcile path for this branch now uses
/// `reconcile_for_spsr_no_ticket` (`akuma_exec::bkl`), a variant that never takes a ticket in
/// the first place (`KernelLock::acquire_no_ticket`) — so there is nothing for `now_serving` to
/// fail to match. Default flipped back to ON the same day. The POOL-over-switch foundation
/// (step 1 of M5c) is always active regardless.
/// Only affects `cfg(kernel_smp_shared)` builds — the default release build is untouched.
static SCHED_BKLFREE_EL0_ENABLED: AtomicBool = AtomicBool::new(true);

/// Whether the BKL-free EL0-preempt scheduler path (M5c step 2) is enabled.
#[inline]
pub fn sched_bklfree_el0_enabled() -> bool {
    SCHED_BKLFREE_EL0_ENABLED.load(Ordering::Relaxed)
}

/// Enable/disable the BKL-free EL0-preempt scheduler path (M5c step 2). Default ON since
/// 2026-07-24 (the ticket-leak fix above). Kept toggleable for A/B debugging.
#[allow(dead_code)]
pub fn set_sched_bklfree_el0_enabled(on: bool) {
    SCHED_BKLFREE_EL0_ENABLED.store(on, Ordering::Relaxed);
}

/// Mark this core idle/busy in [`CORE_IDLE_MASK`] (called around the idle WFI).
#[inline]
fn set_core_idle(core: usize, idle: bool) {
    let bit = 1u32 << (core as u32);
    if idle {
        CORE_IDLE_MASK.fetch_or(bit, Ordering::Release);
    } else {
        CORE_IDLE_MASK.fetch_and(!bit, Ordering::Release);
    }
}

/// Ring one idle peer core's scheduler SGI so it wakes from WFI, reschedules, and can
/// pick up a just-woken READY thread now rather than on its next timer tick. Best-effort
/// (a race that finds no idle core just falls back to the timer). Reached via the
/// `wake_remote_idle` runtime hook — the scheduler's displacement bypass calls it to
/// route READY work to idle capacity instead of preempting a RUNNING thread.
/// Returns `true` iff an idle peer was found and rung.
pub fn wake_remote_idle() -> bool {
    let self_aff0 = read_mpidr() & 0xff;
    let mask = CORE_IDLE_MASK.load(Ordering::Acquire) & !(1u32 << self_aff0);
    if mask == 0 {
        return false; // no idle peer
    }
    let target = mask.trailing_zeros(); // lowest idle peer core
    crate::gic::trigger_sgi_core(target, SCHED_SGI);
    true
}

/// Send a scheduler SGI to a specific core so its scheduler picks up a just-woken
/// READY thread without waiting for the ~10 ms timer tick. Called from
/// `ThreadWaker::wake` via the `wake_core` runtime hook. Best-effort: a race that
/// finds the core already processing an SGI just falls back to the timer tick.
pub fn wake_core(core_id: u8) {
    let self_aff0 = (read_mpidr() & 0xff) as u8;
    if core_id == self_aff0 || core_id == 0xFF {
        return; // self-SGI already fired by trigger_sgi; or no last-known core
    }
    crate::gic::trigger_sgi_core(u32::from(core_id), SCHED_SGI);
}

unsafe extern "C" {
    /// The secondary trampoline (asm, `.text.boot`, defined below). Taking its
    /// address gives the identity-mapped PA to hand PSCI `CPU_ON` as the entry point.
    fn secondary_entry_shared();
}

#[inline]
fn read_mpidr() -> u64 {
    akuma_cpu::sysreg::mpidr_el1()
}

/// Issue a PSCI call over the platform conduit (`hvc`/`smc`). Returns x0 (0 = SUCCESS).
fn psci_call(use_hvc: bool, func: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    // SAFETY: a standard SMCCC call; we clobber the caller-saved GPR range (x1–x17).
    unsafe {
        if use_hvc {
            core::arch::asm!(
                "hvc #0",
                inout("x0") func => ret,
                in("x1") a1, in("x2") a2, in("x3") a3,
                lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
                lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
                lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
                lateout("x16") _, lateout("x17") _,
                options(nostack),
            );
        } else {
            core::arch::asm!(
                "smc #0",
                inout("x0") func => ret,
                in("x1") a1, in("x2") a2, in("x3") a3,
                lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
                lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
                lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
                lateout("x16") _, lateout("x17") _,
                options(nostack),
            );
        }
    }
    ret
}

/// Resolve the DTB pointer the way `detect_memory` does (QEMU does not set x0 for flat
/// kernels; the DTB sits 2 MiB-aligned above the image at 0x4020_0000).
fn resolve_dtb(dtb_ptr: usize) -> usize {
    const DTB_LOCATION: usize = 0x4020_0000;
    const FDT_MAGIC_LE: u32 = 0xedfe0dd0;
    if dtb_ptr != 0 {
        return dtb_ptr;
    }
    // SAFETY: speculative read of a u32 at a fixed RAM address; magic-checked.
    let magic = unsafe { core::ptr::read_volatile(DTB_LOCATION as *const u32) };
    if magic == FDT_MAGIC_LE { DTB_LOCATION } else { 0 }
}

/// `true` if the PSCI conduit is `hvc` (QEMU `virt`); default to it when absent.
fn psci_is_hvc(fdt: &fdt::Fdt) -> bool {
    fdt.find_node("/psci")
        .and_then(|n| n.property("method"))
        .is_none_or(|p| p.value.starts_with(b"hvc"))
}

/// Parse `/cpus` + `/psci` from the DTB and stash the topology. Called from
/// `kernel_main` before heap init (the heap can land on the DTB on large-RAM configs).
pub fn probe_dtb(dtb_ptr: usize) {
    let resolved = resolve_dtb(dtb_ptr);
    if resolved == 0 {
        crate::safe_print!(64, "[SMP-shared] no DTB; staying single-core\n");
        return;
    }
    // SAFETY: `resolved` points at a validated FDT blob (magic-checked above / by QEMU).
    let Ok(fdt) = (unsafe { fdt::Fdt::from_ptr(resolved as *const u8) }) else {
        crate::safe_print!(64, "[SMP-shared] DTB parse failed; single-core\n");
        return;
    };
    let mut count = 0usize;
    for cpu in fdt.cpus() {
        let mpidr = cpu.ids().first() as u64;
        let idx = (mpidr & 0xff) as usize;
        if idx < MAX_CORES {
            PROBED_MPIDRS[idx].store(mpidr, Ordering::Relaxed);
            count = count.max(idx + 1);
        }
    }
    NUM_CORES.store(count.max(1), Ordering::Relaxed);
    USE_HVC.store(psci_is_hvc(&fdt), Ordering::Relaxed);
    PROBED.store(true, Ordering::Release);
    crate::safe_print!(64, "[SMP-shared] probed {} core(s)\n", count.max(1));
}

/// M0 bringup: PSCI `CPU_ON` every secondary onto the shared boot tables, then wait
/// (bounded) for each to report online. Called from `kernel_main` after `gic::init`.
pub fn bringup_secondaries() {
    if !PROBED.load(Ordering::Acquire) {
        crate::safe_print!(56, "[SMP-shared] not probed; staying single-core\n");
        return;
    }
    let num_cores = NUM_CORES.load(Ordering::Relaxed);
    let use_hvc = USE_HVC.load(Ordering::Relaxed);
    let bsp_idx = (read_mpidr() & 0xff) as usize;
    crate::safe_print!(64, "[SMP-shared] {} core(s); BSP is core {}\n", num_cores, bsp_idx);

    if num_cores <= 1 {
        crate::safe_print!(56, "[SMP-shared] single core; no secondaries\n");
        return;
    }

    // Serialize console output from here on, BEFORE the first `CPU_ON`. Doing this
    // on the BSP rather than in `secondary_entry_shared` is what closes the window
    // entirely: a secondary that flipped the flag itself would already be racing
    // the BSP's own bringup prints, which is measurably enough to corrupt one line
    // ("CPU_ON core 1 (mpi[dSrM=P0x1) -->s ok"). Past this point at least two
    // cores can reach `emit()`, which is exactly the condition the lock is for.
    // See `console::set_multicore`.
    crate::console::set_multicore();

    let entry_pa = secondary_entry_shared as *const () as u64;
    // Publish everything the secondaries read (this module's statics, their stacks)
    // before any of them starts executing.
    dsb_sy();

    let mut expected = 0usize;
    for (idx, slot) in PROBED_MPIDRS.iter().enumerate().take(num_cores) {
        if idx == bsp_idx {
            continue;
        }
        let target = slot.load(Ordering::Relaxed);
        // `context_id` (a3) is unused in M0; pass the core index for future use / debug.
        let r = psci_call(use_hvc, PSCI_CPU_ON, target, entry_pa, idx as u64);
        if r == 0 {
            expected += 1;
            crate::safe_print!(80, "[SMP-shared] CPU_ON core {} (mpidr=0x{:x}) -> ok\n", idx, target);
        } else {
            crate::safe_print!(80, "[SMP-shared] CPU_ON core {} failed: {}\n", idx, r);
        }
    }

    // Bounded wait for the secondaries to reach the online barrier. This is a coarse
    // spin (no scheduler yet); ~50M iterations is generous on QEMU.
    let mut spins = 0u64;
    while ONLINE_COUNT.load(Ordering::Acquire) < expected && spins < 50_000_000 {
        core::hint::spin_loop();
        spins += 1;
    }
    let online = ONLINE_COUNT.load(Ordering::Acquire);
    if online == expected {
        crate::safe_print!(72, "[SMP-shared] \u{2713} {} secondary core(s) online (shared kernel)\n", online);
    } else {
        crate::safe_print!(80, "[SMP-shared] only {}/{} secondaries reported online\n", online, expected);
    }
}

/// Number of secondaries that reached the M0 online barrier (excludes the BSP).
/// Consumed by the boot self-test in `process_tests.rs` (compiled out under `no-tests`,
/// e.g. the devbox-smoltcp runtime image — hence `allow(dead_code)`).
#[allow(dead_code)]
pub fn online_secondary_count() -> usize {
    ONLINE_COUNT.load(Ordering::Acquire)
}

/// Whole-machine PSCI `SYSTEM_RESET`. Called from `src/syscall/reboot.rs`.
///
/// Unlike a self-hosted kexec (considered and rejected — see
/// `docs/runbooks/selfhost-kernel-build.md` background) this needs no in-kernel
/// SMP park/quiesce dance: QEMU/firmware tears every core and device back down
/// to the same clean reset state `boot.rs` already assumes, so a plain PSCI
/// reset gets that for free. `-kernel` bytes are cached by QEMU at process
/// startup and are not re-read on an in-process reset, so this only picks up a
/// freshly built kernel when combined with `-action reboot=shutdown` and a
/// host-side relaunch — see `scripts/cargo_runner.sh`'s `KERNEL_DROPOFF`.
#[cfg(feature = "sc-reboot")]
pub fn system_reset() -> ! {
    let use_hvc = USE_HVC.load(Ordering::Relaxed);
    psci_call(use_hvc, akuma_boot::PSCI_SYSTEM_RESET, 0, 0, 0);
    // SYSTEM_RESET does not return on success; reaching here means the call
    // itself failed (e.g. no PSCI conduit). Nothing sensible to do but spin —
    // the syscall dispatcher isn't set up to receive a return from this path.
    loop {
        core::hint::spin_loop();
    }
}

/// Whole-machine PSCI `SYSTEM_OFF`. See `system_reset` for the shared reasoning.
#[cfg(feature = "sc-reboot")]
pub fn system_off() -> ! {
    let use_hvc = USE_HVC.load(Ordering::Relaxed);
    psci_call(use_hvc, akuma_boot::PSCI_SYSTEM_OFF, 0, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

/// Number of cores the DTB reported (including the BSP). See `online_secondary_count`.
#[allow(dead_code)]
pub fn probed_core_count() -> usize {
    NUM_CORES.load(Ordering::Relaxed)
}

#[inline]
fn dsb_sy() {
    akuma_cpu::barrier::dsb_sy();
}

// --- Per-PE GICv3 receive path (M2c) --------------------------------------------
// The boot L1[0] block identity-maps device space, so a secondary on the shared boot
// tables reaches its own redistributor at its physical address directly. Constants
// mirror `crate::smp` (kept private here so the multikernel path stays untouched).
/// Base of the GICv3 redistributor region, as a **physical** address.
///
/// Secondaries reach the redistributor through the low identity mapping
/// (`boot.rs` L1[0] maps 0..1 GiB as a device block, and the GIC is below 1 GiB
/// on both supported machines) rather than through the L0[1] device window,
/// because this runs during bringup on the boot page table.
///
/// The value is machine-specific, and on Firecracker it also depends on the
/// configured vCPU count — see `crate::platform`. It is read from the installed
/// device map so that a redistributor discovered from the FDT wins over the
/// compile-time bootstrap literal; getting this wrong points a core at another
/// core's frames, which silently costs it its timer interrupt.
fn gicr_base() -> usize {
    crate::platform::gicr_base_pa()
}
const GICR_STRIDE: usize = crate::platform::GICR_STRIDE;
const GICR_SGI_OFFSET: usize = crate::platform::GICR_SGI_OFFSET;
const GICR_WAKER: usize = 0x0014;
const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
const GICR_SGI_IGROUPR0: usize = 0x0080;
const GICR_SGI_ISENABLER0: usize = 0x0100;
const GICR_SGI_IPRIORITYR: usize = 0x0400;
/// EL1 virtual-timer PPI (the shared 10 ms scheduler tick) and the scheduler SGI
/// (INTID 0), which each core rings at itself from the shared timer handler.
const TIMER_PPI: u32 = 27;
const SCHED_SGI: u32 = 0;

/// ISV-safe single-register MMIO (no writeback/pair form — those assert under QEMU HVF;
/// same reasoning as `gic_v3::mmio_w32`).
fn mmio_w32(addr: usize, val: u32) {
    // SAFETY: `addr` is a device-mapped GIC redistributor register.
    unsafe {
        core::arch::asm!("str {v:w}, [{a}]", v = in(reg) val, a = in(reg) addr,
            options(nostack, preserves_flags));
    }
}
fn mmio_r32(addr: usize) -> u32 {
    let val: u32;
    // SAFETY: `addr` is a device-mapped GIC redistributor register.
    unsafe {
        core::arch::asm!("ldr {v:w}, [{a}]", v = out(reg) val, a = in(reg) addr,
            options(nostack, preserves_flags, readonly));
    }
    val
}

/// Bring up THIS secondary's GICv3 receive path: enable the system-register CPU
/// interface, wake its redistributor, and enable the scheduler SGI (INTID 0) + the
/// virtual-timer PPI (27). The distributor's global config was done once by the BSP.
fn secondary_gic_init(idx: usize) {
    // SAFETY: GICv3 CPU-interface system registers; values per the architecture.
    unsafe {
        let sre: u64;
        core::arch::asm!("mrs {0}, S3_0_C12_C12_5", out(reg) sre, options(nomem, nostack));
        core::arch::asm!("msr S3_0_C12_C12_5, {0}", in(reg) sre | 1, options(nomem, nostack)); // ICC_SRE_EL1.SRE
        akuma_cpu::barrier::isb();
        core::arch::asm!("msr S3_0_C4_C6_0, {0}", in(reg) 0xFFu64, options(nomem, nostack)); // ICC_PMR_EL1
        core::arch::asm!("msr S3_0_C12_C12_3, {0}", in(reg) 0u64, options(nomem, nostack)); // ICC_BPR1_EL1
        core::arch::asm!("msr S3_0_C12_C12_7, {0}", in(reg) 1u64, options(nomem, nostack)); // ICC_IGRPEN1_EL1
    }
    akuma_cpu::barrier::isb();
    let rd = gicr_base() + idx * GICR_STRIDE;
    let sgi = rd + GICR_SGI_OFFSET;
    let waker = rd + GICR_WAKER;
    mmio_w32(waker, mmio_r32(waker) & !GICR_WAKER_PROCESSOR_SLEEP);
    while mmio_r32(waker) & GICR_WAKER_CHILDREN_ASLEEP != 0 {
        core::hint::spin_loop();
    }
    mmio_w32(sgi + GICR_SGI_IGROUPR0, 0xFFFF_FFFF);
    for i in 0..8 {
        mmio_w32(sgi + GICR_SGI_IPRIORITYR + i * 4, 0xA0A0_A0A0);
    }
    mmio_w32(sgi + GICR_SGI_ISENABLER0, (1u32 << SCHED_SGI) | (1u32 << TIMER_PPI));
    // Ensure redistributor writes complete before IRQs are unmasked.
    akuma_cpu::barrier::dsb_ish();
}

/// Base VA of this core's 64 KiB (`1 << STACK_SHIFT`) boot/idle stack in
/// `secondary_boot_stacks_shared`.
pub fn secondary_stack_base(core: usize) -> usize {
    let addr: usize;
    // SAFETY: resolves the `.bss.smp_shared` symbol's address; no memory access.
    unsafe {
        core::arch::asm!(
            "adrp {t}, secondary_boot_stacks_shared",
            "add {t}, {t}, :lo12:secondary_boot_stacks_shared",
            t = out(reg) addr,
            options(nomem, nostack),
        );
    }
    addr + (core << STACK_SHIFT)
}

/// Set `VBAR_EL1` to the shared exception vector table (the BSP's) so this core takes
/// syscalls/IRQs/faults through the same handlers.
fn set_shared_vbar() {
    // SAFETY: installs the kernel's exception vector base for this PE.
    unsafe {
        core::arch::asm!(
            "adrp {t}, exception_vector_table",
            "add {t}, {t}, :lo12:exception_vector_table",
            "msr vbar_el1, {t}",
            "isb",
            t = out(reg) _,
            options(nomem, nostack),
        );
    }
}

/// Per-core counter of scheduler ticks each core has serviced a worker on — the M2c
/// proof that threads execute across cores. Genuinely shared (not replicated).
static CORES_SEEN: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];

/// Per-core count of EL0 traps (syscalls / user faults) serviced — the M3 proof that
/// USERSPACE runs across cores. Bumped from the EL0 trap entry (`rust_sync_el0_handler`).
static CORES_SEEN_USER: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];

/// Record that this core just took an EL0 trap (a user syscall or fault). Called from
/// the syscall entry; an EL0 trap only originates from userspace, so a nonzero count on
/// core N means a user process executed on core N.
#[inline]
pub fn record_el0_trap() {
    let aff0 = (read_mpidr() & 0xff) as usize;
    if aff0 < MAX_CORES {
        CORES_SEEN_USER[aff0].fetch_add(1, Ordering::Relaxed);
    }
}

/// Number of cores that have serviced a user (EL0) trap (M3 self-test).
pub fn cores_that_ran_userspace() -> usize {
    CORES_SEEN_USER.iter().filter(|c| c.load(Ordering::Relaxed) > 0).count()
}

/// Per-core user-trap count (diagnostics / self-test).
pub fn user_traps(core: usize) -> u64 {
    if core < MAX_CORES {
        CORES_SEEN_USER[core].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// Bitmask of cores a single migration-test thread has observed itself running on
/// (bit `aff0`). `count_ones() >= 2` proves that ONE thread migrated across cores (M4).
static MIGRATION_MASK: AtomicU32 = AtomicU32::new(0);

/// Stop flag for the demo workers/probes so they self-terminate after their test and
/// free their (scarce) system-thread slots for the next test. See [`stop_and_reclaim_demos`].
static DEMO_STOP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Terminate the current kernel thread and park (the standard kernel-thread exit path:
/// mark terminated so `cleanup_terminated` recycles the slot, then yield forever).
fn demo_exit() -> ! {
    akuma_exec::threading::mark_current_terminated();
    loop {
        akuma_exec::threading::yield_now();
    }
}

/// Signal all demo workers/probes to stop, wait for them to self-terminate, reclaim
/// their slots, then reset the flag for the next test. Keeps the scarce system-thread
/// slots (RESERVED_THREADS) from leaking across the M2c/M3/M4 self-tests.
pub fn stop_and_reclaim_demos() {
    DEMO_STOP.store(true, Ordering::Release);
    // Let workers wake from their short sleep, observe the flag, and self-terminate.
    let start = crate::timer::uptime_us();
    while crate::timer::uptime_us().saturating_sub(start) < 300_000 {
        akuma_exec::threading::yield_now();
        akuma_exec::threading::idle_halt();
    }
    akuma_exec::threading::cleanup_terminated();
    DEMO_STOP.store(false, Ordering::Release);
}

/// M4 migration-proof worker: record this core in `MIGRATION_MASK`, then SLEEP so the
/// scheduler is free to resume us on a *different* core next time. Stops on `DEMO_STOP`.
fn migration_worker() -> ! {
    while !DEMO_STOP.load(Ordering::Acquire) {
        let aff0 = read_mpidr() & 0xff;
        MIGRATION_MASK.fetch_or(1u32 << aff0, Ordering::Relaxed);
        akuma_exec::threading::sleep_us(1500);
    }
    demo_exit()
}

/// Spawn ONE migration-test thread (M4). One thread that lands on >1 core over its
/// lifetime demonstrates cross-core migration (not just different threads on different
/// cores). Call once, from the BSP.
pub fn spawn_migration_probe() {
    if NUM_CORES.load(Ordering::Relaxed) <= 1 {
        return;
    }
    let _ = akuma_exec::threading::spawn_system_thread_fn(migration_worker);
}

/// Number of distinct cores the single migration-probe thread has run on (M4 self-test).
pub fn migration_core_count() -> u32 {
    MIGRATION_MASK.load(Ordering::Relaxed).count_ones()
}

/// Demo worker (real shared-kernel SMP M2c): bump this core's counter, then SLEEP.
/// Sleeping (rather than busy-looping) is essential — a kernel thread holds the Big
/// Kernel Lock while it runs, so a never-sleeping worker would let one core monopolize
/// the lock and starve the others. Sleeping makes the worker WAITING, letting a core
/// drop to idle (releasing the BKL) so peers can pick up work. Runs forever.
fn smp_worker() -> ! {
    while !DEMO_STOP.load(Ordering::Acquire) {
        let aff0 = (read_mpidr() & 0xff) as usize;
        if aff0 < MAX_CORES {
            CORES_SEEN[aff0].fetch_add(1, Ordering::Relaxed);
        }
        akuma_exec::threading::sleep_us(2000);
    }
    demo_exit()
}

/// Spawn the M2c demo workers from the BSP (after secondaries are up). Each is an
/// ordinary shared-pool kernel thread; the per-core schedulers distribute them, so over
/// time both the BSP and the secondaries run them. Call once.
pub fn spawn_worker_demo() {
    let cores = NUM_CORES.load(Ordering::Relaxed);
    if cores <= 1 {
        return;
    }
    // A few workers (cores + 1) so there is usually one runnable when a core wakes.
    // Spawning is best-effort (slot-limited), so report what was actually created,
    // not what was attempted: `cores + 1` was printed unconditionally, which turned
    // a partial spawn into a confident false count in the boot log — and
    // `cores_that_ran_workers()` below feeds a boot self-test that is read against
    // exactly this line.
    let mut spawned = 0u32;
    for _ in 0..=cores {
        if akuma_exec::threading::spawn_system_thread_fn(smp_worker).is_ok() {
            spawned += 1;
        }
    }
    crate::safe_print!(64, "[SMP-shared] spawned {} demo workers\n", spawned);
}

/// Self-test waiter that parks in a pure `blocking_relax()` loop — exactly what a thread
/// blocked in a socket recv (`wait_until`) or a DNS resolve does. Each time it is scheduled
/// it holds the Big Kernel Lock and must DROP it across the relax so peer cores can enter
/// the kernel; if `blocking_relax` regressed to not drop the BKL, a waiter that lands on a
/// peer core would hold it forever and freeze every other core (the meow->LLM wedge). Stops
/// on `DEMO_STOP`.
fn blocking_relax_waiter() -> ! {
    while !DEMO_STOP.load(Ordering::Acquire) {
        akuma_exec::threading::blocking_relax();
    }
    demo_exit()
}

/// Spawn one [`blocking_relax_waiter`] per core (BSP self-test). They occupy every core so
/// the test can prove the BSP still makes BKL-requiring forward progress while they are all
/// parked in a blocking wait. Best-effort (slot-limited); stop via [`stop_and_reclaim_demos`].
pub fn spawn_blocking_relax_waiters() {
    let cores = NUM_CORES.load(Ordering::Relaxed);
    if cores <= 1 {
        return;
    }
    for _ in 0..cores {
        let _ = akuma_exec::threading::spawn_system_thread_fn(blocking_relax_waiter);
    }
}

/// Number of cores that have run a demo worker at least once (for the boot self-test).
pub fn cores_that_ran_workers() -> usize {
    CORES_SEEN.iter().filter(|c| c.load(Ordering::Relaxed) > 0).count()
}

/// Per-core worker tick count (diagnostics / self-test).
pub fn worker_ticks(core: usize) -> u64 {
    if core < MAX_CORES {
        CORES_SEEN[core].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// Secondary-core Rust entry (M2c). Runs on the SHARED boot page tables + PMM/heap.
/// Adopts its boot context as this core's idle thread, brings up its GIC receive path,
/// installs the shared vectors, arms the shared 10 ms virtual-timer tick, enables IRQs,
/// and enters the idle loop — from which the timer preempts it onto any runnable thread
/// in the shared scheduler. The BKL (held across each IRQ/scheduler excursion)
/// serializes kernel execution; `idle_halt` drops it around WFI so peers can enter.
///
/// # Safety
/// Called only from the `secondary_entry_shared` trampoline, once per core, MMU on,
/// with this core's boot stack installed and IRQs masked.
#[unsafe(no_mangle)]
pub extern "C" fn secondary_shared_start(_context_id: u64, core_idx: u64) -> ! {
    let core = core_idx as usize;
    let stack_base = secondary_stack_base(core);
    let stack_size = 1usize << STACK_SHIFT;
    let exc_top = (stack_base + stack_size) as u64;

    // Adopt the current (boot) context as this core's idle thread so the shared
    // scheduler can switch away from it and back. Sets TPIDRRO_EL0.
    let idle =
        akuma_exec::threading::adopt_current_as_core_idle(core, exc_top, stack_base, stack_size);

    ONLINE_COUNT.fetch_add(1, Ordering::AcqRel);
    let Some(slot) = idle else {
        crate::safe_print!(64, "[SMP-shared] core {} online but NO idle slot; parking\n", core);
        loop {
            akuma_cpu::park::wfe();
        }
    };
    crate::safe_print!(64, "[SMP-shared] core {} online (idle tid {})\n", core, slot);

    // Bring up this PE's interrupt receive path, install shared vectors, arm the tick.
    secondary_gic_init(core);
    set_shared_vbar();
    // Arm this core's periodic tick from the shared choice (BSP's probe /
    // override result, published via akuma-timer's registry).
    crate::timer::enable_timer_interrupts(crate::timer::current_tick_us());

    // Unmask IRQs: from here the timer tick drives this core's scheduler.
    // The `isb` matters here specifically: this is the first time this core takes
    // interrupts, so a tick already pending must be taken before the idle loop is
    // entered rather than at some later synchronization point.
    akuma_primitives::irq::unmask_irqs_sync();

    // Idle loop. Two requirements pull against each other: (1) don't hammer the BKL
    // when there's nothing to run (a `yield_now()` every iteration livelocks SMP=4 as
    // idle cores fight over the lock); (2) still switch onto a thread the moment one is
    // runnable. The solution: release the BKL, WFI, re-acquire — WITHOUT disabling
    // preemption (unlike `idle_halt`). With preemption enabled, the timer tick's
    // self-SGI runs the scheduler and preempts this idle thread onto any READY thread;
    // when nothing is runnable we just fall back to WFI. One timer IRQ + one SGI per
    // ~10 ms tick — cheap and contention-free.
    loop {
        // Publish that this core is idle so `wake_remote_idle` can nudge it, then drop
        // the BKL so peers can enter the kernel while we're halted.
        set_core_idle(core, true);
        akuma_exec::bkl::leave_kernel();
        // SAFETY: IRQs are enabled; the timer/device IRQ (or a cross-core wake SGI)
        // wakes us and (via the scheduler SGI) may switch us to a runnable thread.
        akuma_cpu::park::wfi();
        // Re-take the BKL for our brief kernel work before the next halt (idempotent
        // if the waking IRQ's reconcile already re-acquired it for us).
        akuma_exec::bkl::enter_kernel();
        // Profiler only: this bootstrap idle loop has no syscall/fault entry point, so
        // name its hold rather than leaving it "unknown" (BKL_VFS_CARVE_OUT.md §18).
        akuma_exec::sync::set_holder_tag(
            akuma_exec::bkl::current_core_id(),
            akuma_exec::sync::HOLD_TAG_IDLE,
        );
        set_core_idle(core, false);
        // Per-core half of `process::reclaim`'s idle drain site (the BSP's lives in
        // thread 0's idle loop): a secondary that has nothing to run is a collector
        // this box would otherwise never use. We hold the BKL here, which sits ABOVE
        // every drop-path lock in the order (`BKL > as_lock > {PMM, ...}`) — the same
        // context netpoll_maint's reclaim already runs in.
        akuma_exec::process::reclaim::drain_retired_if_requested();
    }
}

// Secondary trampoline. Mirrors boot.rs's MMU setup, but loads the SHARED boot
// TTBR0/TTBR1 (never a restricted per-core table) and tail-calls
// `secondary_shared_start`. Lives in `.text.boot` so it is identity-reachable with
// the MMU still off at entry. `boot_ttbr0_addr`/`boot_ttbr1_addr` are the BSP's
// boot-table roots, published MMU-off into `.data.boot` by boot.rs.
core::arch::global_asm!(
    r#"
.section .text.boot
.global secondary_entry_shared
secondary_entry_shared:
    mov     x19, x0                 // x19 = context_id (a3 from PSCI CPU_ON; unused M0)

    // 0. Zero TPIDRRO_EL0 before anything can call `current_tid()`.
    //
    // Same reason as the BSP path in `boot.rs`: the register's reset value is
    // architecturally UNKNOWN, and KVM stamps UNKNOWN-reset registers with
    // 0x1de7ec7edbadc0de, which `current_tid()` treats as fatal. Each PSCI-woken
    // secondary gets its own freshly-reset register, so zeroing it on the BSP is
    // not enough.
    msr     tpidrro_el0, xzr

    // 1. Enable FPU/SIMD (FPEN = 0b11).
    mov     x0, #(3 << 20)
    msr     cpacr_el1, x0
    isb

    // 2. core idx = MPIDR aff0; bail to park if it exceeds MAX_CORES.
    mrs     x20, mpidr_el1
    and     x20, x20, #0xff
    cmp     x20, #{max_cores}
    b.ge    .Lsh_park
    // SP = &secondary_boot_stacks_shared[idx] + STACK_SIZE (top of this core's stack)
    adrp    x0, secondary_boot_stacks_shared
    add     x0, x0, :lo12:secondary_boot_stacks_shared
    add     x0, x0, x20, lsl #{stack_shift}
    mov     x1, #1
    add     x0, x0, x1, lsl #{stack_shift}
    mov     sp, x0

    // 3. MMU registers (mirror boot.rs configure_mmu_regs).
    // CNTKCTL_EL1.EL0VCTEN — EL0 reads CNTVCT/CNTFRQ directly (see boot.rs).
    mov     x0, #0x2
    msr     cntkctl_el1, x0
    // MAIR_EL1 = 0xFFBB4400
    mov     x0, #0x4400
    movk    x0, #0xFFBB, lsl #16
    msr     mair_el1, x0
    // TCR_EL1 = 0x0000_0005_B510_3510
    mov     x0, #0x3510
    movk    x0, #0xB510, lsl #16
    movk    x0, #0x5, lsl #32
    msr     tcr_el1, x0
    // TTBR0_EL1 <- *boot_ttbr0_addr (the BSP's SHARED boot table; read MMU-off)
    adrp    x0, boot_ttbr0_addr
    add     x0, x0, :lo12:boot_ttbr0_addr
    ldr     x0, [x0]
    msr     ttbr0_el1, x0
    // TTBR1_EL1 <- *boot_ttbr1_addr
    adrp    x0, boot_ttbr1_addr
    add     x0, x0, :lo12:boot_ttbr1_addr
    ldr     x0, [x0]
    msr     ttbr1_el1, x0
    tlbi    vmalle1
    dsb     sy
    isb
    // Enable MMU + caches (same SCTLR bits as boot.rs _boot_code).
    //
    // Including the two `bic`s: each PSCI-woken core gets its own SCTLR_EL1 with
    // its own reset value, and KVM's has SA/SA0 set while QEMU's does not. Clearing
    // them on the BSP alone would leave every secondary enforcing EL0 SP-alignment
    // checks that this kernel's userspace has never satisfied. See boot.rs.
    mrs     x0, sctlr_el1
    orr     x0, x0, #1              // M  = MMU enable
    orr     x0, x0, #(1 << 2)      // C  = data cache
    orr     x0, x0, #(1 << 12)     // I  = instruction cache
    orr     x0, x0, #(1 << 14)     // DZE
    orr     x0, x0, #(1 << 15)     // UCT
    orr     x0, x0, #(1 << 26)     // UCI
    bic     x0, x0, #(1 << 3)      // SA  = 0: no SP alignment check at EL1
    bic     x0, x0, #(1 << 4)      // SA0 = 0: no SP alignment check at EL0
    msr     sctlr_el1, x0
    isb

    // 4. secondary_shared_start(context_id, core_idx)
    mov     x0, x19
    mov     x1, x20
    bl      secondary_shared_start

.Lsh_park:
    wfe
    b       .Lsh_park

.section .bss.smp_shared
.balign 16
// `.global` because `secondary_stack_base` is pub and gets inlined into callers in
// other codegen units (the boot self-test). Without it the symbol is local to this
// global_asm! block and any out-of-CGU `adrp` reference fails to link.
.global secondary_boot_stacks_shared
secondary_boot_stacks_shared:
    .space  {stacks_bytes}
"#,
    max_cores = const MAX_CORES,
    stack_shift = const STACK_SHIFT,
    stacks_bytes = const (MAX_CORES << STACK_SHIFT),
);
