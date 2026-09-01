//! Pressure-driven reclaim of RETIRED processes.
//!
//! # The gap this closes
//!
//! Since Phase 7e's "Free" half a dead process's memory returns to the PMM only when
//! [`table::reclaim_retired_processes`] drops the RETIRED `Process`
//! (`UserAddressSpace::drop` frees every user + page-table frame). Until this module
//! existed that function had exactly one steady-state caller — the `netpoll_maint` arm
//! of the main loop, on a 100 ms cadence — so the reclaim that matters most under
//! memory pressure was scheduled by a component that pressure can starve. A large
//! process OOM-killed while that collector is blocked, wedged, or not yet spawned
//! stranded its entire address space (measured: ~35,441 pages / ~138 MB parked, PMM
//! free pinned at `USER_PAGE_RESERVE` for 500 polls). See
//! `docs/archive/OOM_KILL_DEFERRED_RECLAIM_GAP.md`.
//!
//! # Why a request flag and not just "call reclaim from the allocator"
//!
//! Reclaiming runs arbitrary `Process::drop` code: it frees page-table frames (PMM
//! lock), releases an ASID (`ASID_ALLOCATOR`), drops the `Arc<SharedFdTable>` (which
//! can reach VFS/socket close), and touches `SHARED_L0_TABLE`. A caller already
//! holding one of those locks self-deadlocks the instant reclaim retakes it — that is
//! the boot hang that ruled out reclaiming from [`table::register_process`]'s
//! full-table miss (`docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md`, "the
//! on-demand reclaim that wasn't"), and it is why the gap doc's §4 insists any fix
//! add a call site with *known, minimal ambient lock context*.
//!
//! So this module splits the two halves of "reclaim under pressure":
//!
//! - [`request_retired_reclaim`] — three atomic ops, no locks, no drop code. Safe from
//!   anywhere, including contexts that hold drop-path locks or run in an IRQ.
//! - [`drain_retired`] / [`drain_retired_if_requested`] — actually runs the drop. Only
//!   ever called from the vetted sites enumerated below.
//!
//! # The vetted drain sites
//!
//! 1. **Terminal teardown** — `return_to_kernel` / `return_to_kernel_from_fault`, after
//!    the lifecycle guard is released and IRQs are re-enabled, immediately before the
//!    terminal yield loop. The OOM-kill path itself (gap doc §5 candidate 1): the
//!    address space is deactivated, fds are cleaned, the dropped-window ledger is
//!    reset, the trap frame is cleared, and this thread will never touch user state
//!    again. Under a kill storm each kill frees every predecessor whose cooldown has
//!    elapsed, so peak demand is one stranded address space plus those still cooling,
//!    not the sum of all of them.
//! 2. **Idle loops** — thread 0's idle loop (and the secondary-core equivalent). This
//!    is the regime where `netpoll_maint` starves: if pressure is bad enough to block
//!    the maintenance thread, something is blocked, and the idle loop is what runs.
//! 3. **`pmm::alloc_page_zeroed_user`'s pressure ladder** — a rung *above*
//!    `reclaim_clean_file_pages`, because freeing a dead process's pages costs no
//!    re-fault while evicting a live process's mapped pages costs a disk read per page.
//!
//! Every one of those sites is *outside* any drop-path lock. Nothing else may call the
//! drain: request instead, and one of these will pick it up.
//!
//! # What is deliberately unchanged
//!
//! The RETIRE cooldown. It is what guarantees no peer core still holds a raw
//! `*Process` from a BKL-dropped window, so the drain calls the cooldown-honoring
//! [`table::reclaim_retired_processes`] and never the `_force` variant (tests only).
//! A consequence worth stating: a thread draining from its own teardown can never free
//! its *own* `Process` — that slot was retired microseconds ago and is still inside its
//! cooldown by construction.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::process::table;
use crate::threading::types::MAX_THREADS;

