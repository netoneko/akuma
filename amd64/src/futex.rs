//! `futex(2)` — the amd64 half.
//!
//! The decisions are not here. `akuma-syscalls-sync` owns the op decode, the
//! `(tgid, uaddr)` key namespace, the waiter table and the deadline algebra,
//! and it is host-tested; per that crate's own header, every futex bug in this
//! tree's history was a property of one of those four things. This module is
//! the effects: reading and writing user memory, holding the table, and
//! parking.
//!
//! # Why the wait is a poll and not a park
//!
//! The AArch64 kernel wakes a futex waiter by firing a `ThreadWaker` at a
//! specific tid; this target has no such thing — `net::park_until`'s own
//! comment says so, and `wait4` already spins on `yield_now` for the same
//! reason. So `FUTEX_WAKE` here does exactly one thing: it takes the waiter off
//! the table. The waiter notices on its next poll, because "am I still queued?"
//! is the only wake signal it needs, and it is a signal the table already
//! carries. That is not a lesser design so much as a smaller one: it cannot
//! lose a wake (the removal is durable state, not an edge), and it costs a
//! round of the round-robin per waiter per tick.
//!
//! What it does cost is CPU: an untimed `FUTEX_WAIT` with nothing else runnable
//! spins. On a cooperative single-core kernel that is survivable — something
//! else is runnable, by construction, or the wake is never coming — and it is
//! the same trade `wait4` and the block driver already make.
//!
//! # Allocation
//!
//! The poll loop allocates **nothing**. Membership is checked through
//! [`WaiterTable::iter`], which borrows; the obvious spellings
//! (`queue()`, `locate_and_take()`) each allocate or churn a `BTreeMap` entry
//! per tick, which for a loop that runs at scheduler frequency is the
//! difference between free and not. `enqueue` allocates once per wait, and
//! `wake` returns a `Vec` sized by how many waiters it actually took.

use akuma_syscalls_linux::flags::futex as f;
use akuma_syscalls_sync::deadline;
use akuma_syscalls_sync::key::{self, Namespace};
use akuma_syscalls_sync::op::{self, Action};
use akuma_syscalls_sync::table::{Key, MATCH_ANY, WaiterId, WaiterTable};

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
#[derive(Clone, Copy)]
struct Waiter(usize);

impl WaiterId for Waiter {
    fn tid(self) -> usize {
        self.0
    }
}

/// The waiter table.
///
/// `static mut` behind a raw pointer, reached only from syscall context under
/// the BKL — the same discipline as `usermode::PROCS`, and for the same reason:
/// every writer is kernel code, kernel code holds the lock, and a context
/// switch keeps it held.
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
    unsafe { (*waiters()).enqueue(key, Waiter(me), bitset) };

    loop {
        crate::sched::yield_now();
        // Off the table is the wake. Checked with `iter` rather than `queue()`
        // or `locate_and_take` because this runs once per scheduler round and
        // those two allocate; this borrows.
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
        if deadline::expired(deadline_at, crate::net::uptime_us()) {
            // `remove_anywhere`, not `dequeue(key, ..)`: a `FUTEX_REQUEUE` may
            // have moved this waiter behind its back, and dequeuing from the
            // original key would leave the entry on the requeue target where it
            // silently absorbs a later wake.
            // SAFETY: raw-pointer access under the BKL.
            let _ = unsafe { (*waiters()).remove_anywhere(tgid, me) };
            return errno::ETIMEDOUT;
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
    woken.len() as u64
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
    let mut n = unsafe { (*waiters()).wake((tgid, uaddr as usize), val, MATCH_ANY).len() };
    if decoded.compare(oldval) {
        let cap = val2.min(u64::from(u32::MAX)) as u32;
        // SAFETY: raw-pointer access under the BKL.
        n += unsafe { (*waiters()).wake((tgid, uaddr2 as usize), cap, MATCH_ANY).len() };
    }
    n as u64
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
