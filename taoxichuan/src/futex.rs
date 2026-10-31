//! `futex(2)` - the Taoxichuan half: the EFFECTS only.
//!
//! The decisions are `akuma-syscalls-sync`'s (op decode, the `(tgid, uaddr)`
//! key namespace, the waiter table, the deadline algebra, `WAKE_OP`); this
//! module reads and writes the task's memory, holds the table and parks. It is
//! `amd64/src/futex.rs`'s shape on this kernel's park primitive: a waiter
//! queues, then parks on `akuma_threading::schedule_blocking`; membership in
//! the table is the signal (durable state, not an edge), and a wake is the
//! table decision followed by the threading waker, whose sticky wake makes an
//! early wake harmless.
//!
//! Not copied from amd64: the group-exit and `EINTR` checks (`exit_group`
//! here terminates the sibling threads and purges their queue entries itself,
//! see `child::exit_group_threads`), and the diagnostics counters.

use akuma_syscalls_sync::body;
use akuma_syscalls_sync::deadline;
use akuma_syscalls_sync::key;
use akuma_syscalls_sync::table::{Key, WaiterId, WaiterTable};
use spinning_top::Spinlock;

use crate::ring3::err;

/// A queued waiter: its threading slot.
#[derive(Clone, Copy)]
struct Waiter(usize);

impl WaiterId for Waiter {
    fn tid(self) -> usize {
        self.0
    }
}

static WAITERS: Spinlock<WaiterTable<Waiter>> = Spinlock::new(WaiterTable::new());

/// The namespace half of this caller's keys: the thread group (`tgid`) for a
/// private word, as the crate decides; the pid of the calling process stands
/// in for the group when the Process table cannot answer.
fn namespace(is_private: bool) -> u32 {
    let tgid = akuma_exec::process::current_process_shared().map(|p| p.tgid);
    key::namespace(is_private, tgid, false).tgid()
}

fn read_u32(uaddr: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    if !crate::user_ok(uaddr, 4) || !crate::copy_from_user(&mut b, uaddr) {
        return None;
    }
    Some(u32::from_ne_bytes(b))
}

fn write_u32(uaddr: u64, v: u32) -> bool {
    crate::user_ok(uaddr, 4) && crate::copy_to_user(uaddr, &v.to_ne_bytes())
}

/// This kernel's side of `akuma_syscalls_sync::body::Kernel`: user memory,
/// the table lock, the waker and the park loop.
struct Tx;

impl body::Kernel for Tx {
    type W = Waiter;
    fn table<R>(&self, f: impl FnOnce(&mut WaiterTable<Waiter>) -> R) -> R {
        f(&mut WAITERS.lock())
    }
    fn namespace(&self, is_private: bool) -> u32 {
        namespace(is_private)
    }
    fn read_u32(&self, uaddr: u64) -> Option<u32> {
        read_u32(uaddr)
    }
    fn write_u32(&self, uaddr: u64, v: u32) -> bool {
        write_u32(uaddr, v)
    }
    fn read_timespec(&self, uaddr: u64) -> Option<(i64, i64)> {
        let mut b = [0u8; 16];
        if !crate::user_ok(uaddr, 16) || !crate::copy_from_user(&mut b, uaddr) {
            return None;
        }
        let sec = i64::from_ne_bytes(b[0..8].try_into().unwrap_or([0; 8]));
        let nsec = i64::from_ne_bytes(b[8..16].try_into().unwrap_or([0; 8]));
        Some((sec, nsec))
    }
    fn now_us(&self) -> u64 {
        crate::net::uptime_us()
    }
    fn utc_us(&self, now_us: u64) -> Option<u64> {
        akuma_primitives::clock::utc_time_us(now_us)
    }
    fn resume(&self, woken: &[Waiter]) {
        resume(woken);
    }
    fn wait(&self, uaddr: u64, val: u32, bitset: u32, deadline_at: u64, private: bool) -> u64 {
        wait(uaddr, val, bitset, deadline_at, private)
    }
}

/// `futex(uaddr, op, val, timeout, uaddr2, val3)` - x86_64 202.
pub fn sys_futex(uaddr: u64, op: u64, val: u64, timeout: u64, uaddr2: u64, val3: u64) -> u64 {
    body::sys_futex(&Tx, uaddr, op, val, timeout, uaddr2, val3)
}

fn wait(uaddr: u64, val: u32, bitset: u32, deadline_at: u64, private: bool) -> u64 {
    let tgid = namespace(private);
    let me = crate::exec_twin::current_slot();
    if me == crate::exec_twin::NO_SLOT {
        // The boot task parks nowhere (it is the machine's guaranteed runner):
        // a wait that cannot sleep is a would-block.
        return crate::e(err::EAGAIN);
    }
    let key: Key = (tgid, uaddr as usize);
    // Re-read and enqueue with nothing in between: a wake that lands between
    // the comparison and the enqueue would otherwise find no waiter.
    let Some(cur) = read_u32(uaddr) else {
        return crate::e(err::EFAULT);
    };
    if cur != val {
        return crate::e(err::EAGAIN);
    }
    WAITERS.lock().enqueue(key, Waiter(me), bitset);
    loop {
        // Off the table is the wake. `iter` borrows; `queue()` would allocate.
        if queued_key(&WAITERS.lock(), tgid, me).is_none() {
            return 0;
        }
        if deadline::expired(deadline_at, crate::net::uptime_us()) {
            // `remove_anywhere`: a requeue may have moved this waiter.
            let _ = WAITERS.lock().remove_anywhere(tgid, me);
            return crate::e(err::ETIMEDOUT);
        }
        // The stub's user-RSP stash is global; the parked task's own copy
        // must survive the park (the `pipes::park_once` rule).
        let stash = crate::ring3::user_rsp_stash();
        akuma_threading::schedule_blocking(deadline_at);
        crate::ring3::set_user_rsp_stash(stash);
    }
}

fn queued_key(t: &WaiterTable<Waiter>, tgid: u32, tid: usize) -> Option<Key> {
    t.iter()
        .find(|(k, q)| k.0 == tgid && q.iter().any(|(h, _)| h.tid() == tid))
        .map(|(k, _)| *k)
}

fn resume(woken: &[Waiter]) {
    for w in woken {
        akuma_threading::get_waker_for_thread(w.tid()).wake();
    }
}

/// Wake up to `count` waiters on `(tgid, uaddr)` - the thread exit's
/// `CLONE_CHILD_CLEARTID` wake, the call `pthread_join` waits on.
pub fn wake_key(tgid: u32, uaddr: usize, count: u32) -> usize {
    body::wake_key(&Tx, tgid, uaddr, count)
}

/// Drop every queue entry a dead thread slot holds, so a recycled slot cannot
/// absorb a wake meant for someone else.
pub fn purge_task(task: usize) {
    let _ = WAITERS.lock().purge(task);
}
