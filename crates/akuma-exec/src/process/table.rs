use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use spinning_top::Spinlock;

use akuma_slot_table::{SlotMiss, SlotTable};

use crate::process::Process;
use crate::process::types::Pid;
use crate::runtime::{config, runtime, with_irqs_disabled};

/// Maximum number of concurrent processes.
pub const MAX_PROCESSES: usize = 256;

/// Next available PID (monotonically increasing, never recycled)
pub static NEXT_PID: AtomicU32 = AtomicU32::new(1);

/// The lock-free slot store: per-slot FREE/ACTIVE/RETIRED state, the `*mut
/// Process` pointer, a reuse generation, and a retire timestamp — and every
/// dereference of those pointers. See `akuma-slot-table`'s crate docs for the
/// one stated contract (deferred reclamation + cooldown) that all the
/// borrow-returning accessors below rest on.
///
/// The slot lifecycle is ACTIVE → RETIRED → (`reclaim_retired` frees the
/// `Process`) → FREE → claimed again → ACTIVE. The reuse generation is what
/// makes `is_active(i)` mean "still the occupant you cached" rather than merely
/// "somebody is here": a recycled slot reads ACTIVE while a cached pointer
/// dangles (`docs/archive/IDENTITY_CACHE_SMP_REVIEW.md` Finding B), and pointer
/// equality is no substitute because `Process` is a fixed-size allocation the
/// allocator can hand straight back. `reclaim_retired` bumps the generation
/// between the pointer swap and the `FREE` store, while the slot is RETIRED, so
/// no reader can observe ACTIVE paired with a stale stamp.
static PROCESS_TABLE: SlotTable<Process, MAX_PROCESSES> = SlotTable::new();

/// Register a process in the table (takes ownership via Box).
///
/// Finds a free slot via CAS, stores the Process pointer.
/// The Process is kept alive until `unregister_process` + `reclaim_retired_processes`
/// reclaim it.
pub fn register_process(_pid: Pid, proc: Box<Process>) {
    let proc = match PROCESS_TABLE.try_claim(proc) {
        Ok(_slot) => return,
        Err(proc) => proc,
    };
    // Miss: deliberately NOT calling `reclaim_retired_processes` here, even though
    // the table may simply be full of cooled-down zombies awaiting periodic
    // reclamation rather than genuinely out of slots (the same shape
    // `spawn_user_thread_fn_internal`'s on-demand `reclaim_terminated_slots` retry
    // solves for THREAD_STATES). `register_process` is called from deep inside
    // fork/clone/spawn paths whose lock context it doesn't control; reclaiming runs
    // arbitrary `Process::drop` code (frees page-table frames, releases an ASID —
    // `UserAddressSpace::drop`), which needs its own locks. A caller already holding
    // one of those locks (or anything downstream of them) self-deadlocks the instant
    // reclaim tries to take it again on the same core — this is exactly the boot
    // hang found and root-caused while landing this phase (real fork/exec/pipe
    // traffic through `test_sigpipe_terminate_no_deadlock`; see
    // docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md §"the on-demand reclaim that
    // wasn't"). The fix for a starved collector had to be a *different* call site, and
    // that is what `process::reclaim` now provides: five drain sites (the exit-path
    // terminal parks, the idle loops, the allocator's pressure ladder) plus the
    // original 100 ms `netpoll_maint` one. Requesting a drain is lock-free, so it IS
    // safe from here — collecting is not.
    crate::process::reclaim::request_retired_reclaim();
    drop(proc);
    // OPEN (docs/reference/subsystems/memory.md → "OPEN: a full process table still
    // panics the kernel"): this should return an error so fork/clone/spawn surface
    // -EAGAIN — "collector starved, retry" — instead of halting the box, since every
    // slot here may be a reclaimable zombie. Blocked only on making this function
    // fallible, which means threading a `Result` through every spawn caller. The
    // request above at least guarantees the next drain site collects; it cannot help
    // *this* caller, which is already dead by the line below.
    panic!(
        "Process table full ({} slots, {} reclaimable RETIRED)",
        MAX_PROCESSES,
        retired_process_count()
    );
}