/// Runtime toggle (default **on**) for pressure-driven reclaim, exposed both as a kill
/// switch and so a boot self-test can A/B it in a single boot with a single binary
/// (`test_retired_reclaim_pressure_ab`) — the same-binary A/B discipline
/// `docs/reference/subsystems/locking.md` rule 5 requires. With it off, the only
/// collector is the 100 ms `netpoll_maint` one, i.e. the pre-fix behaviour exactly.
static PRESSURE_RECLAIM_ENABLED: AtomicBool = AtomicBool::new(true);

/// Set whenever parked RETIRED memory is known to exist and nobody has collected it
/// yet. Cleared by a drain that leaves the table empty of RETIRED slots — so a drain
/// interrupted (or permanently preempted, as the terminal-teardown site can be) leaves
/// the request standing for the next collector instead of dropping it on the floor.
static RECLAIM_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Reentrancy guard, per thread. Recursion into the drain is structurally impossible
/// today — the three drain sites are unreachable from `Process::drop` — but this makes
/// "safe to call from the pressure ladder" a property of the code rather than of an
/// audit that a future call site could invalidate. Indexed by thread id; threads at or
/// above [`MAX_THREADS`] (synthetic test ids) simply skip the guard.
static DRAINING: [AtomicBool; MAX_THREADS] = [const { AtomicBool::new(false) }; MAX_THREADS];

/// Resident page count stamped into each slot when it retires, so pressure decisions
/// and diagnostics can read "how much is parked" without dereferencing a RETIRED
/// `Process` (which would race the very drop this module schedules). Scalar and
/// lock-free by construction.
static RETIRED_PAGES: [AtomicU32; table::MAX_PROCESSES] =
    [const { AtomicU32::new(0) }; table::MAX_PROCESSES];

/// Running total of pages returned to the PMM by pressure-driven drains. Diagnostics.
static PRESSURE_RECLAIMED_PAGES: AtomicU64 = AtomicU64::new(0);

/// Whether pressure-driven reclaim is currently enabled.
#[inline]
pub fn pressure_reclaim_enabled() -> bool {
    PRESSURE_RECLAIM_ENABLED.load(Ordering::Relaxed)
}

/// Enable/disable pressure-driven reclaim at runtime (A/B measurement + kill switch).
pub fn set_pressure_reclaim_enabled(on: bool) {
    PRESSURE_RECLAIM_ENABLED.store(on, Ordering::Relaxed);
}

/// Note that reclaimable memory exists. Lock-free and IRQ-safe: this is the half of
/// the mechanism that is safe to call from contexts holding drop-path locks (the
/// full-table miss in `register_process`, an allocation failure deep inside a syscall,
/// an interrupt). It runs no `Process::drop` code — a vetted drain site does that
/// later. See the module docs.
#[inline]
pub fn request_retired_reclaim() {
    RECLAIM_REQUESTED.store(true, Ordering::Release);
}

/// Whether a drain is currently wanted.
#[inline]
pub fn reclaim_requested() -> bool {
    RECLAIM_REQUESTED.load(Ordering::Acquire)
}

/// Record a slot's resident page count as it retires, and request a drain.
/// Called from [`table::unregister_process`] under the same conditions that set
/// `RETIRE_TIME`.
#[inline]
pub fn note_retired(slot: usize, pages: u32) {
    if slot < table::MAX_PROCESSES {
        RETIRED_PAGES[slot].store(pages, Ordering::Release);
    }
    request_retired_reclaim();
}

/// Clear a slot's stamp as it is freed. Called from the reclaim sweep.
#[inline]
pub fn clear_retired_slot(slot: usize) -> u32 {
    if slot < table::MAX_PROCESSES {
        RETIRED_PAGES[slot].swap(0, Ordering::AcqRel)
    } else {
        0
    }
}

/// Pages currently parked in RETIRED slots — the reclaimable memory that exists right
/// now. Cheap (a scan of `MAX_PROCESSES` atomics, no locks), so it is safe to consult
/// from the allocator's pressure path before deciding to attempt a drain.
pub fn retired_pages_pending() -> usize {
    let mut total = 0usize;
    for i in 0..table::MAX_PROCESSES {
        total += RETIRED_PAGES[i].load(Ordering::Relaxed) as usize;
    }
    total
}

