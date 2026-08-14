use super::*;

/// Each waiter is recorded as `(handle, bitset)`. The bitset is `FUTEX_BITSET_MATCH_ANY`
/// (`0xFFFFFFFF`) for plain `FUTEX_WAIT`/`FUTEX_WAKE`, or `val3` for `FUTEX_WAIT_BITSET`.
/// `FUTEX_WAKE_BITSET` only drains waiters whose bitset intersects its own `val3` —
/// ignoring it lets a `val=1` wake be eaten by a non-matching waiter (see the
/// "Known divergences" note in `docs/reference/subsystems/syscalls/sync.md`).
///
/// The wake target is a generation-tagged [`WakeHandle`], NOT a bare tid: a bare tid
/// popped from this table and then held across a preemption (`futex_do_wake` fires the
/// wakes outside the table hold, deliberately) can outlive the thread it named and wake
/// whoever owns the recycled slot instead. The handle is minted by the waiter itself at
/// enqueue time and goes stale the moment the slot is scrubbed for a new occupant.
/// Same-tid *scans* (dequeue, purge, self-locate) still key on `handle.tid()` — they
/// identify queue entries, never act on the thread through the bare index.
type Waiter = (WakeHandle, u32);
use akuma_exec::threading::{WakeHandle, current_wake_handle, wake_by_handle};

/// Futex waiter table.
///
/// Key is `(tgid, uaddr)`, where `tgid` is the thread-group leader's PID (from
/// `PROCESS_INFO_ADDR`) — scoping the futex to one address space, which is what keeps
/// two processes that happen to use the same virtual address (Akuma has no ASLR) off
/// each other's queues. `tgid = 0` is the VA-only global namespace and is reserved for
/// memory genuinely shared *between* address spaces; `futex_key_tgid` documents why
/// the `FUTEX_PRIVATE` flag does not decide which of the two applies. Kernel-internal
/// wakes (clear_child_tid, robust futex) publish to both.
/// # Why every access below masks local IRQs
///
/// `FUTEX_WAITERS` is reachable from a BKL-free syscall window (Phase 7f). A nested IRQ
/// taken on a core that holds this table runs `enter_kernel()` and hard-spins for the
/// BKL; if a peer core holds the BKL and is inside `futex_do_wake` waiting on this
/// table, neither can advance — the AB-BA shape `locking.md`'s "Correctness rules
/// learned the hard way" describes, and the reason `PreemptGuard` masks IRQs. Masking
/// makes each hold un-interruptible on its own core, which is the discipline `PIPES`
/// (`syscall/pipe.rs`) and `Process::fault_mutex` (`process/children.rs`) already use.
///
/// Two of the critical sections below read the futex word from user memory *inside* the
/// hold, which the lost-wakeup argument requires (see `sys_futex`'s FUTEX_WAIT arm).
/// That is safe under masked IRQs because the word is already mapped: `sys_futex`
/// validates `uaddr` with `validate_user_ptr` (which demand-pages via
/// `ensure_user_pages_mapped`) before any lock op, and a futex word is writable
/// anonymous memory, so `reclaim_clean_file_pages` — which only evicts clean RO *file*
/// pages — can never unmap it underneath us. A fault there therefore means userspace
/// raced an `munmap` against its own `FUTEX_WAIT`, and with the lazy region gone
/// `try_resolve_el1_user_copy_lazy_fault` declines the fault, so it resolves through
/// the byte loop's fixup to `EFAULT` rather than demand-paging under the hold.
///
/// That requirement is now stated in the code rather than only here: the in-hold read
/// passes `Prefault::No`, so the copy helper range-checks (a read-only page-table walk,
/// safe under the hold) and refuses to demand-page. Changing it to `Prefault::Yes`
/// would allocate frames, take `as_lock` and possibly read a file with IRQs masked
/// and this spinlock held.
static FUTEX_WAITERS: Spinlock<BTreeMap<(u32, usize), Vec<Waiter>>> = Spinlock::new(BTreeMap::new());

const BITSET_MATCH_ANY: u32 = 0xFFFFFFFF;

// ─── Futex bookkeeping trace (`FUTEX_ORPHAN_DIAG`) ───────────────────────────
//
// `[FUTEX-DUMP]` shows which tids ARE queued. The failure we are hunting is a thread
// that is parked inside `sys_futex` and is queued NOWHERE — no wake can ever reach it,
// so it sleeps until the process is killed. Detecting that needs the complement of the
// dump (every thread whose `thread_current_syscall` is FUTEX and whose state is
// WAITING), and explaining it needs to know which of the six paths that can remove a
// tid from `FUTEX_WAITERS` ran last. Both are recorded here.
//
// `FUTEX_TID_HIST[tid]` is a 16-deep ring of 4-bit event codes, newest in the low
// nibble; `FUTEX_TID_TS[tid]` is the uptime of the newest event and `FUTEX_TID_KEY`
// the `uaddr` it concerned. A wedged thread stops emitting events, so its ring freezes
// on exactly the transition that stranded it.
const FE_ENQ: u64 = 1; // enqueued (first entry into the wait)
const FE_REENQ: u64 = 2; // re-enqueued after a spurious wake
const FE_SELFRM: u64 = 3; // removed itself at `key` in the wait loop
const FE_WOKE: u64 = 4; // popped by `futex_do_wake`
const FE_PURGE: u64 = 5; // dropped by `futex_purge_tid` (terminate / slot recycle)
const FE_RQ: u64 = 6; // moved to the requeue target by `futex_requeue_table`
const FE_DEQ: u64 = 7; // `futex_dequeue` (timeout / EFAULT cleanup)
const FE_RMANY: u64 = 8; // `futex_remove_tid_anywhere`
const FE_PARK: u64 = 9; // about to call `schedule_blocking`
const FE_UNPARK: u64 = 10; // `schedule_blocking` returned
const FE_RET: u64 = 11; // `sys_futex` returning to user

const FE_CHARS: [u8; 12] = *b"-EeSWPQDApuX";

static FUTEX_TID_HIST: [core::sync::atomic::AtomicU64;
    akuma_exec::threading::MAX_THREADS] = {
    [const { core::sync::atomic::AtomicU64::new(0) }; akuma_exec::threading::MAX_THREADS]
};
static FUTEX_TID_TS: [core::sync::atomic::AtomicU64;
    akuma_exec::threading::MAX_THREADS] = {
    [const { core::sync::atomic::AtomicU64::new(0) }; akuma_exec::threading::MAX_THREADS]
};
static FUTEX_TID_KEY: [core::sync::atomic::AtomicU64;
    akuma_exec::threading::MAX_THREADS] = {
    [const { core::sync::atomic::AtomicU64::new(0) }; akuma_exec::threading::MAX_THREADS]
};

/// Record one futex bookkeeping event for `tid`. Relaxed throughout: this is a
/// diagnostic ring read only by `futex_dump` long after the fact, never a
/// synchronisation point, and it must not add ordering to the futex fast path.
#[inline]
fn fe(tid: usize, ev: u64, uaddr: usize) {
    if !crate::config::FUTEX_ORPHAN_DIAG || tid >= akuma_exec::threading::MAX_THREADS {
        return;
    }
    use core::sync::atomic::Ordering::Relaxed;
    let h = FUTEX_TID_HIST[tid].load(Relaxed);
    FUTEX_TID_HIST[tid].store((h << 4) | ev, Relaxed);
    FUTEX_TID_TS[tid].store(crate::timer::uptime_us(), Relaxed);
    FUTEX_TID_KEY[tid].store(uaddr as u64, Relaxed);
}