/// Retire a process from the table: makes it invisible to `get_process_ptr`,
/// `for_each_process`, `current_process`, and every other lookup, and ineligible
/// for `register_process`'s free-slot scan — but does **not** free the `Process`
/// or drop its address space yet. Returns `true` if `pid` was found and this call
/// is the one that retired it (a racing second `unregister_process(pid)` for the
/// same pid loses the CAS below and returns `false`).
///
/// # Why deferred
/// A BKL-dropped window (`no-bkl-vfs`/`no-bkl-mm`/`no-bkl-process`) lets a peer
/// core hold a raw `*mut`/`*const Process` (from `lookup_process`/`get_process_ptr`/
/// `lookup_process_shared`) across real work with the BKL released. If this pid's
/// `Process` were freed here — as it used to be, synchronously, via
/// `Box::from_raw` + drop — that peer core's pointer becomes a use-after-free the
/// instant this call returns, with nothing but `with_irqs_disabled` (mutual
/// exclusion on one core) standing between them. Retiring instead of freeing keeps
/// the memory valid until `reclaim_retired_processes` frees it well after any such
/// window could still be open. See docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md
/// (Phase 7e, "Free" half) and docs/archive/BKL_PHASE7_AUDIT.md §2.1/§2.1.1.
pub fn unregister_process(pid: Pid) -> bool {
    // `SlotTable::retire` does the ACTIVE→RETIRED CAS (losing it — a racing
    // retire of this exact slot — skips to the next match) and stamps the
    // retire time *after* the CAS wins. The closure runs with the slot RETIRED
    // and the `Process` provably still live.
    PROCESS_TABLE.retire(
        || (runtime().uptime_us)(),
        |p| p.pid == pid,
        |i, p| {
            // Stamp how much memory this slot is parking and flag that a collector is
            // wanted. Scalar, taken here while the `Process` is provably alive — a reader
            // on the pressure path must never dereference a RETIRED `Process` to size it,
            // since that races the very drop it is trying to schedule. See
            // `process::reclaim`.
            crate::process::reclaim::note_retired(i, p.address_space.resident_pages() as u32);
            // Mark the process's thread as TERMINATED before unregistering.
            // This prevents orphaned threads that stay READY forever after
            // their process is reaped. Without this, kthreads shows "user-process"
            // threads with no corresponding process, and switching to them hangs.
            //
            // IMPORTANT: Only mark terminated if this is NOT the current thread.
            // Tests call unregister_process from the same thread to clean up,
            // and marking ourselves terminated would cause Thread 0's cleanup
            // to zero our context while we're still running.
            //
            // IMPORTANT: `thread_id` is only a *recorded* slot number, and thread slots
            // are recycled (`cleanup_terminated_internal`, ~10 ms cooldown). A process
            // whose thread already self-terminated can have that slot handed to a brand
            // new, unrelated process before this runs — `kill_thread_group` PHASE 2 is
            // the path that gets there late, because PHASE 1 only *requests* deferred
            // kills and then grace-waits up to 2 s for siblings to reach their EL1→EL0
            // boundary. Terminating a recycled slot kills an innocent thread and leaves
            // ITS process alive with no thread at all: unschedulable, unable to exit,
            // never reaped, and its parent's `wait4` blocked forever. That is the silent
            // `rustc big.rs` hang (a linker `gcc` lost its only thread mid-link).
            //
            // So consult THREAD_PID_MAP, which records the slot's *current* owner. Only
            // an entry naming a different pid proves the slot was stolen; a missing entry
            // means nobody has claimed it, and terminating it is still the right thing
            // (that is the orphaned-READY-thread case the paragraph above describes).
            if let Some(tid) = p.thread_id {
                let current_tid = crate::threading::current_thread_id();
                let slot_owner = with_irqs_disabled(|| THREAD_PID_MAP.lock().get(&tid).copied());
                let recycled = matches!(slot_owner, Some(owner) if owner != pid);
                if recycled {
                    crate::safe_print!(112, "[unregister] pid={} stale tid={} now owned by pid={}\n",
                        pid, tid, slot_owner.unwrap_or(0));
                }
                if tid != current_tid && !recycled {
                    crate::threading::mark_thread_terminated(tid);
                }
            }
        },
    )
}

/// Free the memory of every RETIRED slot whose cooldown
/// (`config().process_reclaim_cooldown_us`) has elapsed. Returns the count freed.
///
/// No caller-identity gate (unlike thread-slot cleanup's "only thread 0" default) —
/// there is no steady-state collector here that only runs when the system is idle,
/// so there is nothing to relax. Called *only* periodically, from `netpoll_maint` —
/// `register_process` deliberately does not reclaim on a full-table miss; see the
/// comment there for the self-deadlock that ruled that call site out.
///
/// # Why a time cooldown and not a caller-identity or reference-count gate
/// Same reasoning as `threading::reclaim_terminated_slots`: the thing that makes
/// freeing safe is that any BKL-dropped window holding a stale pointer has had time
/// to close, not who calls this function. `process_reclaim_cooldown_us` must outlast
/// any such window; those windows are single bounded I/O ops or PTE-chunk copies, not
/// open-ended, so the same order of magnitude as `THREAD_CLEANUP_COOLDOWN_US` (10ms)
/// is generous. See docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md.
pub fn reclaim_retired_processes() -> usize {
    reclaim_retired_processes_internal(false)
}

/// Reclaim regardless of cooldown. **Tests only** — never safe in production, the
/// same way `threading::cleanup_terminated_force` is test-only: it removes the
/// margin that guarantees no peer core still holds a stale pointer into the slot
/// being freed.
pub fn reclaim_retired_processes_force() -> usize {
    reclaim_retired_processes_internal(true)
}

fn reclaim_retired_processes_internal(ignore_cooldown: bool) -> usize {
    // `SlotTable::reclaim_retired` does, per eligible RETIRED slot: swap the
    // pointer to null (so `register_process` can never race a half-freed slot),
    // bump the generation while the slot is still RETIRED (the ordering that is
    // the whole safety argument — every reader rejects a non-ACTIVE state before
    // reading the generation, so none can pair ACTIVE with the pre-bump stamp;
    // Release-paired with the Acquire in `identity_get` via `ref_if_current`),
    // store FREE, clear the retire stamp, run `on_free`, then drop the `Box`.
    // Two racers on one slot: one wins the swap and frees, the other skips.
    PROCESS_TABLE.reclaim_retired(
        (runtime().uptime_us)(),
        config().process_reclaim_cooldown_us,
        ignore_cooldown,
        |i| {
            crate::process::reclaim::clear_retired_slot(i);
        },
    )
}

