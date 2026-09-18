//! `futex(2)` — the amd64 half.
//!
//! The decisions are not here. `akuma-syscalls-sync` owns the op decode, the
//! `(tgid, uaddr)` key namespace, the waiter table and the deadline algebra,
//! and it is host-tested; per that crate's own header, every futex bug in this
//! tree's history was a property of one of those four things. This module is
//! the effects: reading and writing user memory, holding the table, and
//! parking.
//!
//! # The wait is a park, and membership is still the signal
//!
//! It was a poll until 2026-09-07 — the scheduler had no blocked state, so
//! `FUTEX_WAKE` did exactly one thing (take the waiter off the table) and the
//! waiter noticed on its next round of the round-robin. That design was right
//! about the *signal* and wrong about the *cost*: an untimed `FUTEX_WAIT` with
//! nothing else runnable span at scheduler frequency, which on a machine
//! expected to run a parallel build is most of a core per blocked thread.
//!
//! **The signal has not changed.** "Am I still queued?" is still the whole test,
//! and it is still durable state rather than an edge, so a wake cannot be lost
//! by being delivered early. What is added is that the waiter now *parks*
//! between tests and a waker now also calls `sched::wake` — belt and braces, in
//! that order: the table decides, the scheduler is merely told to look again.
//! Get the `sched::wake` wrong and the waiter is late (the scheduler's backstop
//! releases it); get the table wrong and it is incorrect. Only one of those is
//! a real bug, and it is the one the crate's host tests cover.
//!
//! The park is armed with `sched::prepare_block` **before** the membership test,
//! which is what closes the window between them: a `FUTEX_WAKE` that dequeues
//! this waiter and fires in that gap leaves `wake_pending` set, and the park
//! returns immediately instead of sleeping through it.
//!
//! # Allocation
//!
//! The wait loop allocates **nothing**. Membership is checked through
//! [`WaiterTable::iter`], which borrows; the obvious spellings
//! (`queue()`, `locate_and_take()`) each allocate or churn a `BTreeMap` entry
//! per test, which for a loop that once ran at scheduler frequency was the
//! difference between free and not. `enqueue` allocates once per wait, and
//! `wake` returns a `Vec` sized by how many waiters it actually took.

use akuma_syscalls_linux::flags::futex as f;
use akuma_syscalls_sync::deadline;
use akuma_syscalls_sync::key::{self, Namespace};
use akuma_syscalls_sync::op::{self, Action};
use akuma_syscalls_sync::table::{Key, MATCH_ANY, WaiterId, WaiterTable};

use core::sync::atomic::{AtomicU64, Ordering};

use crate::fd::errno;
use crate::serial;

/// A queued waiter, identified by its scheduler task slot.
///
/// The slot, not a pid: a thread group's threads share a pid and each has its
/// own task, and it is the *task* a wake has to reach. Slots are recycled
/// ([`crate::sched`] reuses `Finished` ones), which is the hazard the crate's
/// `purge` hook exists for — [`purge_task`] is called from thread teardown so a
/// recycled slot can never inherit a dead thread's queue entry and absorb a
/// wake meant for someone else.
///
/// The second field is the uptime the entry was made, carried for diagnostics
/// only ([`dump_waiters`]) and deliberately not part of any decision. It is
/// what separates "five threads stalled at the same instant" — one lost wake —
/// from "they piled up over a minute", which is an ordinary lock convoy, and
/// the two are indistinguishable from the table's contents alone.
#[derive(Clone, Copy)]
struct Waiter(usize, u64);

impl WaiterId for Waiter {
    fn tid(self) -> usize {
        self.0
    }
}

/// The waiter table.
///
/// `static mut` behind a raw pointer, reached only from syscall context under
/// the BKL — the discipline `usermode::PROCS` had before 5b slice 4 deleted it,
/// and for the same reason: every writer is kernel code, kernel code holds the
/// lock, and a context switch keeps it held.
static mut WAITERS: WaiterTable<Waiter> = WaiterTable::new();

fn waiters() -> *mut WaiterTable<Waiter> {
    &raw mut WAITERS
}