// ─── Wake ring ───────────────────────────────────────────────────────────────
//
// The complement of the orphan check, for the *other* half of a lost wakeup: the
// waiter is queued correctly and the wake simply never reached its key. Post-mortem,
// `[FUTEX-DUMP]` cannot tell "the waker never ran" from "the waker ran and published
// to a different `(tgid, uaddr)`" — both look like a waiter sitting there forever.
//
// Every `FUTEX_WAKE`-family op appends `(tgid, uaddr, woken, waker_tid, ts)` here.
// A wedged process stops generating futex traffic the instant it wedges, so its own
// last wakes stay in the ring — but only if a *busy* process cannot evict them. One
// flat ring cannot manage that: `cargo`'s poll loop emits ~100 wakes per millisecond
// in bursts, which flushed a 128-entry global ring in 1.2 ms and left nothing from
// the wedged process. So the ring is bucketed **by tgid**: a noisy process floods
// only its own bucket, and every other process keeps its last `WAKE_PER_BUCKET`.
//
// Buckets collide (`tgid % WAKE_BUCKETS`), so each entry carries its real tgid and
// the dump prints it — a colliding neighbour is visible rather than confusing. The
// dump also scans *every* bucket for the stuck `uaddr`, which is what catches a wake
// published to the WRONG tgid (the lost-wakeup shape where waiter and waker disagree
// about the key namespace); such an entry lands in the waker's bucket, not the
// waiter's, and would be invisible to a per-tgid lookup alone.
//
// Slots are written field-by-field without a lock, so a reader racing a writer can
// see one torn entry. That is acceptable for a post-mortem dump and is why the ring
// is never used for a correctness decision.
const WAKE_BUCKETS: usize = 64;
const WAKE_PER_BUCKET: usize = 16;
const WAKE_RING: usize = WAKE_BUCKETS * WAKE_PER_BUCKET;

static WAKE_RING_IDX: [core::sync::atomic::AtomicUsize; WAKE_BUCKETS] = {
    [const { core::sync::atomic::AtomicUsize::new(0) }; WAKE_BUCKETS]
};
static WAKE_RING_KEY: [core::sync::atomic::AtomicU64; WAKE_RING] = {
    [const { core::sync::atomic::AtomicU64::new(0) }; WAKE_RING]
};
/// Packed `tgid << 32 | woken << 16 | waker_tid`.
static WAKE_RING_META: [core::sync::atomic::AtomicU64; WAKE_RING] = {
    [const { core::sync::atomic::AtomicU64::new(0) }; WAKE_RING]
};
static WAKE_RING_TS: [core::sync::atomic::AtomicU64; WAKE_RING] = {
    [const { core::sync::atomic::AtomicU64::new(0) }; WAKE_RING]
};

fn wake_ring_record(tgid: u32, uaddr: usize, woken: u64) {
    if !crate::config::FUTEX_ORPHAN_DIAG {
        return;
    }
    use core::sync::atomic::Ordering::Relaxed;
    let b = tgid as usize % WAKE_BUCKETS;
    let i = b * WAKE_PER_BUCKET + WAKE_RING_IDX[b].fetch_add(1, Relaxed) % WAKE_PER_BUCKET;
    let tid = akuma_exec::threading::current_thread_id() as u64;
    WAKE_RING_KEY[i].store(uaddr as u64, Relaxed);
    WAKE_RING_META[i].store((u64::from(tgid) << 32) | ((woken & 0xFFFF) << 16) | (tid & 0xFFFF), Relaxed);
    WAKE_RING_TS[i].store(crate::timer::uptime_us(), Relaxed);
}

fn wake_ring_print(i: usize, tag: &str) {
    use core::sync::atomic::Ordering::Relaxed;
    let ts = WAKE_RING_TS[i].load(Relaxed);
    if ts == 0 {
        return;
    }
    let meta = WAKE_RING_META[i].load(Relaxed);
    tprint!(128, "  {} tgid={} uaddr={:#x} woken={} by_tid={} ts={}us\n",
        tag, meta >> 32, WAKE_RING_KEY[i].load(Relaxed), (meta >> 16) & 0xFFFF, meta & 0xFFFF, ts);
}

/// Report the wakes relevant to one stuck waiter: every recorded wake on its `uaddr`
/// under ANY tgid (`same-addr`), then its own tgid's bucket (`bucket`) for context.
/// Called from `futex_dump` only when something is actually stuck, so it costs
/// nothing on a healthy system.
fn wake_ring_dump_for(tgid: u32, uaddr: usize) {
    tprint!(96, "[FUTEX-WAKERING] for stuck waiter tgid={} uaddr={:#x}\n", tgid, uaddr);
    use core::sync::atomic::Ordering::Relaxed;
    for (i, slot) in WAKE_RING_KEY.iter().enumerate() {
        if slot.load(Relaxed) == uaddr as u64 {
            wake_ring_print(i, "same-addr");
        }
    }
    let b = tgid as usize % WAKE_BUCKETS;
    let head = WAKE_RING_IDX[b].load(Relaxed);
    for n in 0..WAKE_PER_BUCKET {
        wake_ring_print(b * WAKE_PER_BUCKET + (head + n) % WAKE_PER_BUCKET, "bucket");
    }
}

/// Returns the TGID to use as the futex key namespace.
///
/// The thread group's leader PID (shared among CLONE_VM threads via
/// `PROCESS_INFO_ADDR`) for anything living in this address space alone; `0` — the
/// VA-only global namespace — only for memory genuinely shared between address
/// spaces.
///
/// # Why `is_private` alone does not decide this
///
/// Linux keys a futex by the *address space* whenever the page is anonymous,
/// **whether or not `FUTEX_PRIVATE` was passed**: `get_futex_key` only reaches the
/// `(inode, index)` form for a page with a `page->mapping`, and falls back to
/// `(mm, address)` otherwise. Keying every non-private op to `(0, uaddr)` therefore
/// diverges from Linux — and because Akuma has no ASLR, `(0, uaddr)` is the *same*
/// key in every process running the same binary.
///
/// That is not a corner case, it is musl's thread-list lock. `__tl_lock` /
/// `__tl_unlock` wait and wake on `&__thread_list_lock` — a `libc.bss` global at a
/// fixed VA — with `priv = 0`, and `pthread_create` hands the kernel that same
/// address as the `CLONE_CHILD_CLEARTID` word, so *every thread create and exit in
/// every musl process* used one global queue. `FUTEX_WAKE(&__thread_list_lock, 1)`
/// in process A pops the FIFO head, which is often a thread of process B: B is woken
/// spuriously, the wake is counted as delivered, and A's own waiter stays parked
/// forever. It needs several multi-threaded processes running at once to show up,
/// which is exactly the `-j4` rustc self-host build and not the single-process
/// futex probes (`futexops`, `futextest`) that passed throughout.
fn futex_key_tgid(is_private: bool, uaddr: usize) -> u32 {
    let own_tgid = current_tgid_for_futex_key();
    if !is_private
        && own_tgid != 0
        && crate::syscall::mem::is_shared_file_mapping(own_tgid, uaddr)
    {
        // Genuinely cross-address-space memory: the global namespace is the point.
        // (Still VA-keyed, so it only pairs processes that mapped it at the same
        // address — the pre-existing limit of Akuma's shared-futex support, now the
        // ONLY thing that can land in this namespace.)
        return 0;
    }
    own_tgid
}