/// Total pages returned to the PMM by pressure-driven drains since boot.
pub fn pressure_reclaimed_pages() -> u64 {
    PRESSURE_RECLAIMED_PAGES.load(Ordering::Relaxed)
}

/// Run the cooldown-honoring reclaim from a vetted drain site. Returns the number of
/// processes freed.
///
/// **Only call this from the sites enumerated in the module docs.** From anywhere else
/// use [`request_retired_reclaim`].
pub fn drain_retired() -> usize {
    if !pressure_reclaim_enabled() {
        return 0;
    }
    let tid = crate::threading::current_thread_id();
    let guarded = tid < MAX_THREADS;
    if guarded && DRAINING[tid].swap(true, Ordering::AcqRel) {
        // Reentered from inside a drain (see the guard's docs) — the outer sweep owns
        // this; leave the request standing for it.
        request_retired_reclaim();
        return 0;
    }

    let pages_before = retired_pages_pending();
    let freed = table::reclaim_retired_processes();
    let still_parked = table::retired_process_count() > 0;
    // The flag means "parked memory exists and nobody has collected it". Recomputing it
    // from the table (rather than unconditionally clearing) is what makes a drain that
    // is permanently preempted mid-sweep — the terminal-teardown site can be, it runs
    // on an already-terminated thread — self-healing: whatever it did not reach stays
    // requested for the next collector.
    RECLAIM_REQUESTED.store(still_parked, Ordering::Release);
    PRESSURE_RECLAIMED_PAGES.fetch_add(
        pages_before.saturating_sub(retired_pages_pending()) as u64,
        Ordering::Relaxed,
    );

    if guarded {
        DRAINING[tid].store(false, Ordering::Release);
    }
    freed
}

/// Clear a recycled slot's re-entrancy guard.
///
/// `drain_retired` sets `DRAINING[tid]` and clears it on the way out, but its own docs
/// note the terminal-teardown site "runs on an already-terminated thread" and can be
/// permanently preempted mid-sweep. That leaves the flag set, and once the slot is
/// recycled the next occupant is treated as already inside a drain forever — it takes the
/// early return on every call and never collects. Called from `scrub_thread_slot`.
pub(crate) fn clear_draining(tid: usize) {
    if tid < MAX_THREADS {
        DRAINING[tid].store(false, Ordering::Release);
    }
}

/// [`drain_retired`], skipped when nothing is parked. The form the hot drain sites use.
#[inline]
pub fn drain_retired_if_requested() -> usize {
    // Piggyback the TTBR-deferred frame drain on the same periodic sites: frames
    // parked because a core's TTBR0 was still on a dying L0 (mmu::any_core_on_l0)
    // must eventually free even if no further address space ever drops. Lock-free
    // count check inside; costs nothing when the list is empty.
    crate::mmu::drain_pending_ttbr_frees();
    if !reclaim_requested() {
        return 0;
    }
    drain_retired()
}

/// Attempt a pressure-driven drain from the allocator's user-page path.
///
/// Distinct from [`drain_retired`] only in that it declines when there is nothing worth
/// collecting, so the common (no zombies parked) allocation-failure path pays one
/// lock-free scan and no more. The caller re-checks the free count itself.
pub fn drain_retired_under_pressure() -> usize {
    if !pressure_reclaim_enabled() || retired_pages_pending() == 0 {
        return 0;
    }
    drain_retired()
}

// ============================================================================
// PMM diagnostics that needed the process table — moved here 2026-09-01
// ============================================================================
//
// `surviving_mapper` used to live in `src/pmm.rs` as the one bridge hook
// `akuma-pmm` could never own ("walks the process table, which can never move
// into a crate below `akuma-exec`"). It still can't move into `akuma-pmm` — but
// it never needed `src/` either: `akuma-exec` is above `akuma-pmm`, owns the
// process table, and already registers callbacks downward
// (`drain_retired_under_pressure` above is another `PmmHooks` target). So the
// fn moved here and `akuma_exec::init` registers it, which retires the
// `src/main.rs` registration line. `report_poison_value` came along: its only
// other needs are `akuma_pmm`'s poison codec and the RAM window from
// `crate::mmu` — all inside this crate.