/// Has the `(0, uaddr)` fallback ever been taken? A tripwire, printed once.
static DEGRADED_SEEN: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// The namespace half of this caller's futex keys.
///
/// `shared_file_mapping` is unconditionally `false`: `mm.rs` refuses
/// file-backed and `MAP_SHARED` mappings outright on this target, so there is
/// no memory here that two address spaces can both reach. When that changes,
/// this is the line that has to change with it.
///
/// `is_private` is passed through honestly rather than pinned to `true`, even
/// though with no shared mapping the answer is `AddressSpace(tgid)` either way.
/// It is not decoration: Linux keys an *anonymous* page by address space
/// whether or not `FUTEX_PRIVATE_FLAG` was set, and a real trace shows why that
/// matters here — musl's `pthread_join` waits on the `CLONE_CHILD_CLEARTID`
/// word with `priv = 0`. Keying that to `(0, uaddr)` would put every process
/// running one binary on one queue, which is the `__tl_lock` bug in
/// `akuma_syscalls_sync::key`.
fn namespace(is_private: bool) -> u32 {
    let pid = crate::usermode::current_pid();
    let ns = key::namespace(is_private, Some(pid), false);
    if matches!(ns, Namespace::Degraded)
        && !DEGRADED_SEEN.swap(true, core::sync::atomic::Ordering::Relaxed)
    {
        // Not a branch, a correctness event: `(0, uaddr)` is keyed by virtual
        // address alone and this target has no ASLR, so every process running
        // one binary would share a queue. See `akuma_syscalls_sync::key`.
        serial::puts("  [futex] DEGRADED namespace — pid unresolved, keys are VA-only\n");
    }
    ns.tgid()
}

/// `futex(uaddr, op, val, timeout, uaddr2, val3)` — x86_64 202.
///
/// The sixth argument is why `syscall_entry` passes `a6`: until this syscall
/// existed nothing needed one, and `FUTEX_WAIT_BITSET`'s `val3` is not
/// optional — Rust's `std` emits it for *every* timed wait.
pub fn sys_futex(uaddr: u64, op: u64, val: u64, timeout: u64, uaddr2: u64, val3: u64) -> u64 {
    let opi = (op as u32).cast_signed();
    let private = op & (f::FUTEX_PRIVATE_FLAG as u64) != 0;
    let val3 = val3 as u32;
    let val = val as u32;

    // `uaddr_mapped` is a *probe*, and the crate is explicit that it must be
    // one the kernel performs: reading user memory can demand-page. A
    // successful 4-byte read is exactly `validate_user_ptr(uaddr, 4)`.
    let mapped = crate::uaccess::read_val::<u32>(uaddr).is_some();

    match op::decode(opi, val3, uaddr as usize, mapped) {
        Action::Return(v) => v,
        Action::Wait { bitset } => {
            let deadline = if timeout == 0 {
                deadline::NEVER
            } else {
                let Some([sec, nsec]) = crate::uaccess::read_val::<[i64; 2]>(timeout) else {
                    return errno::EFAULT;
                };
                let us = (sec.max(0).cast_unsigned())
                    .saturating_mul(1_000_000)
                    .saturating_add(nsec.max(0).cast_unsigned() / 1000);
                deadline::deadline_us(opi, us, crate::net::uptime_us(), crate::clock::utc_seconds().map(|s| s.saturating_mul(1_000_000)))
            };
            wait(uaddr, val, bitset, deadline, private)
        }
        Action::Wake { mask } => wake(uaddr, val, mask, private),
        Action::Requeue { compare } => requeue(uaddr, uaddr2, val, timeout, compare, private),
        Action::WakeOp => wake_op(uaddr, uaddr2, val, timeout, val3, private),
    }
}

