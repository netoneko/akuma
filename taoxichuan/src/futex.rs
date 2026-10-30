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

use akuma_syscalls_linux::flags::futex as f;
use akuma_syscalls_sync::deadline;
use akuma_syscalls_sync::key;
use akuma_syscalls_sync::op::{self, Action};
use akuma_syscalls_sync::table::{Key, MATCH_ANY, WaiterId, WaiterTable};
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

/// `futex(uaddr, op, val, timeout, uaddr2, val3)` - x86_64 202.
pub fn sys_futex(uaddr: u64, op: u64, val: u64, timeout: u64, uaddr2: u64, val3: u64) -> u64 {
    let opi = (op as u32).cast_signed();
    let private = op & (f::FUTEX_PRIVATE_FLAG as u64) != 0;
    let val3 = val3 as u32;
    let val = val as u32;
    let mapped = read_u32(uaddr).is_some();
    match op::decode(opi, val3, uaddr as usize, mapped) {
        Action::Return(v) => v,
        Action::Wait { bitset } => {
            let deadline_at = if timeout == 0 {
                deadline::NEVER
            } else {
                let mut b = [0u8; 16];
                if !crate::user_ok(timeout, 16) || !crate::copy_from_user(&mut b, timeout) {
                    return crate::e(err::EFAULT);
                }
                let sec = i64::from_ne_bytes(b[0..8].try_into().unwrap_or([0; 8]));
                let nsec = i64::from_ne_bytes(b[8..16].try_into().unwrap_or([0; 8]));
                let us = sec.max(0).cast_unsigned().saturating_mul(1_000_000)
                    .saturating_add(nsec.max(0).cast_unsigned() / 1000);
                let now = crate::net::uptime_us();
                let utc = akuma_primitives::clock::utc_time_us(now);
                deadline::deadline_us(opi, us, now, utc)
            };
            wait(uaddr, val, bitset, deadline_at, private)
        }
        Action::Wake { mask } => wake(uaddr, val, mask, private),
        Action::Requeue { compare } => requeue(uaddr, uaddr2, val, timeout, compare, private),
        Action::WakeOp => wake_op(uaddr, uaddr2, val, timeout, val3, private),
    }
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

fn wake(uaddr: u64, val: u32, mask: u32, private: bool) -> u64 {
    let key: Key = (namespace(private), uaddr as usize);
    let woken = WAITERS.lock().wake(key, val, mask);
    resume(&woken);
    woken.len() as u64
}

fn requeue(uaddr: u64, uaddr2: u64, val: u32, val2: u64, compare: Option<u32>, private: bool) -> u64 {
    if let Some(expect) = compare {
        let Some(cur) = read_u32(uaddr) else {
            return crate::e(err::EFAULT);
        };
        if cur != expect {
            return crate::e(err::EAGAIN);
        }
    }
    let tgid = namespace(private);
    let (woken, moved) = WAITERS.lock().requeue(
        (tgid, uaddr as usize),
        (tgid, uaddr2 as usize),
        val,
        val2.min(u64::from(u32::MAX)) as u32,
    );
    resume(&woken);
    (woken.len() + moved.len()) as u64
}

fn wake_op(uaddr: u64, uaddr2: u64, val: u32, val2: u64, val3: u32, private: bool) -> u64 {
    let decoded = akuma_syscalls_sync::wakeop::WakeOp::decode(val3);
    let Some(oldval) = read_u32(uaddr2) else {
        return crate::e(err::EFAULT);
    };
    let Some(newval) = decoded.apply(oldval) else {
        return crate::e(err::ENOSYS);
    };
    if !write_u32(uaddr2, newval) {
        return crate::e(err::EFAULT);
    }
    let tgid = namespace(private);
    let first = WAITERS.lock().wake((tgid, uaddr as usize), val, MATCH_ANY);
    resume(&first);
    let mut n = first.len();
    if decoded.compare(oldval) {
        let cap = val2.min(u64::from(u32::MAX)) as u32;
        let second = WAITERS.lock().wake((tgid, uaddr2 as usize), cap, MATCH_ANY);
        resume(&second);
        n += second.len();
    }
    n as u64
}

/// Wake up to `count` waiters on `(tgid, uaddr)` - the thread exit's
/// `CLONE_CHILD_CLEARTID` wake, the call `pthread_join` waits on.
pub fn wake_key(tgid: u32, uaddr: usize, count: u32) -> usize {
    let woken = WAITERS.lock().wake((tgid, uaddr), count, MATCH_ANY);
    resume(&woken);
    woken.len()
}

/// Drop every queue entry a dead thread slot holds, so a recycled slot cannot
/// absorb a wake meant for someone else.
pub fn purge_task(task: usize) {
    let _ = WAITERS.lock().purge(task);
}
