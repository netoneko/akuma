//! The System V message-queue family — `msgget`, `msgctl`, `msgsnd`, `msgrcv`.
//!
//! The fifth syscall family to move out of `src/syscall/`, after `-time`,
//! `-sync`, `-poll` and `-mem`, and the first to move for a **structural**
//! reason rather than for host tests: `/proc` lists the queues, so
//! `src/vfs/proc.rs` read `crate::syscall::msgqueue` while `src/syscall/` reads
//! 37 symbols back out of `src/vfs/`. Cargo crates cannot be mutually
//! dependent, and that loop is what kept both directories in the binary
//! (`docs/archive/SRC_SYSCALL_EXTRACTION.md`, Blocker 1).
//!
//! Unlike its siblings this crate is not "the pure logic, with the effects left
//! behind" — it owns the queue table, performs its own user copies and wakes its
//! own pollers. The seam here is a dependency edge, not a purity boundary, and
//! pretending otherwise would mean threading the whole table through a hook for
//! no gain.
//!
//! # Queues are keyed `(box_id, msqid)`
//!
//! Not by `msqid` alone. The key is what keeps two boxes that both call
//! `msgget(0x1234)` from landing on one queue, and it is why every entry point
//! here takes a `box_id` the caller resolves rather than reading one itself.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeMap as PollerMap, VecDeque};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use spinning_top::Spinlock;

use akuma_exec::process::user_access::{
    Prefault, copy_from_user, copy_to_user, copy_to_user_with, validate_user_range,
};
use akuma_exec::threading::{WakeHandle, wake_by_handle, wake_handle_for_thread};
use akuma_primitives::clock::uptime_us;
use akuma_primitives::errno::negated::{E2BIG, EAGAIN, EEXIST, EFAULT, EINVAL, ENOENT, ENOMSG};
use akuma_primitives::irq::with_irqs_disabled;

/// Whether to print the per-call `[msg*]` traces.
///
/// `src/config.rs` stays the single source of truth; `src/syscall/msgqueue.rs`
/// is a shim that reads `SYSCALL_DEBUG_INFO_ENABLED` and calls [`init`].
static DEBUG_INFO: AtomicBool = AtomicBool::new(false);

/// Install the trace toggle. Called once from `kernel_main`.
pub fn init(debug_info: bool) {
    DEBUG_INFO.store(debug_info, Ordering::Relaxed);
}

fn debug_info() -> bool {
    DEBUG_INFO.load(Ordering::Relaxed)
}

/// Validate a user pointer range, faulting lazy pages in.
///
/// The same thin forwarder `src/syscall/mod.rs` keeps for its own arms:
/// `Prefault::Yes` is the right default for anything on a syscall stack, and the
/// range tests plus the demand-paging half live in `akuma-exec` so the copy
/// helpers can fold them in and stop being skippable
/// (`docs/archive/UNSAFE_AUDIT.md` §4 P0).
fn validate_user_ptr(ptr: u64, len: usize) -> bool {
    validate_user_range(ptr, len, Prefault::Yes)
}

const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: i32 = 0o1000;
const IPC_EXCL: i32 = 0o2000;
const IPC_RMID: i32 = 0;
const IPC_SET: i32 = 1;
const IPC_STAT: i32 = 2;
const IPC_NOWAIT: i32 = 0o4000;
const MSG_NOERROR: i32 = 0o10000;
const MSGMAX: usize = 8192;
const MSGMNB: usize = 16384;

struct KernelMsg {
    mtype: i64,
    data: alloc::vec::Vec<u8>,
}

struct MsgQueue {
    key: i32,
    mode: u32,
    cbytes: usize,
    messages: VecDeque<KernelMsg>,
    /// Threads waiting to receive a message
    recv_pollers: PollerMap<usize, WakeHandle>,
    /// Threads waiting to send (queue full)
    send_pollers: PollerMap<usize, WakeHandle>,
}