fn wait(uaddr: u64, val: u32, bitset: u32, deadline_at: u64, private: bool) -> u64 {
    let tgid = namespace(private);
    let me = crate::sched::current_task();
    let key: Key = (tgid, uaddr as usize);

    // Re-read the word and enqueue with nothing in between. That ordering is
    // the whole lost-wakeup argument: a `FUTEX_WAKE` that runs between the
    // comparison and the enqueue would find no waiter and this thread would
    // park on a value that has already changed. Nothing yields here, and the
    // BKL is held, so the pair is atomic against every other task.
    let Some(cur) = crate::uaccess::read_val::<u32>(uaddr) else {
        return errno::EFAULT;
    };
    if cur != val {
        return errno::EAGAIN;
    }
    // SAFETY: raw-pointer access under the BKL; see `WAITERS`.
    unsafe { (*waiters()).enqueue(key, Waiter(me, crate::net::uptime_us()), bitset) };
    WAIT_ENQUEUES.fetch_add(1, Ordering::Relaxed);

    loop {
        // Arm the park before the membership test. A `FUTEX_WAKE` that dequeues
        // this waiter between the test and the park would otherwise be a lost
        // wake: the table says "gone" a moment too late and the scheduler was
        // never told. Armed first, that wake sets `wake_pending` and the park
        // below returns at once.
        // Without this the deadline below is unreachable whenever every other
        // runnable task is also spinning in the kernel: `uptime_us` is the
        // LAPIC tick counter, a syscall runs with `IF` clear, and only the
        // idle loop re-enables it. `allow_tick`'s own comment has the
        // measurement.
        //
        // **Timed waits only.** The hlt costs one LAPIC tick (10 ms) per
        // loop iteration, and the loop runs it *before* the wake test — so an
        // untimed waiter paid a full tick on the way in and another after
        // every wake, on a core where the waker is runnable and cannot run
        // until the hlt ends. With `block_current` descheduling properly and
        // `sched::wake` recording `wake_pending`, an untimed wait needs no
        // interrupt window at all: park, waker runs, resume, re-check. Every
        // pthread condvar/jobserver round-trip was two ticks of pure latency;
        // a self-host rustc build spent its wall clock here, not in the
        // compiler (measured 2026-09-17: a 100 s `akuma-exec` rebuild billed
        // its rustc 90 ms of CPU). A *timed* wait still needs the window —
        // its deadline is read off `uptime_us`, which does not advance while
        // `IF` is clear — so the hlt stays for that arm.
        if deadline_at != deadline::NEVER {
            crate::sched::allow_tick();
        }
        // Off the table is the wake. Checked with `iter` rather than `queue()`
        // or `locate_and_take` because those two allocate; this borrows.
        // SAFETY: raw-pointer access under the BKL.
        let still = unsafe { queued_key(&*waiters(), tgid, me) };
        if still.is_none() {
            return 0;
        }
        // The group is going away. A thread parked here has *already* passed
        // syscall entry, so `should_leave_now`'s check there can never fire for
        // it again — and `thread::drain` waits for exactly this thread before
        // the process may be reaped. Without this line an `exit_group` while a
        // sibling holds an untimed `FUTEX_WAIT` is a hang, not a wake: the
        // waiter spins for a wake that is never coming and the exiting thread
        // spins for it. Both loops make progress and neither terminates, which
        // is the worst shape a deadlock can take because nothing looks stuck.
        if crate::thread::should_leave_now() {
            // SAFETY: raw-pointer access under the BKL.
            let _ = unsafe { (*waiters()).remove_anywhere(tgid, me) };
            return errno::EINTR;
        }
        // **And the leader's route out, which the line above cannot be.**
        //
        // `should_leave_now` returns `false` for the main thread by
        // construction — `thread::drain` is called *by* the leader and must not
        // interrupt itself — and it reads `GROUP_EXIT`, which only `exit_group`
        // and `drain` ever set. Neither is on the path a *fault* takes:
        // `signal::notify_group_of_thread_fatal` records a group exit status
        // and calls `deliver_signal`, on the stated understanding that "every
        // group member's next syscall return takes this exit". A leader parked
        // in an untimed `FUTEX_WAIT` has no next syscall return, so it never
        // took it — and since the loop's only other exits are a dequeue and a
        // deadline it does not have, it parked forever.
        //
        // That is the `-j4` wedge, measured end to end 2026-09-18
        // (`AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §13): a rustc worker `#GP`s,
        // its siblings die, and the leader sits on `pthread_join`'s futex for
        // the whole 600 s budget while `cargo`'s `wait4` waits on a process
        // that can never exit. `[BKL] stuck` silent, no leaked scheduler gate,
        // nothing in `ps` but a row at `0:00`.
        //
        // `should_interrupt_blocking_syscall` is the predicate every blocking
        // arm in `akuma-syscalls-glue` already consults, and it **takes** the
        // interrupted flag rather than peeking, so this cannot become an
        // `EINTR` storm. `EINTR` is also what Linux returns here; musl retries
        // it, and the retry is what carries the thread through the syscall
        // epilogue where `group_exit_status` is waiting for it.
        if akuma_exec::process::should_interrupt_blocking_syscall() {
            // SAFETY: raw-pointer access under the BKL.
            let _ = unsafe { (*waiters()).remove_anywhere(tgid, me) };
            return errno::EINTR;
        }
        if deadline::expired(deadline_at, crate::net::uptime_us()) {
            // `remove_anywhere`, not `dequeue(key, ..)`: a `FUTEX_REQUEUE` may
            // have moved this waiter behind its back, and dequeuing from the
            // original key would leave the entry on the requeue target where it
            // silently absorbs a later wake.
            // SAFETY: raw-pointer access under the BKL.
            let _ = unsafe { (*waiters()).remove_anywhere(tgid, me) };
            return errno::ETIMEDOUT;
        }
        // Park. `deadline_at` is an absolute *uptime* deadline in both arms —
        // `deadline::deadline_us` normalises a relative `FUTEX_WAIT`, an
        // absolute `FUTEX_WAIT_BITSET` and a `FUTEX_CLOCK_REALTIME` wait onto
        // the one clock `net::uptime_us` reads, which is the clock the
        // scheduler's own deadlines use. `NEVER` is `u64::MAX`, so handing it
        // to `block_until_deadline` would work too; `block_current` is spelled
        // out because it is the arm that carries the backstop, and an untimed
        // futex wait is exactly where a missing wake must not become a hang.
        if deadline_at == deadline::NEVER {
            crate::sched::block_current();
        } else {
            crate::sched::block_until_deadline(deadline_at);
        }
    }
}