/// The address-space-scoped half of [`futex_key_tgid`], split out because the
/// degradation warning below is about *identity resolution* and applies equally to
/// private and non-private ops.
fn current_tgid_for_futex_key() -> u32 {
    {
        if let Some(tgid) = akuma_exec::process::read_current_pid() {
            tgid
        } else {
            // A futex whose tgid cannot be resolved does NOT belong in the shared
            // namespace — that is a correctness event, not a graceful fallback, and it is
            // logged rather than silently absorbed.
            //
            // Key `(0, uaddr)` is keyed by virtual address ALONE. Akuma has no ASLR, so
            // every process running the same binary parks on the same addresses; N copies
            // of one program collapse into one queue. `FUTEX_WAKE(uaddr, 1)` from one
            // process then pops a *different* process's waiter, counts it as woken, and
            // leaves the real waiter parked forever — the cross-process lost wakeup
            // described on `futex_key_tgid`.
            //
            // Measured at ZERO occurrences across boot, 8-way and 16-way thread-churn runs
            // (2026-08-04): with `VFORK_FASTPATH_ENABLED`, `read_current_pid` resolves via
            // `THREAD_PID_MAP` and returns before this branch is reachable. Kept as a
            // tripwire, not a known-hot path — and rate-limited in case that ever changes.
            let n = FUTEX_KEY_DEGRADED_TO_SHARED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
            if n <= 10 || n.is_multiple_of(1000) {
                tprint!(192,
                    "[futex] WARNING: futex key degraded to tgid=0 (read_current_pid \
                     returned None) — shares the VA-only namespace with every other process; \
                     occurrence {}\n",
                    n);
            }
            0
        }
    }
}

/// Count of `futex_key_tgid` degradations to the shared namespace. Drives the rate limit
/// above; a non-zero value is itself the bug signature.
static FUTEX_KEY_DEGRADED_TO_SHARED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Pop up to `max_wake` waiters from the `(tgid, uaddr)` bucket whose stored bitset
/// intersects `wake_mask` (`BITSET_MATCH_ANY` for plain `FUTEX_WAKE`/kernel-internal
/// wakes; `val3` for `FUTEX_WAKE_BITSET`), fire their wakers, and return how many were
/// woken.
///
/// The wakes deliberately run *outside* the hold: `Waker::wake` touches the scheduler,
/// which must not be entered with the futex table held.
pub fn futex_do_wake(tgid: u32, uaddr: usize, max_wake: u32, wake_mask: u32) -> u64 {
    let key = (tgid, uaddr);

    let to_wake: Vec<WakeHandle> = crate::irq::with_irqs_disabled(|| {
        let mut waiters = FUTEX_WAITERS.lock();
        let Some(queue) = waiters.get_mut(&key) else { return Vec::new() };
        let mut to_wake: Vec<WakeHandle> = Vec::new();
        let mut i = 0;
        while i < queue.len() && (to_wake.len() as u32) < max_wake {
            if queue[i].1 & wake_mask != 0 {
                // Matching waiter: drain it (FIFO) for waking.
                let (handle, _) = queue.remove(i);
                to_wake.push(handle);
            } else {
                // Non-matching bitset: leave queued, keep scanning.
                i += 1;
            }
        }
        if queue.is_empty() {
            waiters.remove(&key);
        }
        to_wake
    });

    // Outside the hold — the generation in each handle is what makes that safe: if this
    // path is preempted here and a popped waiter's slot is recycled meanwhile, the wake
    // is refused instead of landing on the slot's new occupant.
    for h in &to_wake {
        fe(h.tid(), FE_WOKE, uaddr);
        wake_by_handle(*h);
    }
    wake_ring_record(tgid, uaddr, to_wake.len() as u64);
    to_wake.len() as u64
}

/// Kernel-internal futex wake (clear_child_tid, robust futex).
/// Wakes both tgid=0 (shared futex waiters) and tgid=tgid (FUTEX_PRIVATE waiters such
/// as pthread_join), since we cannot know which variant the waiter used.
pub fn futex_wake(tgid: u32, uaddr: usize, max_wake: i32) {
    let n0 = futex_do_wake(0, uaddr, max_wake as u32, BITSET_MATCH_ANY);
    let n1 = if tgid != 0 {
        futex_do_wake(tgid, uaddr, max_wake as u32, BITSET_MATCH_ANY)
    } else {
        0
    };
    if crate::config::FUTEX_DBG_ENABLED {
        tprint!(128, "[clear_child_tid] tgid={} addr={:#x} woke shared={} private={}\n", tgid, uaddr, n0, n1);
    }
}

/// Test helper: insert the current thread into the futex waiter table at an
/// explicit (tgid, uaddr) key and block until woken.
///
/// `FUTEX_WAIT_PRIVATE` in the test environment always resolves to tgid=0
/// (because `read_current_pid()` returns None with no user address space).
/// This helper lets tests place a waiter at a non-zero tgid so we can
/// verify that `futex_wake(tgid, ...)` correctly reaches private-futex
/// queues (the fix for the `clear_child_tid` / `pthread_join` hang).
#[allow(dead_code)]
pub fn futex_wait_at_tgid_for_test(tgid: u32, uaddr: usize) {
    let key = (tgid, uaddr);
    crate::irq::with_irqs_disabled(|| {
        let mut waiters = FUTEX_WAITERS.lock();
        waiters.entry(key).or_default().push((current_wake_handle(), BITSET_MATCH_ANY));
    });
    akuma_exec::threading::schedule_blocking(u64::MAX);
    // futex_do_wake removed us from the queue before calling wake()
}

/// Atomically re-check the futex word and enqueue `tid` as a waiter on `key`,
/// storing `bitset` for later `FUTEX_WAKE_BITSET` selectivity.
///
/// The user read happens INSIDE the hold on purpose — that is what makes it atomic with
/// respect to `futex_do_wake`. A concurrent wake either runs before we take the table
/// (and changes the futex value, so we observe the new value and report `EAGAIN`) or
/// after we insert our tid (so it finds us and wakes us). Splitting the read out would
/// reopen the lost-wakeup window. See the `FUTEX_WAITERS` doc comment for why doing the
/// read under masked IRQs cannot demand-page.
///
/// `Err` carries the errno the caller must return; `Ok` means we are enqueued.
fn futex_check_and_enqueue(
    key: (u32, usize),
    tid: usize,
    bitset: u32,
    uaddr: usize,
    val: u32,
    first: bool,
) -> Result<(), u64> {
    let r = crate::irq::with_irqs_disabled(|| {
        let mut waiters = FUTEX_WAITERS.lock();
        let mut current_val: u32 = 0;
        if read_user_into_with(&mut current_val, uaddr as u64, Prefault::No).is_err() {
            return Err(EFAULT);
        }
        if current_val != val {
            return Err(EAGAIN);
        }
        // `tid` is the enqueuing thread itself, so its handle is live by definition.
        waiters.entry(key).or_default().push((akuma_exec::threading::wake_handle_for_thread(tid), bitset));
        Ok(())
    });
    if r.is_ok() {
        fe(tid, if first { FE_ENQ } else { FE_REENQ }, uaddr);
    }
    r
}