/// Number of RETIRED slots awaiting `reclaim_retired_processes`. Diagnostics/tests only.
pub fn retired_process_count() -> usize {
    PROCESS_TABLE.retired_count()
}

/// Access a process by PID within a callback. **This is the safe API.**
///
/// The callback runs with IRQs disabled, guaranteeing the Process pointer
/// is valid for the entire duration (no other thread can free it).
///
/// The callback MUST NOT allocate on the heap. For operations that need
/// allocation, copy scalar fields inside the callback and allocate outside.
///
/// # Example
/// ```ignore
/// let name = table::with_process(pid, |p| p.name.clone()); // OK for short strings
/// let exit_code = table::with_process(pid, |p| p.exit_code); // preferred for scalars
/// ```
#[inline]
pub fn with_process<T, F: FnOnce(&mut Process) -> T>(pid: Pid, f: F) -> Option<T> {
    PROCESS_TABLE.with_active_mut(|p| p.pid == pid, f)
}

/// Shared `&'static Process` for `pid`, resolved under an IRQ mask.
/// `lookup_process_shared` is the one caller. The borrow's validity past the
/// mask rests on `akuma-slot-table`'s deferred-reclamation contract.
///
/// Replaced `get_process_ptr` (raw `*mut Process`) and the `unsafe fn
/// with_process_exclusive` — the execve/first-run destructive windows those
/// existed for now take `&Process` (`AKUMA_EXEC_AUDIT.md` §6.E group 2).
pub fn active_process_ref(pid: Pid) -> Option<&'static Process> {
    PROCESS_TABLE.active_ref(|p| p.pid == pid)
}

/// Iterate all active processes, calling `f` for each.
///
/// Runs entirely with IRQs disabled — the callback MUST NOT allocate.
/// For iteration that needs allocation, use `collect_pids` + per-PID lookup.
#[inline]
pub fn for_each_process<F: FnMut(&Process)>(mut f: F) {
    PROCESS_TABLE.for_each_active(|_, p| f(p));
}

/// Iterate all active processes, calling `f` for each. Returns early if `f` returns Some.
///
/// Runs entirely with IRQs disabled — the callback MUST NOT allocate.
#[inline]
pub fn find_process<T, F: FnMut(&Process) -> Option<T>>(mut f: F) -> Option<T> {
    PROCESS_TABLE.find_active(|_, p| f(p))
}

/// Collect PIDs matching a predicate.
///
/// Two-phase: scan with IRQs disabled (no allocation), then collect PIDs
/// into a Vec with IRQs enabled. Safe because PIDs are just u32 values
/// copied out during the scan.
pub fn collect_pids<F: FnMut(&Process) -> bool>(mut pred: F) -> Vec<Pid> {
    // Phase 1: scan into fixed-size stack buffer (no heap allocation)
    let mut buf = [0u32; MAX_PROCESSES];
    let mut count = 0usize;
    PROCESS_TABLE.for_each_active(|_, p| {
        if pred(p) && count < MAX_PROCESSES {
            buf[count] = p.pid;
            count += 1;
        }
    });
    // Phase 2: copy to Vec with IRQs enabled (safe to allocate)
    buf[..count].to_vec()
}

/// Collect (PID, thread_id, extra_field) tuples matching a predicate.
///
/// Same two-phase approach as `collect_pids` but captures additional fields.
/// Stack buffer holds up to MAX_PROCESSES entries.
pub fn collect_process_info<T: Copy + Default, F>(mut f: F) -> Vec<T>
where
    F: FnMut(&Process) -> Option<T>,
{
    let mut buf = [T::default(); MAX_PROCESSES];
    let mut count = 0usize;
    PROCESS_TABLE.for_each_active(|_, p| {
        if let Some(val) = f(p) {
            if count < MAX_PROCESSES {
                buf[count] = val;
                count += 1;
            }
        }
    });
    buf[..count].to_vec()
}

/// Number of active processes.
pub fn process_count() -> usize {
    PROCESS_TABLE.active_count()
}

/// `(total, running_or_ready, blocked)` — a scalar tally over every active
/// process's `state`, with no `Vec`. `/proc/stat`'s `procs_running`/
/// `procs_blocked` and `/proc/loadavg`'s `runnable/total` need exactly these
/// three numbers, and `top`/`free` (via those files) read them on every
/// refresh; `collect_process_info`/`list_processes` would answer the same
/// question but always allocate (a `Vec`, plus a `String` clone per process),
/// which is unnecessary for a plain count. Same locking shape as
/// `collect_process_info`: the slot pointers are only valid to dereference
/// with IRQs disabled, which `for_each_active` handles.
pub fn count_process_states() -> (usize, usize, usize) {
    let mut total = 0usize;
    let mut running = 0usize;
    let mut blocked = 0usize;
    PROCESS_TABLE.for_each_active(|_, p| {
        total += 1;
        match p.state.load() {
            crate::process::types::ProcessState::Ready
            | crate::process::types::ProcessState::Running => running += 1,
            crate::process::types::ProcessState::Blocked => blocked += 1,
            crate::process::types::ProcessState::Zombie(_) => {}
        }
    });
    (total, running, blocked)
}