// Keyed by (box_id, msqid). SysV message queues use integer keys visible to any
// process, so they must be scoped per box — otherwise a process in one container
// could open a queue belonging to another container by guessing the key.
// msqids are still allocated from a global atomic so they are unique across all
// boxes; the box_id in the tuple provides the isolation boundary.
static MSGQUEUE_TABLE: Spinlock<BTreeMap<(u64, u32), MsgQueue>> = Spinlock::new(BTreeMap::new());
// Global counter — msqids only need to be unique within a box (the table key is
// (box_id, msqid)), but a single atomic is simpler and the 32-bit space is large
// enough that cross-box "waste" is not a concern in practice.
static NEXT_MSQID: AtomicU32 = AtomicU32::new(1);

fn current_box_id() -> u64 {
    akuma_exec::process::current_process_shared().map_or(0, |p| p.box_id)
}

#[must_use]
pub fn sys_msgget(key: i32, flags: i32) -> u64 {
    let box_id = current_box_id();
    with_irqs_disabled(|| {
        let mut table = MSGQUEUE_TABLE.lock();
        if key == IPC_PRIVATE {
            let msqid = NEXT_MSQID.fetch_add(1, Ordering::SeqCst);
            let mode = (flags & 0o777) as u32;
            table.insert((box_id, msqid), MsgQueue { key, mode, cbytes: 0, messages: VecDeque::new(), recv_pollers: PollerMap::new(), send_pollers: PollerMap::new() });
            if debug_info() {
                akuma_primitives::tprint!(96, "[msgget] box={} IPC_PRIVATE -> msqid={}\n", box_id, msqid);
            }
            u64::from(msqid)
        } else {
            let found = table.iter()
                .find(|((bid, _), q)| *bid == box_id && q.key == key)
                .map(|((_, msqid), _)| *msqid);
            if let Some(msqid) = found {
                if flags & IPC_EXCL != 0 {
                    return EEXIST;
                }
                if debug_info() {
                    akuma_primitives::tprint!(96, "[msgget] box={} key={} found msqid={}\n", box_id, key, msqid);
                }
                u64::from(msqid)
            } else if flags & IPC_CREAT != 0 {
                let msqid = NEXT_MSQID.fetch_add(1, Ordering::SeqCst);
                let mode = (flags & 0o777) as u32;
                table.insert((box_id, msqid), MsgQueue { key, mode, cbytes: 0, messages: VecDeque::new(), recv_pollers: PollerMap::new(), send_pollers: PollerMap::new() });
                if debug_info() {
                    akuma_primitives::tprint!(96, "[msgget] box={} IPC_CREAT key={} -> msqid={}\n", box_id, key, msqid);
                }
                u64::from(msqid)
            } else {
                ENOENT
            }
        }
    })
}

#[must_use]
pub fn sys_msgctl(msqid: u32, cmd: i32, buf: u64) -> u64 {
    let box_id = current_box_id();
    match cmd {
        IPC_RMID => {
            let pollers_to_wake = with_irqs_disabled(|| {
                let mut table = MSGQUEUE_TABLE.lock();
                let mut handles = alloc::vec::Vec::new();
                if let Some(q) = table.get_mut(&(box_id, msqid)) {
                    for (_, h) in q.recv_pollers.iter().chain(q.send_pollers.iter()) {
                        handles.push(*h);
                    }
                }
                table.remove(&(box_id, msqid));
                handles
            });
            for handle in pollers_to_wake {
                wake_by_handle(handle);
            }
            if debug_info() {
                akuma_primitives::tprint!(96, "[msgctl] box={} IPC_RMID msqid={}\n", box_id, msqid);
            }
            0
        }
        IPC_STAT => {
            let (key, mode, cbytes, qnum) = with_irqs_disabled(|| {
                let table = MSGQUEUE_TABLE.lock();
                if let Some(q) = table.get(&(box_id, msqid)) {
                    (q.key, q.mode, q.cbytes, q.messages.len())
                } else {
                    (0i32, 0u32, 0usize, 0usize)
                }
            });
            // msqid_ds layout (112 bytes total)
            let mut ds = [0u8; 112];
            // ipc_perm.key (i32 at offset 0)
            ds[0..4].copy_from_slice(&key.to_ne_bytes());
            // ipc_perm.mode (u16 at offset 20)
            let mode16 = mode as u16;
            ds[20..22].copy_from_slice(&mode16.to_ne_bytes());
            // msg_cbytes (u64 at offset 72)
            ds[72..80].copy_from_slice(&(cbytes as u64).to_ne_bytes());
            // msg_qnum (u64 at offset 80)
            ds[80..88].copy_from_slice(&(qnum as u64).to_ne_bytes());
            // msg_qbytes (u64 at offset 88)
            ds[88..96].copy_from_slice(&(MSGMNB as u64).to_ne_bytes());
            if copy_to_user(buf, &ds).is_err() {
                return EFAULT;
            }
            0
        }
        IPC_SET => {
            let mut ds = [0u8; 112];
            if copy_from_user(&mut ds, buf).is_err() {
                return EFAULT;
            }
            let mode = u32::from(u16::from_ne_bytes([ds[20], ds[21]]));
            with_irqs_disabled(|| {
                let mut table = MSGQUEUE_TABLE.lock();
                if let Some(q) = table.get_mut(&(box_id, msqid)) {
                    q.mode = mode;
                    0u64
                } else {
                    EINVAL
                }
            })
        }
        _ => EINVAL,
    }
}