/// Which key `tid` is queued on under `tgid`, without allocating.
fn queued_key(t: &WaiterTable<Waiter>, tgid: u32, tid: usize) -> Option<Key> {
    t.iter()
        .find(|(k, q)| k.0 == tgid && q.iter().any(|(h, _)| h.tid() == tid))
        .map(|(k, _)| *k)
}

fn wake(uaddr: u64, val: u32, mask: u32, private: bool) -> u64 {
    let key: Key = (namespace(private), uaddr as usize);
    // SAFETY: raw-pointer access under the BKL.
    let woken = unsafe { (*waiters()).wake(key, val, mask) };
    WAKE_CALLS.fetch_add(1, Ordering::Relaxed);
    if woken.is_empty() {
        // A wake that found nobody. **Not by itself a bug** — the overwhelmingly
        // common case is an uncontended mutex unlock, where musl calls
        // `FUTEX_WAKE` because it cannot know there is no waiter. It earns a
        // counter because the *ratio* is the diagnostic: a process whose threads
        // are all parked while empty wakes keep arriving is a key mismatch (the
        // waiter is queued under a different `(tgid, uaddr)` than the waker
        // computes), which looks identical from `ps` to a userspace deadlock and
        // is a kernel bug where the other is not.
        WAKE_EMPTY.fetch_add(1, Ordering::Relaxed);
    } else {
        WOKEN_TOTAL.fetch_add(woken.len() as u64, Ordering::Relaxed);
    }
    resume(&woken);
    woken.len() as u64
}

/// `FUTEX_WAKE` calls, calls that found no waiter, waiters actually woken, and
/// `FUTEX_WAIT` enqueues. See [`dump_waiters`] for how to read them.
static WAKE_CALLS: AtomicU64 = AtomicU64::new(0);
/// See [`WAKE_CALLS`].
static WAKE_EMPTY: AtomicU64 = AtomicU64::new(0);
/// See [`WAKE_CALLS`].
static WOKEN_TOTAL: AtomicU64 = AtomicU64::new(0);
/// See [`WAKE_CALLS`].
static WAIT_ENQUEUES: AtomicU64 = AtomicU64::new(0);