/// Move waiters off `key1`: take up to `max_wake` of them for the caller to wake, and
/// requeue up to `max_requeue` of the rest onto `key2` (skipped when `key2`'s uaddr is
/// 0). Shared verbatim by FUTEX_REQUEUE and FUTEX_CMP_REQUEUE, which differ only in the
/// value pre-check they do before calling.
///
/// Returns the tids to wake and how many were requeued. The wakes are deliberately left
/// to the caller so they happen outside the hold. The requeued waiters keep their
/// stored bitset (requeue moves waiters unconditionally regardless of bitset, matching
/// Linux).
fn futex_requeue_table(
    key1: (u32, usize),
    key2: (u32, usize),
    max_wake: u32,
    max_requeue: u32,
) -> (Vec<WakeHandle>, usize) {
    let has_requeue_target = key2.1 != 0;
    crate::irq::with_irqs_disabled(|| {
        let mut waiters = FUTEX_WAITERS.lock();

        let (to_wake, to_requeue) = if let Some(queue) = waiters.remove(&key1) {
            let wake_count = (max_wake as usize).min(queue.len());
            let mut remaining: Vec<Waiter> = queue;
            let to_wake: Vec<WakeHandle> = remaining.drain(..wake_count).map(|(h, _)| h).collect();

            let requeue_count = if has_requeue_target {
                (max_requeue as usize).min(remaining.len())
            } else {
                0
            };
            let to_requeue: Vec<Waiter> = remaining.drain(..requeue_count).collect();

            // Put back any remaining waiters
            if !remaining.is_empty() {
                waiters.insert(key1, remaining);
            }

            (to_wake, to_requeue)
        } else {
            (Vec::new(), Vec::new())
        };

        if !to_requeue.is_empty() && has_requeue_target {
            waiters.entry(key2).or_default().extend(to_requeue.iter().copied());
        }
        for (h, _) in &to_requeue {
            fe(h.tid(), FE_RQ, key2.1);
        }

        (to_wake, to_requeue.len())
    })
}

/// Remove `tid` from `key`'s waiter queue, dropping the queue if it empties.
fn futex_dequeue(key: (u32, usize), tid: usize) {
    crate::irq::with_irqs_disabled(|| {
        let mut waiters = FUTEX_WAITERS.lock();
        if let Some(queue) = waiters.get_mut(&key) {
            queue.retain(|(h, _)| h.tid() != tid);
            if queue.is_empty() { waiters.remove(&key); }
        }
    });
    fe(tid, FE_DEQ, key.1);
}

/// Remove `tid` from *whichever* queue under `tgid` currently holds it, or do nothing
/// if it is not queued.
///
/// This is the cleanup path for a waiter that may have been `FUTEX_REQUEUE`d off its
/// original `key` onto the requeue target. The waiting thread's loop only ever computes
/// its original `key` locally, so after a requeue it cannot dequeue from the right place
/// on its own — without this helper, a requeued waiter that left via timeout/EINTR would
/// strand its tid on the requeue target forever, and every such dead tid would silently
/// absorb one future `FUTEX_WAKE` on that address (the lost-wakeup generator behind the
/// `typenum` stall in `archive/SELFHOST_DEVBOX_SMOLTCP.md`). Requeue never
/// crosses `tgid`, so the search is bounded to this thread group's queues.
fn futex_remove_tid_anywhere(tgid: u32, tid: usize) {
    crate::irq::with_irqs_disabled(|| {
        let mut waiters = FUTEX_WAITERS.lock();
        let mut found_key: Option<(u32, usize)> = None;
        for (&k, q) in waiters.iter() {
            if k.0 != tgid { continue; }
            if q.iter().any(|(h, _)| h.tid() == tid) {
                found_key = Some(k);
                break;
            }
        }
        if let Some(k) = found_key
            && let Some(q) = waiters.get_mut(&k)
        {
            q.retain(|(h, _)| h.tid() != tid);
            if q.is_empty() { waiters.remove(&k); }
            fe(tid, FE_RMANY, k.1);
        }
    });
}

/// Dump the whole futex waiter table: every `(tgid, uaddr)` key and the tids queued on it.
///
/// The decisive diagnostic for the lost-wakeup jam
/// (docs/archive/SELFHOST_DEVBOX_SMOLTCP.md "Open issue #2"). `[THR-DUMP]` shows a thread
/// parked with `tsc=98` and its `uaddr` in `a0`, but not whether the kernel still has it
/// ENQUEUED, which is what separates the two possible bugs:
///
/// - waiter IS queued at `(tgid, uaddr)` => no wake was ever delivered to that key; the
///   waker either never ran or computed a different key.
/// - waiter is NOT queued anywhere => it was dequeued by a wake that then failed to make
///   it runnable, i.e. the defect is in the wake/scheduler handoff, not the queueing.
///
/// Printed next to `[THR-DUMP]`/`[PIPE-DUMP]` under `DEADLOCK_THREAD_DUMP_ENABLED`.
pub fn futex_dump() {
    let now = crate::timer::uptime_us();
    let stuck: Vec<(u32, usize)> = crate::irq::with_irqs_disabled(|| {
        let waiters = FUTEX_WAITERS.lock();
        if waiters.is_empty() {
            tprint!(48, "[FUTEX-DUMP] table empty\n");
            return Vec::new();
        }
        tprint!(48, "[FUTEX-DUMP] {} keys\n", waiters.len());
        let mut stuck = Vec::new();
        for (&(tgid, uaddr), q) in waiters.iter() {
            tprint!(120, "  key tgid={} uaddr={:#x} waiters={}\n", tgid, uaddr, q.len());
            for (handle, bitset) in q.iter().take(8) {
                let tid = handle.tid();
                // Age comes from the per-tid event ring (last transition = its enqueue),
                // so a long-queued waiter is visible without widening `Waiter` itself.
                let age = if crate::config::FUTEX_ORPHAN_DIAG {
                    now.saturating_sub(FUTEX_TID_TS[tid].load(core::sync::atomic::Ordering::Relaxed))
                } else {
                    0
                };
                if age > STUCK_WAITER_US {
                    stuck.push((tgid, uaddr));
                }
                let hist = fmt_hist(tid);
                tprint!(128, "    tid={} bitset={:#x} queued_for={}us hist={}\n",
                    tid, bitset, age, core::str::from_utf8(&hist).unwrap_or("?"));
            }
        }
        stuck
    });
    let orphans = futex_orphan_check();
    // Only when something is actually wedged, and only a few times per boot: each
    // report is tens of lines of serial output, which is itself enough to perturb a
    // timing bug.
    if crate::config::FUTEX_ORPHAN_DIAG
        && !(stuck.is_empty() && orphans.is_empty())
        && WAKE_RING_DUMPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 3
    {
        for (tgid, uaddr) in stuck.iter().chain(orphans.iter()) {
            wake_ring_dump_for(*tgid, *uaddr);
        }
    }
}