#[must_use]
pub fn sys_msgsnd(msqid: u32, msgp: u64, msgsz: usize, flags: i32) -> u64 {
    let box_id = current_box_id();
    if msgsz > MSGMAX {
        return EINVAL;
    }
    if !validate_user_ptr(msgp, 8 + msgsz) {
        return EFAULT;
    }
    let mut mtype_bytes = [0u8; 8];
    if copy_from_user(&mut mtype_bytes, msgp).is_err() {
        return EFAULT;
    }
    let mtype = i64::from_ne_bytes(mtype_bytes);
    if mtype <= 0 {
        return EINVAL;
    }
    let mut data = alloc::vec![0u8; msgsz];
    if msgsz > 0 && copy_from_user(&mut data, msgp + 8).is_err() {
        return EFAULT;
    }
    let tid = akuma_exec::threading::current_thread_id();
    loop {
        let result = with_irqs_disabled(|| {
            let mut table = MSGQUEUE_TABLE.lock();
            let q = match table.get_mut(&(box_id, msqid)) {
                Some(q) => q,
                None => return (Some(EINVAL), alloc::vec::Vec::new()),
            };
            if q.cbytes + msgsz > MSGMNB {
                if flags & IPC_NOWAIT != 0 {
                    return (Some(EAGAIN), alloc::vec::Vec::new());
                }
                // Atomically register as poller before releasing lock (TOCTOU prevention)
                q.send_pollers.insert(tid, wake_handle_for_thread(tid));
                return (None, alloc::vec::Vec::new()); // need to retry
            }
            q.cbytes += msgsz;
            q.messages.push_back(KernelMsg { mtype, data: data.clone() });
            // Wake all threads waiting to receive
            let recv_handles: alloc::vec::Vec<WakeHandle> = q.recv_pollers.values().copied().collect();
            q.recv_pollers.clear();
            (Some(0u64), recv_handles)
        });
        if let (Some(r), handles) = result {
            for handle in handles {
                wake_by_handle(handle);
            }
            return r;
        }
        let deadline = uptime_us() + 10_000;
        akuma_exec::threading::schedule_blocking(deadline);
    }
}