/// Every queued futex waiter, by key, plus the wake tallies.
///
/// # Why this exists
///
/// The slot table (`sched::dump_slot_table`) says a thread is `WAITING` and
/// that its last syscall was `futex`. It cannot say *which* futex, and that is
/// the whole remaining question for the `-j4` wedge: five threads of one
/// address space parked in an untimed `FUTEX_WAIT`, waking on the scheduler's
/// backstop every half second, re-testing membership and re-parking, with no
/// new syscall between (`AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §13).
///
/// Two very different faults produce that, and the key is what tells them
/// apart. If all the parked threads sit on **one** key with nobody outside it
/// to unlock, it is an ordinary userspace deadlock — possibly downstream of an
/// earlier dropped wake, but not a live kernel bug. If they sit on keys whose
/// `tgid` differs while sharing one address space, or if `WAKE_EMPTY` is
/// climbing against a table that is not empty, the waker and the waiter
/// disagree about the key and the kernel is losing wakes.
///
/// Allocation-free and bounded: borrows through [`WaiterTable::iter`] and
/// prints at most [`DUMP_KEY_LIMIT`] keys.
pub fn dump_waiters() {
    let (calls, empty, woken, enq) = (
        WAKE_CALLS.load(Ordering::Relaxed),
        WAKE_EMPTY.load(Ordering::Relaxed),
        WOKEN_TOTAL.load(Ordering::Relaxed),
        WAIT_ENQUEUES.load(Ordering::Relaxed),
    );
    akuma_primitives::safe_print!(160,
        "[FUTEX] wakes={} empty={} woken={} enqueues={}\n", calls, empty, woken, enq);
    let now = crate::net::uptime_us();
    // SAFETY: raw-pointer read under the BKL, the same discipline every other
    // reader of this table uses.
    let t = unsafe { &*waiters() };
    let mut keys = 0usize;
    for (key, q) in t.iter() {
        if q.is_empty() {
            continue;
        }
        keys += 1;
        if keys > DUMP_KEY_LIMIT {
            continue;
        }
        // One line per key, the tids inline. A queue longer than eight is
        // truncated rather than wrapped — the interesting case is a handful of
        // threads, and an unbounded line is how a console loses the next one.
        let mut w = akuma_primitives::console::StackWriter::<224>::new();
        let _ = core::fmt::write(&mut w, format_args!(
            "[FUTEX] key tgid={} uaddr=0x{:x} waiters={} tids=", key.0, key.1, q.len()));
        for (h, bits) in q.iter().take(8) {
            // `tid/bitset@age-in-ms`. The age is the diagnostic: see `Waiter`.
            let _ = core::fmt::write(&mut w, format_args!(
                "{}/{:#x}@{}ms ", h.tid(), bits, now.saturating_sub(h.1) / 1000));
        }
        let _ = core::fmt::write(&mut w, format_args!("\n"));
        w.flush();
    }
    if keys > DUMP_KEY_LIMIT {
        akuma_primitives::safe_print!(96,
            "[FUTEX] ... {} more non-empty keys\n", keys - DUMP_KEY_LIMIT);
    }
    if keys == 0 {
        akuma_primitives::safe_print!(64, "[FUTEX] no queued waiters\n");
    }
}

/// How many keys [`dump_waiters`] prints before summarising the rest.
const DUMP_KEY_LIMIT: usize = 24;

/// Tell the scheduler to look at every waiter the table just took off a queue.
///
/// The dequeue is the wake; this is what stops it costing a round of the
/// round-robin to notice. A waiter that has been dequeued but has not parked yet
/// is not missed — `sched::wake` records `wake_pending` for a task that is
/// merely runnable, and the waiter's own `prepare_block` is what reads it.
fn resume(woken: &[Waiter]) {
    for w in woken {
        crate::sched::wake(w.tid());
    }
}