/// A waiter queued this long is wedged, not merely slow: the longest legitimate
/// untimed park in these workloads (a jobserver token, a codegen unit handoff) is
/// orders of magnitude shorter, and every timed wait carries its own deadline.
const STUCK_WAITER_US: u64 = 60_000_000;

static WAKE_RING_DUMPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Report every thread parked inside `sys_futex` that is queued on no key.
///
/// The complement of `futex_dump`, and the only one of the two that can *falsify*
/// anything: "parked in FUTEX_WAIT ⇒ queued in `FUTEX_WAITERS`" is an invariant of
/// the wait loop, so a violation is a kernel bug by construction — no wake can ever
/// reach that thread again. An ordinary userspace deadlock, by contrast, leaves every
/// parked thread correctly queued.
///
/// Render `tid`'s 16-deep event ring as text, oldest first, so it reads left-to-right
/// in time. See [`futex_orphan_check`] for the legend.
fn fmt_hist(tid: usize) -> [u8; 16] {
    let mut buf = [b'-'; 16];
    if !crate::config::FUTEX_ORPHAN_DIAG || tid >= akuma_exec::threading::MAX_THREADS {
        return buf;
    }
    let hist = FUTEX_TID_HIST[tid].load(core::sync::atomic::Ordering::Relaxed);
    for (i, slot) in buf.iter_mut().enumerate() {
        let ev = ((hist >> (4 * (15 - i))) & 0xF) as usize;
        *slot = FE_CHARS[ev.min(FE_CHARS.len() - 1)];
    }
    buf
}

/// Each orphan is printed with its 16-deep event ring (oldest first, so it reads
/// left-to-right in time) naming the path that removed it:
/// `E`=enqueue `e`=re-enqueue `S`=self-removed `W`=woken-by-wake `P`=purged
/// `Q`=requeued `D`=dequeue `A`=remove-anywhere `p`=park `u`=unpark `X`=return.
///
/// Returns each orphan's `(tgid, uaddr)` so the caller can pull the matching wakes.
fn futex_orphan_check() -> Vec<(u32, usize)> {
    let mut found = Vec::new();
    if !crate::config::FUTEX_ORPHAN_DIAG {
        return found;
    }
    use akuma_exec::threading::{MAX_THREADS, thread_state};
    let queued: Vec<usize> = crate::irq::with_irqs_disabled(|| {
        FUTEX_WAITERS.lock().values().flatten().map(|(h, _)| h.tid()).collect()
    });
    for tid in 0..MAX_THREADS {
        if akuma_exec::threading::get_thread_state(tid) != thread_state::WAITING {
            continue;
        }
        // 98 == FUTEX (see the syscall table in `src/syscall/mod.rs`).
        if akuma_exec::threading::thread_current_syscall(tid) != 98 {
            continue;
        }
        if queued.contains(&tid) {
            continue;
        }
        use core::sync::atomic::Ordering::Relaxed;
        let uaddr = FUTEX_TID_KEY[tid].load(Relaxed) as usize;
        // The orphan's own key namespace, resolved the same way `futex_key_tgid` does
        // for the *waker* — so a mismatch between the two shows up in the wake report.
        let tgid = akuma_exec::process::find_pid_by_thread(tid)
            .and_then(|p| akuma_exec::process::lookup_process_shared(p).map(|pr| pr.tgid))
            .unwrap_or(0);
        found.push((tgid, uaddr));
        let buf = fmt_hist(tid);
        tprint!(176,
            "[FUTEX-ORPHAN] tid={} tgid={} uaddr={:#x} last_ev_ts={}us now={}us hist={}\n",
            tid,
            tgid,
            uaddr,
            FUTEX_TID_TS[tid].load(Relaxed),
            crate::timer::uptime_us(),
            core::str::from_utf8(&buf).unwrap_or("?"),
        );
    }
    found
}

/// Drop every queued reference to `tid`, across **all** keys and thread groups.
///
/// Registered as the threading subsystem's slot-purge hook and invoked when a thread slot
/// is recycled, because `futex_remove_tid_anywhere` only ever runs on the waiter's *own*
/// timeout/EINTR path. A thread that dies while parked — `exit_group` killing siblings, a
/// consumed `PENDING_KILL`, a fault-kill — never runs that path, so its tid stayed queued
/// forever. Once the slot is recycled, that stale entry names a *live, unrelated* thread:
/// `futex_do_wake` pops it, calls `wake()` on the new occupant, and counts it toward
/// `max_wake` — so a `FUTEX_WAKE(uaddr, 1)` is consumed by a thread that was never waiting
/// while the real waiter stays parked. That is the same "stale entry absorbs a wake"
/// defect the requeue fix closed for requeued waiters, left open for dead ones.
///
/// Unlike `futex_remove_tid_anywhere` this cannot bound the scan by `tgid`: the caller is
/// the slot recycler, which by then has no process context to derive one from. The map is
/// small (only addresses with live waiters) and this runs once per slot recycle, not per
/// futex op.
pub fn futex_purge_tid(tid: usize) {
    crate::irq::with_irqs_disabled(|| {
        let mut waiters = FUTEX_WAITERS.lock();
        let mut emptied: Vec<(u32, usize)> = Vec::new();
        for (&k, q) in waiters.iter_mut() {
            let before = q.len();
            q.retain(|(h, _)| h.tid() != tid);
            if q.len() != before {
                fe(tid, FE_PURGE, k.1);
                if q.is_empty() {
                    emptied.push(k);
                }
            }
        }
        for k in emptied {
            waiters.remove(&k);
        }
    });
}

/// Test hooks for the boot self-test in `src/process_tests.rs`.
///
/// The requeue table logic below was factored out of two byte-identical copies
/// (FUTEX_REQUEUE / FUTEX_CMP_REQUEUE) while masking IRQs, so it is exactly the kind of
/// change that wants a deterministic test rather than a log grep. The waiter table needs
/// no user address space, so it is fully drivable from the boot suite — unlike
/// `futex_check_and_enqueue`, whose in-hold user read has no valid `uaddr` there.
#[cfg(kernel_tests)]
pub mod test_hooks {
    use super::{FUTEX_WAITERS, futex_dequeue, futex_requeue_table};
    use alloc::vec::Vec;

    pub fn enqueue(key: (u32, usize), tid: usize) {
        crate::irq::with_irqs_disabled(|| {
            FUTEX_WAITERS.lock().entry(key).or_default().push((
                akuma_exec::threading::wake_handle_for_thread(tid),
                super::BITSET_MATCH_ANY,
            ));
        });
    }

    /// `None` when no queue exists for `key` (distinct from an empty one, which the
    /// table never stores — every removal path drops an emptied queue). The returned
    /// tids have their bitset stripped: the deterministic test checks FIFO ordering,
    /// not bitset bookkeeping.
    pub fn queue(key: (u32, usize)) -> Option<Vec<usize>> {
        crate::irq::with_irqs_disabled(|| {
            FUTEX_WAITERS
                .lock()
                .get(&key)
                .map(|q| q.iter().map(|(h, _)| h.tid()).collect())
        })
    }