#[must_use]
pub fn sys_msgrcv(msqid: u32, msgp: u64, msgsz: usize, msgtyp: i64, flags: i32) -> u64 {
    let box_id = current_box_id();
    let tid = akuma_exec::threading::current_thread_id();
    if !validate_user_ptr(msgp, 8 + msgsz) {
        return EFAULT;
    }
    loop {
        let result = with_irqs_disabled(|| {
            let mut table = MSGQUEUE_TABLE.lock();
            let q = match table.get_mut(&(box_id, msqid)) {
                Some(q) => q,
                None => return (Some(EINVAL), alloc::vec::Vec::new()),
            };
            // find matching message index
            let idx = match msgtyp.cmp(&0) {
                core::cmp::Ordering::Equal => {
                    if q.messages.is_empty() { None } else { Some(0) }
                }
                core::cmp::Ordering::Greater => {
                    q.messages.iter().position(|m| m.mtype == msgtyp)
                }
                core::cmp::Ordering::Less => {
                    // first message with lowest mtype <= |msgtyp|
                    let abs_typ = -msgtyp;
                    let mut best: Option<(usize, i64)> = None;
                    for (i, m) in q.messages.iter().enumerate() {
                        if m.mtype <= abs_typ
                            && (best.is_none() || m.mtype < best.unwrap().1) {
                                best = Some((i, m.mtype));
                            }
                    }
                    best.map(|(i, _)| i)
                }
            };
            let idx = if let Some(i) = idx { i } else {
                if flags & IPC_NOWAIT != 0 {
                    return (Some(ENOMSG), alloc::vec::Vec::new());
                }
                // Atomically register as poller before releasing lock (TOCTOU prevention)
                q.recv_pollers.insert(tid, wake_handle_for_thread(tid));
                return (None, alloc::vec::Vec::new()); // retry
            };
            let msg = q.messages.remove(idx).unwrap();
            let actual_len = msg.data.len();
            if actual_len > msgsz {
                if flags & MSG_NOERROR == 0 {
                    // put it back
                    q.messages.insert(idx, msg);
                    return (Some(E2BIG), alloc::vec::Vec::new());
                }
                // truncate: copy msgsz bytes
                let mtype_bytes = msg.mtype.to_ne_bytes();
                if copy_to_user_with(msgp, &mtype_bytes, Prefault::No).is_err() {
                    return (Some(EFAULT), alloc::vec::Vec::new());
                }
                if msgsz > 0
                    && copy_to_user_with(msgp + 8, &msg.data[..msgsz], Prefault::No).is_err()
                {
                    return (Some(EFAULT), alloc::vec::Vec::new());
                }
                q.cbytes -= actual_len;
                // Wake senders waiting for space
                let send_handles: alloc::vec::Vec<WakeHandle> = q.send_pollers.values().copied().collect();
                q.send_pollers.clear();
                return (Some(msgsz as u64), send_handles);
            }
            q.cbytes -= actual_len;
            let mtype_bytes = msg.mtype.to_ne_bytes();
            if copy_to_user_with(msgp, &mtype_bytes, Prefault::No).is_err() {
                return (Some(EFAULT), alloc::vec::Vec::new());
            }
            if actual_len > 0
                && copy_to_user_with(msgp + 8, &msg.data[..actual_len], Prefault::No).is_err()
            {
                return (Some(EFAULT), alloc::vec::Vec::new());
            }
            // Wake senders waiting for space
            let send_handles: alloc::vec::Vec<WakeHandle> = q.send_pollers.values().copied().collect();
            q.send_pollers.clear();
            (Some(actual_len as u64), send_handles)
        });
        if let (Some(r), handles) = result {
            for handle in handles {
                wake_by_handle(handle);
            }
            return r;
        }
        let deadline = uptime_us() + 10_000;
        akuma_exec::threading::schedule_blocking(deadline);
    }
}

/// Register a thread as interested in receiving from this queue (for epoll/poll).
#[allow(dead_code)]
pub fn msgqueue_add_recv_poller(box_id: u64, msqid: u32, tid: usize) {
    with_irqs_disabled(|| {
        let mut table = MSGQUEUE_TABLE.lock();
        if let Some(q) = table.get_mut(&(box_id, msqid)) {
            q.recv_pollers.insert(tid, wake_handle_for_thread(tid));
        }
    });
}

/// Register a thread as interested in sending to this queue (for epoll/poll).
#[allow(dead_code)]
pub fn msgqueue_add_send_poller(box_id: u64, msqid: u32, tid: usize) {
    with_irqs_disabled(|| {
        let mut table = MSGQUEUE_TABLE.lock();
        if let Some(q) = table.get_mut(&(box_id, msqid)) {
            q.send_pollers.insert(tid, wake_handle_for_thread(tid));
        }
    });
}

pub struct MsgQueueSnapshot {
    pub box_id: u64,
    pub key: i32,
    pub msqid: u32,
    pub mode: u32,
    pub cbytes: usize,
    pub qnum: usize,
}