// ── Thread PID map and lazy regions (unchanged) ─────────────────────────

/// Maps kernel thread IDs to PIDs for CLONE_THREAD children.
/// Needed because thread clones share the parent's ProcessInfo page, so
/// read_current_pid() would return the parent's PID.
pub static THREAD_PID_MAP: Spinlock<BTreeMap<usize, Pid>> =
    Spinlock::new(BTreeMap::new());

// ── Per-thread identity cache (the `current` this kernel never had) ──────
//
// `THREAD_PID_MAP` is authoritative but resolving through it costs a
// spinlock + a BTreeMap walk per lookup, and the interesting resolutions then
// need an IRQ-masked scan of the process table — the syscall entry path was
// doing that *nine times per excursion* (prologue identity, interrupt check,
// stats hooks, the dispatch arm's own `read_current_pid`, epilogue), which is
// the bulk of why a bare syscall cost 3x Linux
// (docs/archive/AKUMA_SYSCALL_PERFORMANCE_AUDIT.md).
//
// This array caches, per thread slot, both resolutions a caller can want:
//
// - **own**: the map's value for this thread — its per-thread `Process`
//   (`clone_thread` gives every thread its own `Process` sharing the group's
//   address space). `current_channel`/`current_process_shared` semantics.
// - **tgid**: the thread-group leader's pid and `Process` — what
//   `read_current_pid` + `lookup_process_shared(owner)` compute for the
//   syscall prologue, stats and log attribution.
//
// Invariant: entries are written **only** inside the same IRQ-masked critical
// section that writes `THREAD_PID_MAP` (`thread_pid_map_insert` /
// `thread_pid_map_remove`), so map and cache can never disagree for longer
// than one critical section. Every fast-path read re-validates the slot state
// and generation through `SlotTable::ref_if_current`, so a process that retired
// (ACTIVE→RETIRED) while a straggler thread still runs falls back to the slow
// path — exactly what an uncached lookup would do — instead of touching the entry.
//
// The pointer stays valid across RETIRED (deferred reclamation keeps the
// `Process` alive) and is cleared when the map entry is removed, which is the
// same lifetime `lookup_process_shared` documents for own-thread lookups.

/// Cached identity for one thread slot. 24 B × `MAX_THREADS` — a couple of
/// cache lines, touched by exactly one thread each.
pub struct ThreadIdentity {
    own_pid: AtomicU32,
    /// Slot of the OWN process, or `INVALID_SLOT` when empty/unresolved. Written
    /// with `Release` and read with `Acquire` — it is the **publication point**
    /// for the half, replacing the old `own_ptr` `Release` store (the pointer is
    /// no longer cached: `identity_get` derives it via `SlotTable::ref_if_current`
    /// from `slot` + `own_gen`, which within one generation is immutable and
    /// equal to what `own_ptr` used to hold).
    own_slot: AtomicU16,
    tgid: AtomicU32,
    tgid_slot: AtomicU16,
    /// `SlotTable` generation at the moment each half was stamped. A slot that
    /// has been recycled since reads ACTIVE with a different generation, which
    /// is the only thing that distinguishes "still ours" from "freed and
    /// re-issued".
    own_gen: AtomicU32,
    tgid_gen: AtomicU32,
    /// Failed lazy re-stamps for this entry, bounded by [`MAX_REPAIR_ATTEMPTS`].
    ///
    /// The stamp is written once, at `thread_pid_map_insert`. If the map insert
    /// raced ahead of `register_process` the pid does not resolve to a slot yet
    /// and the entry is stamped `INVALID_SLOT` — which used to be **permanent**,
    /// so that thread took the lock + map walk + table scan on every syscall for
    /// the rest of its life. Measured 2026-08-28 at a sub-1% hit rate under
    /// thread churn (`docs/archive/IDENTITY_CACHE_LAZY_RESTAMP.md`).
    ///
    /// `identity_get` now repairs such an entry on the miss, and this counter is
    /// what keeps an entry that will *never* resolve (its process died before
    /// registering) from paying a table scan per syscall forever. Reset to 0 on
    /// every successful stamp and on clear.
    repair_attempts: AtomicU8,
}

/// How many times a miss may pay a table scan trying to re-resolve before the
/// entry is left on the slow path for good.
///
/// The repairable population is threads whose `register_process` had not landed
/// at map-insert time, and that lands within microseconds — so the first attempt
/// after it does succeeds and every later syscall on that thread is a hit. A
/// handful of attempts covers that with room to spare; the bound exists only to
/// cap the unresolvable case, not to tune the common one.
const MAX_REPAIR_ATTEMPTS: u8 = 4;

const INVALID_SLOT: u16 = u16::MAX;

impl ThreadIdentity {
    const fn new() -> Self {
        Self {
            own_pid: AtomicU32::new(0),
            own_slot: AtomicU16::new(INVALID_SLOT),
            tgid: AtomicU32::new(0),
            tgid_slot: AtomicU16::new(INVALID_SLOT),
            own_gen: AtomicU32::new(0),
            tgid_gen: AtomicU32::new(0),
            repair_attempts: AtomicU8::new(0),
        }
    }
}