    pub fn requeue(key1: (u32, usize), key2: (u32, usize), max_wake: u32, max_requeue: u32) -> (Vec<usize>, usize) {
        let (to_wake, requeued) = futex_requeue_table(key1, key2, max_wake, max_requeue);
        (to_wake.into_iter().map(akuma_exec::threading::WakeHandle::tid).collect(), requeued)
    }

    pub fn dequeue(key: (u32, usize), tid: usize) {
        futex_dequeue(key, tid);
    }

    pub fn drop_key(key: (u32, usize)) {
        crate::irq::with_irqs_disabled(|| { FUTEX_WAITERS.lock().remove(&key); });
    }
}

pub(super) fn sys_futex(uaddr: usize, op: i32, val: u32, timeout_ptr: u64, uaddr2: usize, val3: u32) -> u64 {
    const FUTEX_WAIT: i32 = 0;
    const FUTEX_WAKE: i32 = 1;
    #[allow(dead_code)]
    const FUTEX_FD: i32 = 2;  // Deprecated, returns ENOSYS
    const FUTEX_REQUEUE: i32 = 3;
    const FUTEX_CMP_REQUEUE: i32 = 4;
    const FUTEX_WAKE_OP: i32 = 5;
    const FUTEX_LOCK_PI: i32 = 6;
    const FUTEX_UNLOCK_PI: i32 = 7;
    const FUTEX_TRYLOCK_PI: i32 = 8;
    const FUTEX_WAIT_BITSET: i32 = 9;
    const FUTEX_WAKE_BITSET: i32 = 10;
    const FUTEX_WAIT_REQUEUE_PI: i32 = 11;
    const FUTEX_CMP_REQUEUE_PI: i32 = 12;
    const FUTEX_PRIVATE_FLAG: i32 = 128;
    const FUTEX_CLOCK_REALTIME: i32 = 256;

    let is_private = (op & FUTEX_PRIVATE_FLAG) != 0;
    let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);

    // Validate uaddr - must be 4-byte aligned and in user space
    if uaddr == 0 || uaddr & 3 != 0 {
        return EINVAL;
    }
    if !validate_user_ptr(uaddr as u64, 4) {
        // For WAKE operations on unmapped addresses: there can't be any
        // waiters, so return 0 (none woken).  Go's runtime calls
        // futex(0xfffffffffffffffc, FUTEX_WAKE) during exit coordination —
        // returning EFAULT breaks Go's exit path and leaves goroutine
        // threads stuck.
        if cmd == FUTEX_WAKE || cmd == FUTEX_WAKE_BITSET || cmd == FUTEX_WAKE_OP {
            return 0; // no waiters on unmapped address
        }
        if cmd == FUTEX_WAIT || cmd == FUTEX_WAIT_BITSET {
            return EAGAIN; // "value changed" — Go retries and proceeds with exit
        }
        return EFAULT;
    }

    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let tid = akuma_exec::threading::current_thread_id();
            let tgid = futex_key_tgid(is_private, uaddr);
            let key = (tgid, uaddr);

            if crate::config::FUTEX_DBG_ENABLED {
                let ts = crate::timer::uptime_us();
                tprint!(128, "[futex-dbg] WAIT tid={} tgid={} addr={:#x} val={} ts={}us\n", tid, tgid, uaddr, val, ts);
            }

            // FUTEX_WAIT_BITSET with val3==0 is invalid per spec.
            if cmd == FUTEX_WAIT_BITSET && val3 == 0 {
                return EINVAL;
            }

            // Bitset this waiter matches wakes against. Plain FUTEX_WAIT is
            // equivalent to FUTEX_BITSET_MATCH_ANY.
            let waiter_bitset: u32 = if cmd == FUTEX_WAIT_BITSET { val3 } else { BITSET_MATCH_ANY };

            if let Err(errno) = futex_check_and_enqueue(key, tid, waiter_bitset, uaddr, val, true) {
                fe(tid, FE_RET, uaddr);
                return errno;
            }

            let is_realtime = (op & FUTEX_CLOCK_REALTIME) != 0;
            let deadline = if timeout_ptr != 0 {
                // A non-null timespec MUST be readable. Linux answers an
                // unreadable pointer with EFAULT; silently downgrading it to
                // "no timeout" (the old behaviour) converted a transient fault
                // into a permanent park — exactly the lost-wakeup shape, and
                // reachable under memory pressure where `validate_user_ptr`'s
                // demand-page fails.
                if !validate_user_ptr(timeout_ptr, 16) {
                    futex_dequeue(key, tid);
                    return EFAULT;
                }
                let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
                if read_user_into(&mut ts, timeout_ptr).is_err() {
                    // Remove ourselves from the waiter queue before returning.
                    futex_dequeue(key, tid);
                    return EFAULT;
                }
                let timeout_us = (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1000;
                // Timeout interpretation per Linux semantics, NOT op-flag-agnostic:
                //   - FUTEX_WAIT (plain): timeout is RELATIVE to now.
                //   - FUTEX_WAIT_BITSET: timeout is ABSOLUTE. Default clock is
                //     CLOCK_MONOTONIC; the CLOCK_REALTIME flag selects wall-clock.
                // The wait loop below compares deadlines against `uptime_us()`, and our
                // CLOCK_MONOTONIC == uptime_us (src/syscall/time.rs), so an absolute
                // monotonic deadline is used directly.  This is exactly what Rust std
                // emits for *every* timed wait (Condvar::wait_timeout, park_timeout,
                // Mutex/Once contention): it computes `CLOCK_MONOTONIC::now() + dur`
                // and passes FUTEX_WAIT_BITSET *without* CLOCK_REALTIME.  Treating that
                // already-absolute value as relative (adding uptime again) made every
                // std timed wait sleep ~2x current-uptime — growing the longer the VM
                // runs — which manifested as the rustc "futex deadlock" (see
                // docs/AKUMA_SELF_HOSTING.md §7d).
                if cmd == FUTEX_WAIT_BITSET {
                    if is_realtime {
                        // Absolute CLOCK_REALTIME (wall-clock) deadline.  Convert into
                        // uptime terms so the wait loop's uptime comparison is correct:
                        // remaining = abs_realtime - utc_now; deadline = uptime_now + remaining.
                        match crate::timer::utc_time_us() {
                            Some(utc_now) if timeout_us > utc_now => {
                                crate::timer::uptime_us() + (timeout_us - utc_now)
                            }
                            Some(_) => crate::timer::uptime_us(), // already past → immediate timeout
                            // No wall clock available: fall back to treating the absolute
                            // value as uptime microseconds (imprecise but bounded).
                            None => timeout_us,
                        }
                    } else {
                        // Absolute CLOCK_MONOTONIC deadline == absolute uptime.
                        timeout_us
                    }
                } else {
                    // Plain FUTEX_WAIT: relative timeout.
                    crate::timer::uptime_us() + timeout_us
                }
            } else {
                u64::MAX
            };

            // Safety net for untimed waits (deadline == u64::MAX).
            //
            // The scheduler's wake/schedule handshake (`schedule_blocking` ↔
            // `ThreadWaker::wake`) has residual wake-loss windows under heavy SMP
            // preemption (docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md
            // §7.9: 4/4 untimed Barrier/Condvar hangs under CPU-hog pressure; timed
            // waits survive because their deadline forces a schedule_blocking return,
            // letting the value re-check below rescue the lost wake via EAGAIN).
            //
            // For untimed waits there is no deadline to force that return, so one lost
            // wake strands the thread forever.  We convert u64::MAX into a periodic
            // bounded "revalidation" deadline: when it expires the thread self-removes,
            // re-reads the futex word (via futex_check_and_enqueue, which returns EAGAIN
            // if the value changed — exactly the lost-wake rescue), and re-parks.  This
            // is correct per the futex contract (spurious wakes are always allowed) and
            // costs one wakeup per interval per untimed waiter.
            const FUTEX_REVALIDATE_US: u64 = 200_000; // 200 ms — safety net, well above scheduling latency

            // Main wait loop — handles spurious wakeups from schedule_blocking.
            //
            // We distinguish genuine FUTEX_WAKE from spurious by locating ourselves
            // in the table after schedule_blocking returns. Crucially, the lookup is
            // NOT just a membership check on `key`: a `FUTEX_REQUEUE` may have moved
            // us off `key` onto the requeue target's queue. The result drives cleanup:
            //   - not queued anywhere  → removed by FUTEX_WAKE → genuine wake → return 0
            //   - queued at `key`      → spurious; re-check deadline/value, re-enqueue
            //   - queued at other key  → requeued; stay parked (or leave on deadline/
            //                            signal, cleaning up the requeue target so no
            //                            dead tid is left to eat a future wake)
            loop {
                // For untimed waits, park with the revalidation deadline instead of
                // u64::MAX so the scheduler's wake-pass eventually returns us.  The
                // ETIMEDOUT checks below still compare against the original `deadline`
                // (u64::MAX), so the user-visible behaviour is unchanged.
                let park_deadline = if deadline == u64::MAX {
                    crate::timer::uptime_us().saturating_add(FUTEX_REVALIDATE_US)
                } else {
                    deadline
                };

                fe(tid, FE_PARK, uaddr);
                akuma_exec::threading::schedule_blocking(park_deadline);
                fe(tid, FE_UNPARK, uaddr);

                // Locate ourselves under this tgid (requeue never crosses tgid), and
                // — if we are still on the ORIGINAL key — drop ourselves so the
                // re-validate/re-enqueue below cannot double-enqueue. A waiter sitting
                // on the requeue target is left parked.
                let located: Option<(u32, usize)> = crate::irq::with_irqs_disabled(|| {
                    let mut waiters = FUTEX_WAITERS.lock();
                    let mut found: Option<(u32, usize)> = None;
                    for (&k, q) in waiters.iter() {
                        if k.0 == tgid && q.iter().any(|(h, _)| h.tid() == tid) {
                            found = Some(k);
                            break;
                        }
                    }
                    if let Some(k) = found
                        && k == key
                        && let Some(q) = waiters.get_mut(&k)
                    {
                        q.retain(|(h, _)| h.tid() != tid);
                        if q.is_empty() { waiters.remove(&k); }
                        fe(tid, FE_SELFRM, k.1);
                    }
                    found
                });

                // A pending signal terminates the wait regardless of where we park.
                if akuma_exec::threading::peek_pending_signal(tid) != 0 {
                    // Clean up any queue we still occupy (only the requeue-target
                    // case; the original-key case already removed itself above, and
                    // the "woken" case is already gone).
                    if located.is_some_and(|k| k != key) {
                        futex_remove_tid_anywhere(tgid, tid);
                    }
                    if crate::config::FUTEX_DBG_ENABLED {
                        tprint!(128, "[futex-dbg] WOKE tid={} addr={:#x} result=EINTR ts={}us\n", tid, uaddr, crate::timer::uptime_us());
                    }
                    fe(tid, FE_RET, uaddr);
                    return EINTR;
                }

                match located {
                    None => {
                        // Removed by FUTEX_WAKE → genuine wake.
                        if crate::config::FUTEX_DBG_ENABLED {
                            tprint!(128, "[futex-dbg] WOKE tid={} addr={:#x} result=0 ts={}us\n", tid, uaddr, crate::timer::uptime_us());
                        }
                        fe(tid, FE_RET, uaddr);
                        return 0;
                    }
                    Some(k) if k == key => {
                        // Spurious at the original key. Check terminal conditions,
                        // then re-validate the futex value and re-enqueue (classic
                        // futex contract: a changed value reports EAGAIN so the
                        // caller re-evaluates its condition variable).
                        if deadline != u64::MAX && crate::timer::uptime_us() >= deadline {
                            if crate::config::FUTEX_DBG_ENABLED {
                                tprint!(128, "[futex-dbg] WOKE tid={} addr={:#x} result=ETIMEDOUT ts={}us\n", tid, uaddr, crate::timer::uptime_us());
                            }
                            fe(tid, FE_RET, uaddr);
                            return ETIMEDOUT;
                        }
                        if let Err(errno) = futex_check_and_enqueue(key, tid, waiter_bitset, uaddr, val, false) {
                            fe(tid, FE_RET, uaddr);
                            return errno;
                        }
                    }
                    Some(_) => {
                        // Moved by FUTEX_REQUEUE onto the target queue. We are
                        // correctly parked there — a FUTEX_WAKE on that address will
                        // drain us and we'll observe it as a genuine wake next
                        // iteration. Do NOT re-validate the original futex value
                        // (its contract no longer applies to the requeue target).
                        // Only a deadline (here) or a signal (above) can release us;
                        // both must clean up the requeue target so no dead tid is
                        // left behind to eat a future wake.
                        if deadline != u64::MAX && crate::timer::uptime_us() >= deadline {
                            futex_remove_tid_anywhere(tgid, tid);
                            if crate::config::FUTEX_DBG_ENABLED {
                                tprint!(128, "[futex-dbg] WOKE tid={} addr={:#x} result=ETIMEDOUT (requeued) ts={}us\n", tid, uaddr, crate::timer::uptime_us());
                            }
                            fe(tid, FE_RET, uaddr);
                            return ETIMEDOUT;
                        }
                        // Spurious: stay parked at the requeue target and re-loop.
                    }
                }
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let tgid = futex_key_tgid(is_private, uaddr);
            // WAKE_BITSET restricts the wake to waiters whose bitset intersects val3.
            let mask = if cmd == FUTEX_WAKE_BITSET { val3 } else { BITSET_MATCH_ANY };
            let woken = futex_do_wake(tgid, uaddr, val, mask);
            if crate::config::FUTEX_DBG_ENABLED {
                // `tgid` is printed because it is the half of the key that can silently
                // differ between waker and waiter: an exiting thread whose process entry
                // is already RETIRED resolves `read_current_pid` differently from the
                // joiner still sitting in the same thread group. Without it, a
                // `woken=0` line cannot be told apart from "published to the wrong queue".
                tprint!(160, "[futex-dbg] WAKE tgid={} addr={:#x} max={} mask={:#x} woken={} ts={}us\n", tgid, uaddr, val, mask, woken, crate::timer::uptime_us());
            }
            woken
        }
        FUTEX_REQUEUE => {
            // Wake up to val waiters, requeue rest to uaddr2
            // val2 (passed as timeout_ptr) is max to requeue
            let max_requeue = timeout_ptr as u32;
            let tgid = futex_key_tgid(is_private, uaddr);
            let key1 = (tgid, uaddr);
            let key2 = (tgid, uaddr2);

            if uaddr2 != 0 && !validate_user_ptr(uaddr2 as u64, 4) {
                return EFAULT;
            }

            let (to_wake, requeued) = futex_requeue_table(key1, key2, val, max_requeue);
            let woken = to_wake.len();

            for h in &to_wake {
                wake_by_handle(*h);
            }

            if crate::config::FUTEX_DBG_ENABLED {
                tprint!(128, "[futex-dbg] REQUEUE addr={:#x} addr2={:#x} woken={} requeued={} ts={}us\n", uaddr, uaddr2, woken, requeued, crate::timer::uptime_us());
            }
            (woken + requeued) as u64
        }
        FUTEX_CMP_REQUEUE => {
            // Like FUTEX_REQUEUE but also checks val3 against uaddr value
            let max_requeue = timeout_ptr as u32;
            let tgid = futex_key_tgid(is_private, uaddr);
            let key1 = (tgid, uaddr);
            let key2 = (tgid, uaddr2);

            // Check current value matches expected
            let mut current_val: u32 = 0;
            if read_user_into(&mut current_val, uaddr as u64).is_err() {
                return EFAULT;
            }
            if current_val != val3 {
                return EAGAIN;
            }

            if uaddr2 != 0 && !validate_user_ptr(uaddr2 as u64, 4) {
                return EFAULT;
            }

            let (to_wake, requeued) = futex_requeue_table(key1, key2, val, max_requeue);
            let woken = to_wake.len();

            for h in &to_wake {
                wake_by_handle(*h);
            }

            (woken + requeued) as u64
        }
        FUTEX_WAKE_OP => {
            // val2 (uaddr2 wake count) rides in the timeout argument slot.
            let val2 = timeout_ptr as u32;
            let tgid = futex_key_tgid(is_private, uaddr);

            if uaddr2 == 0 || uaddr2 & 3 != 0 || !validate_user_ptr(uaddr2 as u64, 4) {
                return EFAULT;
            }

            // Decode val3: { shift[31], op[30:28], cmp[27:24], oparg[23:12], cmparg[11:0] }
            // (matches Linux's `futex_atomic_op_inuser` extraction).
            let encoded = val3;
            let op = (encoded >> 28) & 0x7;
            let cmp = (encoded >> 24) & 0xf;
            let mut oparg = (encoded << 8) >> 20;
            let cmparg = (encoded << 20) >> 20;
            if (encoded & (8u32 << 28)) != 0 {
                // FUTEX_OP_OPARG_SHIFT: oparg becomes 1 << oparg.
                oparg = 1u32 << oparg;
            }

            // Read-modify-write *uaddr2. Linux performs this atomically against peer
            // cores' userspace atomic ops; we run single-threaded w.r.t. this task's
            // own syscall entry and the page is validated above, so a plain RMW is
            // sufficient here (and is what the WAKE_OP probes exercise).
            let mut oldval: u32 = 0;
            if read_user_into(&mut oldval, uaddr2 as u64).is_err() {
                return EFAULT;
            }
            let newval: u32 = match op {
                0 => oparg,                          // FUTEX_OP_SET
                1 => oldval.wrapping_add(oparg),     // FUTEX_OP_ADD
                2 => oldval | oparg,                 // FUTEX_OP_OR
                3 => oldval & !oparg,                // FUTEX_OP_ANDN
                4 => oldval ^ oparg,                 // FUTEX_OP_XOR
                _ => return ENOSYS,
            };
            if write_user_val(uaddr2 as u64, &newval).is_err() {
                return EFAULT;
            }

            // Wake up to `val` waiters on uaddr.
            let woken1 = futex_do_wake(tgid, uaddr, val, BITSET_MATCH_ANY);

            // Conditional second wake: if (oldval CMP cmparg), wake up to `val2` on
            // uaddr2. The comparison is signed, as in Linux.
            let cmp_ok = match cmp {
                0 => oldval == cmparg,                                   // EQ
                1 => oldval != cmparg,                                   // NE
                2 => (oldval as i32) < (cmparg as i32),                  // LT
                3 => (oldval as i32) <= (cmparg as i32),                 // LE
                4 => (oldval as i32) > (cmparg as i32),                  // GT
                5 => (oldval as i32) >= (cmparg as i32),                 // GE
                _ => false,
            };
            let woken2 = if cmp_ok { futex_do_wake(tgid, uaddr2, val2, BITSET_MATCH_ANY) } else { 0 };

            if crate::config::FUTEX_DBG_ENABLED {
                tprint!(128, "[futex-dbg] WAKE_OP addr={:#x} addr2={:#x} old={} new={} woken={}+{} ts={}us\n",
                    uaddr, uaddr2, oldval, newval, woken1, woken2, crate::timer::uptime_us());
            }
            woken1 + woken2
        }
        FUTEX_LOCK_PI | FUTEX_UNLOCK_PI | FUTEX_TRYLOCK_PI => ENOSYS,
        FUTEX_WAIT_REQUEUE_PI | FUTEX_CMP_REQUEUE_PI => ENOSYS,
        _ => {
            crate::tprint!(96, "[futex] unsupported op={} (cmd={})\n", op, cmd);
            // §7k investigation: a corrupt futex op (e.g. -1) reaching here means the
            // op register (x1) held garbage at the `svc`. Dump the user instruction
            // stream at the syscall so a recurrence tells us WHICH mechanism:
            //   - `svc #0` at [elr-4] AND a sane `mov w1,#<op>` just before, yet op is
            //     garbage  → the register was corrupted after it was set (preemption /
            //     context-switch save-restore bug);
            //   - garbage/wrong instruction at [elr-4]/[elr-8] → stale I-cache mis-decode.
            // ELR (trap frame) points just past the `svc`. Cheap; only the rare
            // corruption path hits it.
            if let Some(elr) = akuma_exec::threading::current_trap_frame_elr() {
                let mut buf = [0u8; 12];
                let read_ok = copy_from_user(&mut buf, elr.wrapping_sub(8)).is_ok();
                if read_ok {
                    let pre = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]); // elr-8
                    let svc = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]); // elr-4
                    let nxt = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]); // elr
                    let tid = akuma_exec::threading::current_thread_id();
                    crate::safe_print!(
                        224,
                        "[futex-diag] tid={} elr={:#x} op={:#x} uaddr={:#x} val={} val3={} insn[-8]={:#010x} insn[-4]={:#010x}({}) insn[0]={:#010x}\n",
                        tid, elr, op as u32, uaddr, val, val3, pre, svc,
                        if svc == 0xd400_0001 { "svc#0" } else { "NOT-SVC" }, nxt,
                    );
                } else {
                    crate::safe_print!(96, "[futex-diag] elr={:#x} user read failed\n", elr);
                }
            }
            ENOSYS
        }
    }
}