#[must_use]
pub fn list_msg_queues() -> Vec<MsgQueueSnapshot> {
    with_irqs_disabled(|| {
        MSGQUEUE_TABLE.lock().iter()
            .map(|((box_id, msqid), q)| MsgQueueSnapshot {
                box_id: *box_id,
                key: q.key,
                msqid: *msqid,
                mode: q.mode,
                cbytes: q.cbytes,
                qnum: q.messages.len(),
            })
            .collect()
    })
}

// ============================================================================
// Test helpers
// ============================================================================

/// Test helper: return the number of recv pollers registered on a queue.
#[allow(dead_code)]
#[must_use]
pub fn msgqueue_recv_pollers_count(box_id: u64, msqid: u32) -> usize {
    with_irqs_disabled(|| {
        MSGQUEUE_TABLE.lock().get(&(box_id, msqid)).map_or(0, |q| q.recv_pollers.len())
    })
}

/// Test helper: return the number of send pollers registered on a queue.
#[allow(dead_code)]
#[must_use]
pub fn msgqueue_send_pollers_count(box_id: u64, msqid: u32) -> usize {
    with_irqs_disabled(|| {
        MSGQUEUE_TABLE.lock().get(&(box_id, msqid)).map_or(0, |q| q.send_pollers.len())
    })
}

/// Test helper: check if a specific tid is registered as a recv poller.
#[allow(dead_code)]
#[must_use]
pub fn msgqueue_is_recv_poller(box_id: u64, msqid: u32, tid: usize) -> bool {
    with_irqs_disabled(|| {
        MSGQUEUE_TABLE.lock().get(&(box_id, msqid)).is_some_and(|q| q.recv_pollers.contains_key(&tid))
    })
}

/// Test helper: directly push a message into a queue (bypasses userspace pointer validation).
#[allow(dead_code)]
#[must_use]
pub fn msgqueue_push_direct(box_id: u64, msqid: u32, mtype: i64, data: &[u8]) -> bool {
    with_irqs_disabled(|| {
        let mut table = MSGQUEUE_TABLE.lock();
        if let Some(q) = table.get_mut(&(box_id, msqid)) {
            let msg_len = data.len();
            q.messages.push_back(KernelMsg { mtype, data: data.to_vec() });
            q.cbytes += msg_len;
            // Wake recv pollers
            let handles: alloc::vec::Vec<WakeHandle> = q.recv_pollers.values().copied().collect();
            q.recv_pollers.clear();
            drop(table);
            for handle in handles {
                wake_by_handle(handle);
            }
            true
        } else {
            false
        }
    })
}

/// Test helper: pop a message from a queue (bypasses userspace pointer validation).
#[allow(dead_code)]
#[must_use]
pub fn msgqueue_pop_direct(box_id: u64, msqid: u32) -> Option<(i64, alloc::vec::Vec<u8>)> {
    with_irqs_disabled(|| {
        let mut table = MSGQUEUE_TABLE.lock();
        if let Some(q) = table.get_mut(&(box_id, msqid)) {
            if let Some(msg) = q.messages.pop_front() {
                q.cbytes -= msg.data.len();
                // Wake send pollers (space freed)
                let handles: alloc::vec::Vec<WakeHandle> = q.send_pollers.values().copied().collect();
                q.send_pollers.clear();
                drop(table);
                for handle in handles {
                    wake_by_handle(handle);
                }
                Some((msg.mtype, msg.data))
            } else {
                None
            }
        } else {
            None
        }
    })
}

/// Test helper: return the number of messages in the queue.
#[allow(dead_code)]
#[must_use]
pub fn msgqueue_message_count(box_id: u64, msqid: u32) -> usize {
    with_irqs_disabled(|| {
        MSGQUEUE_TABLE.lock().get(&(box_id, msqid)).map_or(0, |q| q.messages.len())
    })
}

/// Called from sys_kill_box to remove all queues belonging to a box.
#[allow(dead_code)]
pub fn cleanup_box_queues(box_id: u64) {
    with_irqs_disabled(|| {
        let mut table = MSGQUEUE_TABLE.lock();
        table.retain(|(bid, _), _| *bid != box_id);
    });
}