use crate::threading::types::MAX_THREADS;
static THREAD_IDENTITY: [ThreadIdentity; MAX_THREADS] = {
    const INIT: ThreadIdentity = ThreadIdentity::new();
    [INIT; MAX_THREADS]
};

/// Fast-path fallbacks: reads that missed the cache and took the slow path.
/// Diagnostic only — nonzero steady-state means a writer site bypassed the
/// wrappers and the map and cache have diverged.
pub static IDENTITY_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// The syscall epilogue found the identity its prologue resolved no longer
/// registered — `lookup_process_shared` returns `None` where the prologue had a
/// `Process`. The pre-cache epilogue re-did that lookup and skipped its writes
/// on `None`; `handle_syscall` now reuses the prologue's `&'static Process`
/// across the whole dispatch, so each count here is one excursion that wrote
/// through a pointer an uncached lookup would have refused.
///
/// Only counted when `config::IDENTITY_AUDIT` is on (the check costs exactly
/// the lookup the cache removed). See `docs/archive/IDENTITY_CACHE_SMP_REVIEW.md`.
pub static EPILOGUE_STALE_IDENTITY: AtomicU64 = AtomicU64::new(0);

/// As above, but the pid still resolves — to a *different* `Process`. The slot
/// was retired, reclaimed and reissued while the dispatch ran, so the prologue's
/// pointer is dangling rather than merely retired.
pub static EPILOGUE_IDENTITY_MOVED: AtomicU64 = AtomicU64::new(0);

/// Miss breakdown for [`IDENTITY_FALLBACKS`], which on its own is unreadable:
/// it sums causes with completely different meanings. Measured under a SMP=4
/// forktest load the raw total ran at ~5 per syscall, which says nothing about
/// whether the cache is broken or merely being consulted by threads that have
/// no identity to cache.
///
/// - `UNSTAMPED` — the entry was never written (`own_pid == 0`): a kernel/idle
///   thread, or a user thread resolving before `thread_pid_map_insert` lands.
///   Expected; costs only the slow path.
/// - `CLEARED` — a pid is stamped but it does not resolve to a live slot, and
///   the bounded lazy re-stamp did not fix it. **Its meaning narrowed on
///   2026-08-28.** It used to also cover entries invalidated by
///   `thread_pid_map_remove`, which stored `INVALID_SLOT` while leaving
///   `own_pid` set — so a thread that had simply been torn down, and one that
///   never resolved at all, landed in the same bucket. That is why this counter
///   read 556M against 164 UNSTAMPED in the measurement that prompted the fix:
///   it was not reporting cache loss, it was reporting a permanently
///   unresolved stamp. `identity_clear_locked` now zeroes the pids, so a
///   cleared entry reads as UNSTAMPED and this counter means only what its
///   description says.
/// - `INACTIVE` — cached slot is no longer ACTIVE (retired/free). This is the
///   guard that makes a retired process fall back correctly.
/// - `NULL` — cached pointer is null.
pub static IDENTITY_FB_UNSTAMPED: AtomicU64 = AtomicU64::new(0);
pub static IDENTITY_FB_CLEARED: AtomicU64 = AtomicU64::new(0);
pub static IDENTITY_FB_INACTIVE: AtomicU64 = AtomicU64::new(0);
pub static IDENTITY_FB_NULL: AtomicU64 = AtomicU64::new(0);

/// Cached slot is ACTIVE but has been **recycled** since it was stamped — a
/// different process now occupies it. Every one of these is a use-after-free
/// that did not happen: before the generation check
/// (`IDENTITY_CACHE_SMP_REVIEW.md` Finding B) this read passed validation and
/// dereferenced a freed `Process`.
///
/// A nonzero value is therefore **expected and healthy** — it is the guard
/// working, not a defect. What it does say is that the window is live on this
/// workload, which inspection alone could not establish.
pub static IDENTITY_FB_STALE_GEN: AtomicU64 = AtomicU64::new(0);

/// Misses that the lazy re-stamp turned back into a live cache entry, and misses
/// where the re-scan still could not resolve the pid.
///
/// `repairs` is the whole point of the fix: each one converts a thread that
/// would have taken the slow path for the rest of its life into a cache hit
/// from that syscall on, so this counts *threads rescued*, not work done.
/// `repair_failed` is the bounded waste — at most [`MAX_REPAIR_ATTEMPTS`] table
/// scans per entry — and a large value means processes are dying before they
/// register, which is a different bug than this one.
pub static IDENTITY_REPAIRS: AtomicU64 = AtomicU64::new(0);
pub static IDENTITY_REPAIR_FAILED: AtomicU64 = AtomicU64::new(0);

/// Successful cache resolutions — the denominator without which the miss counts
/// above cannot be turned into a hit rate. Only counted while [`IDENTITY_STATS`]
/// is on, since this increment would otherwise sit on the kernel's hottest path.
pub static IDENTITY_HITS: AtomicU64 = AtomicU64::new(0);

