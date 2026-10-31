//! The `futex(2)` syscall body, shared by the two bare-metal kernels.
//!
//! `amd64/src/futex.rs` and `taoxichuan/src/futex.rs` each carried a copy of the
//! same dispatch and the same three non-blocking operations (`wake`, `requeue`,
//! `wake_op`), differing only in how they read user memory, how they name the
//! caller's thread group and what "wake a thread" means. That difference is the
//! [`Kernel`] trait; the copies are this file.
//!
//! What stays with the kernel, deliberately: the table's *lock* (`table`), the
//! user-memory reads (which can demand-page), the **wait loop** (each kernel's
//! park primitive and exit/interrupt checks differ - see both `wait`s), and
//! the diagnostics. This is the "decisions plus effects returned" model of the
//! rest of the crate with one concession: the effects are a trait the kernel
//! implements once, on a zero-sized type.

use alloc::vec::Vec;

use akuma_syscalls_linux::flags::futex as f;

use crate::deadline;
use crate::op::{self, Action};
use crate::table::{Key, MATCH_ANY, WaiterId, WaiterTable};
use crate::wakeop::WakeOp;

/// `-EFAULT`, `-EAGAIN`, `-ENOSYS` as the syscall return word.
pub const EFAULT: u64 = (-14i64) as u64;
pub const EAGAIN: u64 = (-11i64) as u64;
pub const ENOSYS: u64 = (-38i64) as u64;

/// What the body needs from the kernel it runs in.
pub trait Kernel {
    /// The queued-waiter handle (a slot, plus whatever the kernel records).
    type W: WaiterId;

    /// Run `f` with the waiter table locked. Nothing inside `f` may block.
    fn table<R>(&self, f: impl FnOnce(&mut WaiterTable<Self::W>) -> R) -> R;
    /// The namespace half of this caller's keys (`key::namespace(..).tgid()`).
    fn namespace(&self, is_private: bool) -> u32;
    /// A checked 4-byte read of user memory; `None` is unmapped.
    fn read_u32(&self, uaddr: u64) -> Option<u32>;
    /// A checked 4-byte write; `false` is unmapped.
    fn write_u32(&self, uaddr: u64, v: u32) -> bool;
    /// A checked read of a user `struct timespec` as `(sec, nsec)`.
    fn read_timespec(&self, uaddr: u64) -> Option<(i64, i64)>;
    /// Monotonic microseconds, and wall-clock microseconds when known.
    fn now_us(&self) -> u64;
    fn utc_us(&self, now_us: u64) -> Option<u64>;
    /// Tell the scheduler to look at each waiter the table just dequeued.
    fn resume(&self, woken: &[Self::W]);
    /// A `FUTEX_WAKE` finished and dequeued `n` waiters (diagnostic tally only).
    fn woke(&self, _n: usize) {}
    /// `FUTEX_WAIT[_BITSET]` after decode: the park loop is the kernel's.
    fn wait(&self, uaddr: u64, val: u32, bitset: u32, deadline_at: u64, private: bool) -> u64;
}

/// `futex(uaddr, op, val, timeout, uaddr2, val3)`.
pub fn sys_futex<K: Kernel>(k: &K, uaddr: u64, op: u64, val: u64, timeout: u64, uaddr2: u64, val3: u64) -> u64 {
    let opi = (op as u32).cast_signed();
    let private = op & (f::FUTEX_PRIVATE_FLAG as u64) != 0;
    let val3 = val3 as u32;
    let val = val as u32;
    // `uaddr_mapped` is a probe the kernel performs: a read can demand-page.
    let mapped = k.read_u32(uaddr).is_some();
    match op::decode(opi, val3, uaddr as usize, mapped) {
        Action::Return(v) => v,
        Action::Wait { bitset } => {
            let deadline_at = if timeout == 0 {
                deadline::NEVER
            } else {
                let Some((sec, nsec)) = k.read_timespec(timeout) else {
                    return EFAULT;
                };
                let us = sec.max(0).cast_unsigned().saturating_mul(1_000_000)
                    .saturating_add(nsec.max(0).cast_unsigned() / 1000);
                let now = k.now_us();
                deadline::deadline_us(opi, us, now, k.utc_us(now))
            };
            k.wait(uaddr, val, bitset, deadline_at, private)
        }
        Action::Wake { mask } => wake(k, uaddr, val, mask, private),
        Action::Requeue { compare } => requeue(k, uaddr, uaddr2, val, timeout, compare, private),
        Action::WakeOp => wake_op(k, uaddr, uaddr2, val, timeout, val3, private),
    }
}

/// `FUTEX_WAKE`: dequeue up to `val` waiters on `(ns, uaddr)` matching `mask`.
pub fn wake<K: Kernel>(k: &K, uaddr: u64, val: u32, mask: u32, private: bool) -> u64 {
    let key: Key = (k.namespace(private), uaddr as usize);
    let woken = k.table(|t| t.wake(key, val, mask));
    k.woke(woken.len());
    k.resume(&woken);
    woken.len() as u64
}

/// `FUTEX_REQUEUE` / `FUTEX_CMP_REQUEUE`.
pub fn requeue<K: Kernel>(k: &K, uaddr: u64, uaddr2: u64, val: u32, val2: u64, compare: Option<u32>, private: bool) -> u64 {
    if let Some(expect) = compare {
        let Some(cur) = k.read_u32(uaddr) else {
            return EFAULT;
        };
        if cur != expect {
            return EAGAIN;
        }
    }
    let tgid = k.namespace(private);
    let (woken, moved) = k.table(|t| {
        t.requeue((tgid, uaddr as usize), (tgid, uaddr2 as usize), val, val2.min(u64::from(u32::MAX)) as u32)
    });
    k.resume(&woken);
    (woken.len() + moved.len()) as u64
}

/// `FUTEX_WAKE_OP`.
pub fn wake_op<K: Kernel>(k: &K, uaddr: u64, uaddr2: u64, val: u32, val2: u64, val3: u32, private: bool) -> u64 {
    let decoded = WakeOp::decode(val3);
    let Some(oldval) = k.read_u32(uaddr2) else {
        return EFAULT;
    };
    let Some(newval) = decoded.apply(oldval) else {
        return ENOSYS;
    };
    if !k.write_u32(uaddr2, newval) {
        return EFAULT;
    }
    let tgid = k.namespace(private);
    let first = k.table(|t| t.wake((tgid, uaddr as usize), val, MATCH_ANY));
    k.resume(&first);
    let mut n = first.len();
    if decoded.compare(oldval) {
        let cap = val2.min(u64::from(u32::MAX)) as u32;
        let second = k.table(|t| t.wake((tgid, uaddr2 as usize), cap, MATCH_ANY));
        k.resume(&second);
        n += second.len();
    }
    n as u64
}

/// Wake up to `count` waiters on `(tgid, uaddr)` - the `CLONE_CHILD_CLEARTID`
/// wake `pthread_join` waits on.
pub fn wake_key<K: Kernel>(k: &K, tgid: u32, uaddr: usize, count: u32) -> usize {
    let woken = k.table(|t| t.wake((tgid, uaddr), count, MATCH_ANY));
    k.resume(&woken);
    woken.len()
}

/// Drop every queue entry a dead thread slot holds.
pub fn purge_task<K: Kernel>(k: &K, task: usize) -> Vec<Key> {
    k.table(|t| t.purge(task))
}