/// `FUTEX_REQUEUE` / `FUTEX_CMP_REQUEUE`. `timeout` is reinterpreted as
/// `val2` (the requeue cap) — Linux overloads the argument for this op.
fn requeue(
    uaddr: u64,
    uaddr2: u64,
    val: u32,
    val2: u64,
    compare: Option<u32>,
    private: bool,
) -> u64 {
    if let Some(expect) = compare {
        let Some(cur) = crate::uaccess::read_val::<u32>(uaddr) else {
            return errno::EFAULT;
        };
        if cur != expect {
            return errno::EAGAIN;
        }
    }
    let tgid = namespace(private);
    // SAFETY: raw-pointer access under the BKL.
    let (woken, moved) = unsafe {
        (*waiters()).requeue(
            (tgid, uaddr as usize),
            (tgid, uaddr2 as usize),
            val,
            val2.min(u64::from(u32::MAX)) as u32,
        )
    };
    resume(&woken);
    // The requeued waiters are deliberately **not** resumed: they were moved to
    // another key, not woken, and they are still queued. Waking them would be
    // harmless (each re-tests its own membership and parks again) but it would
    // undo the whole point of a requeue, which is that a broadcast does not
    // stampede every waiter onto one lock.
    //
    // Linux reports woken + requeued for `CMP_REQUEUE`, and woken alone for the
    // deprecated `REQUEUE`. Both callers in practice (musl's
    // `pthread_cond_broadcast`, Rust's `Condvar::notify_all`) use `CMP_`.
    (woken.len() + moved.len()) as u64
}

/// `FUTEX_WAKE_OP`: read-modify-write `*uaddr2`, wake on `uaddr`, and wake on
/// `uaddr2` too if the *old* value satisfies the comparison.
fn wake_op(uaddr: u64, uaddr2: u64, val: u32, val2: u64, val3: u32, private: bool) -> u64 {
    let decoded = akuma_syscalls_sync::wakeop::WakeOp::decode(val3);
    let Some(oldval) = crate::uaccess::read_val::<u32>(uaddr2) else {
        return errno::EFAULT;
    };
    let Some(newval) = decoded.apply(oldval) else {
        return errno::ENOSYS;
    };
    if !crate::uaccess::write_val::<u32>(uaddr2, newval) {
        return errno::EFAULT;
    }
    let tgid = namespace(private);
    // SAFETY: raw-pointer access under the BKL.
    let first = unsafe { (*waiters()).wake((tgid, uaddr as usize), val, MATCH_ANY) };
    resume(&first);
    let mut n = first.len();
    if decoded.compare(oldval) {
        let cap = val2.min(u64::from(u32::MAX)) as u32;
        // SAFETY: raw-pointer access under the BKL.
        let second = unsafe { (*waiters()).wake((tgid, uaddr2 as usize), cap, MATCH_ANY) };
        resume(&second);
        n += second.len();
    }
    n as u64
}

/// Wake up to `count` waiters on `(tgid, uaddr)`, for `akuma-exec`'s
/// `futex_wake` runtime hook.
///
/// The hook was a `not_wired!` panic until 5b slice 4, on the stated grounds
/// that this table is "keyed by its own task ids — a different namespace from
/// `akuma-exec`'s pids". Half of that was true and the wrong half: the *waiter
/// identity* is a scheduler task slot, but the **key** is `(tgid, uaddr)`, and
/// since 5b slice 2 that `tgid` comes from [`namespace`] → `current_pid()` →
/// `akuma-exec`'s `THREAD_PID_MAP`. It is the same number `Process::tgid`
/// carries, so the hook's argument needs no translation at all; what unblocked
/// it was the identity fold, not this one.
///
/// `count` is `i32` because the caller passes `i32::MAX` for "all"
/// (`clear_child_tid` at process exit); a negative value is treated as all,
/// which is the only reading of it that is not a silent no-op.
pub fn wake_key(tgid: u32, uaddr: usize, count: i32) -> usize {
    let n = if count < 0 { u32::MAX } else { count.cast_unsigned() };
    // SAFETY: raw-pointer access under the BKL; see `WAITERS`.
    let woken = unsafe { (*waiters()).wake((tgid, uaddr), n, MATCH_ANY) };
    resume(&woken);
    woken.len()
}

/// Drop every queue entry naming task slot `task`.
///
/// Called from thread teardown, which is the point at which the slot becomes
/// eligible for reuse. A thread that left `FUTEX_WAIT` by its own loop has
/// already dequeued itself and this finds nothing; one killed while parked has
/// not, and without this its entry would name whoever inherits the slot.
pub fn purge_task(task: usize) {
    // SAFETY: raw-pointer access under the BKL.
    let touched = unsafe { (*waiters()).purge(task) };
    let _ = touched;
}