/// Enables [`IDENTITY_HITS`]. Set at boot from `config::IDENTITY_AUDIT`.
pub static IDENTITY_STATS: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Resolve `pid` in the table and store both identity halves for `tid`.
/// Caller must hold IRQs masked (scans the slots via `find_active_locked`).
/// An unresolvable pid (insert raced ahead of `register_process`) stores the
/// invalid marker: fast paths miss, slow paths answer, nothing is wrong.
fn identity_store_locked(tid: usize, pid: Pid) {
    if tid >= MAX_THREADS {
        return;
    }
    let e = &THREAD_IDENTITY[tid];
    // First pass: the own-pid slot. A non-`CLONE_THREAD` process is its own tgid
    // leader, so this pass fills the tgid half too.
    let mut own_slot = INVALID_SLOT;
    let mut tgid = pid;
    let mut tgid_slot = INVALID_SLOT;
    PROCESS_TABLE.find_active_locked::<()>(|i, p| {
        if p.pid == pid {
            own_slot = i as u16;
            tgid = p.tgid;
            tgid_slot = i as u16;
            Some(())
        } else {
            None
        }
    });
    // A CLONE_THREAD child's own pid differs from its tgid: resolve the leader
    // in a second pass (the first pass stopped at the own hit).
    if tgid != pid {
        tgid_slot = PROCESS_TABLE
            .find_active_locked(|i, p| (p.pid == tgid).then_some(i as u16))
            .unwrap_or(INVALID_SLOT);
    }
    // Stamp the generation BEFORE publishing the slot (the `Release` store on
    // `*_slot` below is the publication point — it replaces the old `*_ptr`
    // `Release`), so a reader that sees the new slot with `Acquire` cannot still
    // see the old generation stamp.
    if own_slot != INVALID_SLOT {
        e.own_gen.store(PROCESS_TABLE.generation(own_slot as usize), Ordering::Relaxed);
    }
    if tgid_slot != INVALID_SLOT {
        e.tgid_gen.store(PROCESS_TABLE.generation(tgid_slot as usize), Ordering::Relaxed);
    }
    e.own_pid.store(pid, Ordering::Relaxed);
    e.tgid.store(tgid, Ordering::Relaxed);
    e.own_slot.store(own_slot, Ordering::Release);
    e.tgid_slot.store(tgid_slot, Ordering::Release);
    // Fresh budget only when BOTH halves resolved. The bound in `identity_get`
    // is unreachable otherwise, and it has now been got wrong twice in the same
    // way, so the reasoning is worth spelling out:
    //
    //   * Resetting unconditionally means each failed repair zeroes the counter
    //     it is about to increment. The entry sits at 1 attempt forever and
    //     re-scans the whole table on every syscall — the permanent-scan
    //     regression the bound exists to prevent. Caught by the boot test:
    //     `attempts=1 failed_delta=7 budget=4`.
    //   * Resetting on `own_slot` alone has the same effect for a thread whose
    //     OWN half resolves and whose TGID half does not — a `CLONE_THREAD`
    //     sibling whose group leader has exited, which is a *normal* state, not
    //     an exotic one. Every `current_thread_tgid_process()` (i.e. every
    //     syscall prologue) then repaired, reset, and repaired again, at TWO
    //     table scans a time since `identity_store_locked` makes a second pass
    //     for the tgid half. Caught by `pthread_kill_eintr` timing out at
    //     SMP=1 — 420 s against an `ok` baseline, reproduced 2/2, while SMP=4
    //     stayed green because the other cores absorbed the scans.
    //
    // A half that cannot resolve must be allowed to exhaust the budget and stop.
    // The other half is unaffected: once `own_slot` is valid, `identity_get`
    // hits on it and never reaches the repair path at all.
    if own_slot != INVALID_SLOT && tgid_slot != INVALID_SLOT {
        e.repair_attempts.store(0, Ordering::Relaxed);
    }
}

/// Invalidate `tid`'s cached identity. Caller must hold IRQs masked.
fn identity_clear_locked(tid: usize) {
    if tid >= MAX_THREADS {
        return;
    }
    let e = &THREAD_IDENTITY[tid];
    e.own_slot.store(INVALID_SLOT, Ordering::Release);
    e.tgid_slot.store(INVALID_SLOT, Ordering::Release);
    // The pids go too, and that is load-bearing rather than tidiness: this is
    // the DELIBERATE invalidation (`thread_pid_map_remove`), and `identity_get`
    // decides whether a miss may be lazily re-stamped by asking whether a pid is
    // still stamped. Leaving `own_pid` set would let the repair path resolve the
    // process again and hand back an identity this call just revoked — undoing
    // the only sanctioned invalidation in the cache. Zeroing makes a cleared
    // entry read as UNSTAMPED, which is both unrepairable and the honest
    // description: this thread has no identity, rather than a lost one.
    e.own_pid.store(0, Ordering::Relaxed);
    e.tgid.store(0, Ordering::Relaxed);
    e.repair_attempts.store(0, Ordering::Relaxed);
}

/// Publish `tid` → `pid` in `THREAD_PID_MAP` **and** refresh the identity
/// cache, atomically w.r.t. both. Every non-test map insert must go through
/// here — a bare `THREAD_PID_MAP.lock().insert` leaves a stale cache entry
/// behind (or none, so the thread runs on the slow path forever).
pub fn thread_pid_map_insert(tid: usize, pid: Pid) {
    with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().insert(tid, pid);
        identity_store_locked(tid, pid);
    });
}