/// **Permanent.** The first live address space (other than one this thread is
/// tearing down) that still tracks `pa` as one of its user frames, as
/// `(pid, tgid)`. `find_process` is lock-free (slot-state atomics + raw
/// pointers under an IRQ guard); RETIRED slots are skipped, so a process being
/// reaped cannot report itself.
pub fn surviving_mapper(pa: usize) -> Option<(u32, u32)> {
    table::find_process(|p| {
        if p.address_space.tracks_user_frame(pa) {
            Some((p.pid, p.tgid))
        } else {
            None
        }
    })
}

/// If `word` is a quarantine poison word, the frame it was written for.
///
/// This is what turns the null-`Rc` crash from "a qword read back as garbage"
/// into "frame P, freed by thread T at free-seq S". Returns `None` when the
/// quarantine is compiled off, since nothing writes poison then and any match
/// would be a coincidence.
fn poison_word_frame(word: u64) -> Option<usize> {
    if !akuma_pmm::config().pmm_uaf_quarantine {
        return None;
    }
    akuma_pmm::poison_decode(word, crate::mmu::ram_base(), crate::mmu::ram_end())
}

/// Report a value that decoded as quarantine poison, naming the frame it
/// belonged to, who freed it and how its reference count got to zero. Called
/// from the fault path with whatever registers the faulting instruction used.
pub fn report_poison_value(tag: &str, word: u64) {
    let Some(pa) = poison_word_frame(word) else { return };
    let (tid_freed, seq_freed, site) = akuma_pmm::last_free_record_at(pa)
        .unwrap_or((u32::MAX, 0, akuma_pmm::FreeSite::Unknown));
    crate::safe_print!(255,
        "[PMM-POISON] {}={:#x} is quarantine poison for pa={:#x} — the kernel FREED \
         this frame while the process still had it. freed_by=(tid={} seq={} site={}) now_seq={} cow_ref={}\n",
        tag, word, pa, tid_freed, seq_freed, site.name(),
        akuma_pmm::free_ledger_seq(), akuma_pmm::cow_ref_get(pa));
    if let Some((pid, tgid)) = surviving_mapper(pa) {
        crate::safe_print!(128,
            "  [PMM-POISON] pa={:#x} still tracked by pid={} tgid={}\n", pa, pid, tgid);
    }
    akuma_pmm::print_cow_history(pa);
}

#[cfg(test)]
mod tests {
    use super::*;

    // These share process-global statics with every other host test in this crate, so
    // each one touches a distinct high slot index and asserts only per-slot facts or
    // monotone bounds — never an exact global sum.

    #[test]
    fn retiring_a_slot_requests_a_drain() {
        note_retired(table::MAX_PROCESSES - 1, 7);
        assert!(reclaim_requested());
        assert_eq!(clear_retired_slot(table::MAX_PROCESSES - 1), 7);
    }

    #[test]
    fn stamped_pages_are_counted_then_cleared() {
        let slot = table::MAX_PROCESSES - 2;
        assert_eq!(clear_retired_slot(slot), 0);
        note_retired(slot, 123);
        assert!(retired_pages_pending() >= 123);
        assert_eq!(clear_retired_slot(slot), 123);
        assert_eq!(clear_retired_slot(slot), 0);
    }

    #[test]
    fn out_of_range_slot_is_ignored() {
        note_retired(table::MAX_PROCESSES + 5, 999);
        assert_eq!(clear_retired_slot(table::MAX_PROCESSES + 5), 0);
    }

    #[test]
    fn nothing_parked_means_the_pressure_rung_declines() {
        // The allocator's rung must not sweep the table just because an allocation
        // failed; with no stamped pages it returns immediately.
        let slot = table::MAX_PROCESSES - 3;
        assert_eq!(clear_retired_slot(slot), 0);
        if retired_pages_pending() == 0 {
            assert_eq!(drain_retired_under_pressure(), 0);
        }
    }
}