/// Remove `tid`'s map entry **and** invalidate the identity cache, atomically.
/// Every non-test map removal must go through here. Returns the removed pid
/// (the map's `remove` semantics — `on_thread_cleanup` decides reaping on it).
pub fn thread_pid_map_remove(tid: usize) -> Option<Pid> {
    with_irqs_disabled(|| {
        let prev = THREAD_PID_MAP.lock().remove(&tid);
        identity_clear_locked(tid);
        prev
    })
}

/// Read one half of the current thread's cached identity, re-validating the
/// slot through `SlotTable::ref_if_current` (state first, then generation, then
/// pointer). `own=false` selects the tgid-leader half.
fn identity_get(own: bool) -> Option<(Pid, &'static Process)> {
    let tid = crate::threading::current_thread_id();
    if tid >= MAX_THREADS {
        return None;
    }
    let e = &THREAD_IDENTITY[tid];
    let mut slot = if own { e.own_slot.load(Ordering::Acquire) } else { e.tgid_slot.load(Ordering::Acquire) };
    if slot == INVALID_SLOT {
        // Split the miss by cause: a thread that never had an identity (kernel
        // /idle thread) or one whose entry was deliberately cleared reads as
        // UNSTAMPED and is left alone. An entry that carries a pid but no slot
        // is the repairable class — `identity_store_locked` ran before
        // `register_process` and stamped the invalid marker.
        let stamped_pid = e.own_pid.load(Ordering::Relaxed);
        if stamped_pid == 0 {
            IDENTITY_FALLBACKS.fetch_add(1, Ordering::Relaxed);
            IDENTITY_FB_UNSTAMPED.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        // Lazy re-stamp. Before this existed the marker was permanent and the
        // thread paid the slow path for the rest of its life; measured at a
        // sub-1% hit rate under thread churn, ~10M fallbacks/s
        // (docs/archive/IDENTITY_CACHE_LAZY_RESTAMP.md). The repair is bounded
        // so an entry that can never resolve does not trade one permanent slow
        // path for a permanent table scan, which would be strictly worse.
        //
        // IRQs are masked only for the scan, which is what
        // `identity_store_locked` documents as its precondition. Nesting is
        // fine (`IrqGuard` saves and restores DAIF) and nothing here takes
        // `THREAD_PID_MAP`, so this cannot deadlock against the insert path
        // that calls the same function with the lock held.
        if e.repair_attempts.load(Ordering::Relaxed) >= MAX_REPAIR_ATTEMPTS {
            IDENTITY_FALLBACKS.fetch_add(1, Ordering::Relaxed);
            IDENTITY_FB_CLEARED.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        with_irqs_disabled(|| identity_store_locked(tid, stamped_pid));
        slot = if own { e.own_slot.load(Ordering::Acquire) } else { e.tgid_slot.load(Ordering::Acquire) };
        if slot == INVALID_SLOT {
            // Still unresolvable. `identity_store_locked` reset the budget on
            // its way out (it resets unconditionally), so charge the attempt
            // AFTER it, or the bound never bites and this becomes the per-
            // syscall table scan it exists to prevent.
            e.repair_attempts.fetch_add(1, Ordering::Relaxed);
            IDENTITY_FALLBACKS.fetch_add(1, Ordering::Relaxed);
            IDENTITY_FB_CLEARED.fetch_add(1, Ordering::Relaxed);
            IDENTITY_REPAIR_FAILED.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        IDENTITY_REPAIRS.fetch_add(1, Ordering::Relaxed);
    }
    let slot = slot as usize;
    // Finding B: ACTIVE alone cannot tell "still ours" from "freed and
    // re-issued" — the slot may have gone ACTIVE → RETIRED → freed → FREE →
    // claimed since we cached it. `SlotTable::ref_if_current` reads state FIRST,
    // generation SECOND (reclaim bumps it while the slot is RETIRED, so ACTIVE
    // paired with a matching stamp means the occupant we stamped is still
    // installed), pointer THIRD. Within one generation the slot's pointer is
    // immutable — written once by `try_claim` after the CAS, nulled once by
    // reclaim's swap — so a matching stamp already proves the cached pid is
    // right, which is why no pid re-check is needed and the `Process` cache
    // line is never touched on a hit.
    let stamped_gen = if own { e.own_gen.load(Ordering::Relaxed) } else { e.tgid_gen.load(Ordering::Relaxed) };
    let proc = match PROCESS_TABLE.ref_if_current(slot, stamped_gen) {
        Ok(p) => p,
        Err(SlotMiss::Inactive) => {
            IDENTITY_FALLBACKS.fetch_add(1, Ordering::Relaxed);
            IDENTITY_FB_INACTIVE.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Err(SlotMiss::StaleGen) => {
            IDENTITY_FALLBACKS.fetch_add(1, Ordering::Relaxed);
            IDENTITY_FB_STALE_GEN.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Err(SlotMiss::Null) => {
            IDENTITY_FALLBACKS.fetch_add(1, Ordering::Relaxed);
            IDENTITY_FB_NULL.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };
    // Gated: the miss paths above are meant to be rare, but a hit counter sits
    // on the hottest path in the kernel, so it must not cost anything in a
    // shipping build. One relaxed load of a hot static when disabled.
    if IDENTITY_STATS.load(Ordering::Relaxed) {
        IDENTITY_HITS.fetch_add(1, Ordering::Relaxed);
    }
    let pid = if own { e.own_pid.load(Ordering::Relaxed) } else { e.tgid.load(Ordering::Relaxed) };
    Some((pid, proc))
}

/// Test hooks for the lazy re-stamp (`process_tests::test_identity_lazy_restamp`).
///
/// The repair only fires on an entry that carries a pid but no slot, and that
/// state is produced by a race (`thread_pid_map_insert` landing before
/// `register_process`) which a boot test cannot schedule. These let a test put
/// an entry into it deliberately, and read back what the repair did.
pub mod test_hooks {
    use super::*;

    /// Force `tid`'s entry into the repairable state: pid stamped, slot invalid,
    /// repair budget full. This is exactly what `identity_store_locked` leaves
    /// behind when it cannot resolve `pid`.
    pub fn stamp_unresolved(tid: usize, pid: Pid) {
        if tid >= MAX_THREADS {
            return;
        }
        let e = &THREAD_IDENTITY[tid];
        e.own_pid.store(pid, Ordering::Relaxed);
        e.own_slot.store(INVALID_SLOT, Ordering::Release);
        e.tgid.store(pid, Ordering::Relaxed);
        e.tgid_slot.store(INVALID_SLOT, Ordering::Release);
        e.repair_attempts.store(0, Ordering::Relaxed);
    }

    /// `(own_pid, own_slot, repair_attempts)` — enough to tell a repaired entry
    /// from one that gave up.
    pub fn state(tid: usize) -> (Pid, u16, u8) {
        if tid >= MAX_THREADS {
            return (0, INVALID_SLOT, 0);
        }
        let e = &THREAD_IDENTITY[tid];
        (
            e.own_pid.load(Ordering::Relaxed),
            e.own_slot.load(Ordering::Relaxed),
            e.repair_attempts.load(Ordering::Relaxed),
        )
    }

    /// Restore an entry to "no identity", the state a kernel thread sits in.
    pub fn clear(tid: usize) {
        with_irqs_disabled(|| identity_clear_locked(tid));
    }

    pub fn max_repair_attempts() -> u8 {
        MAX_REPAIR_ATTEMPTS
    }

    /// `(own_slot, stamped own generation, live generation of that slot)`.
    /// A test asserts on the last two diverging after a recycle.
    pub fn generation_state(tid: usize) -> (u16, u32, u32) {
        if tid >= MAX_THREADS {
            return (INVALID_SLOT, 0, 0);
        }
        let e = &THREAD_IDENTITY[tid];
        let slot = e.own_slot.load(Ordering::Relaxed);
        let live = if slot == INVALID_SLOT || slot as usize >= MAX_PROCESSES {
            0
        } else {
            PROCESS_TABLE.generation(slot as usize)
        };
        (slot, e.own_gen.load(Ordering::Relaxed), live)
    }

    pub const INVALID_SLOT_FOR_TEST: u16 = INVALID_SLOT;
}

/// The current thread's **tgid** and its thread-group leader's `Process` —
/// the `read_current_pid()` + `lookup_process_shared(owner)` pair the syscall
/// entry path wants, in two validated loads. `None` → use the slow paths.
pub fn current_thread_tgid_process() -> Option<(Pid, &'static Process)> {
    identity_get(false)
}

/// The current thread's **own** pid and per-thread `Process` — the
/// `current_pid()` + `lookup_process_shared` pair (`current_process_shared`
/// semantics) in two validated loads.
pub fn current_thread_own_process() -> Option<(Pid, &'static Process)> {
    identity_get(true)
}

pub fn register_thread_pid(tid: usize, pid: Pid) {
    thread_pid_map_insert(tid, pid);
}

pub fn unregister_thread_pid(tid: usize) {
    thread_pid_map_remove(tid);
}

/// The pid that currently owns thread slot `tid`, or `None` if nobody claims it.
///
/// This is the **authoritative** tid→pid mapping. `fork_process`,
/// `vfork_process` and `clone_thread` all publish it before the child can be
/// scheduled, `current_process_shared` already trusts it, and
/// `unregister_process` uses it to decide whether a slot has been recycled out
/// from under a dying process. Prefer it over scanning the process table for
/// `p.thread_id == Some(tid)`: `thread_id` is a *recorded* slot number that
/// several teardown paths deliberately leave set, so the scan can match a
/// stale process and — because `find_process` returns the first ACTIVE slot —
/// a stale entry at a lower slot index wins.
pub fn pid_for_thread(tid: usize) -> Option<Pid> {
    with_irqs_disabled(|| THREAD_PID_MAP.lock().get(&tid).copied())
}

/// The live thread slot owned by `pid`, or `None` if `pid` has no live thread.
///
/// The authoritative inverse of [`pid_for_thread`], and the reason it exists is
/// the same: `Process::thread_id` is a *recorded* slot number that teardown
/// paths deliberately leave set, so a process that lost its thread still names a
/// slot — one that may since have been recycled to an unrelated process. Every
/// path that acts *on* a slot (terminate it, post a deferred kill to it, drop its
/// map entry) must resolve through here, or it acts on whoever holds the slot now.
pub fn thread_for_pid(pid: Pid) -> Option<usize> {
    with_irqs_disabled(|| {
        THREAD_PID_MAP
            .lock()
            .iter()
            .find_map(|(tid, owner)| (*owner == pid).then_some(*tid))
    })
}
