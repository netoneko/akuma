use super::*;
use alloc::collections::VecDeque;
use akuma_exec::mmu::user_access::{copy_from_user_safe, copy_to_user_safe};

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

const ENOMSG: u64 = (-42i64) as u64;
const E2BIG: u64 = (-7i64) as u64;

struct KernelMsg {
    mtype: i64,
    data: alloc::vec::Vec<u8>,
}

struct MsgQueue {
    key: i32,
    mode: u32,
    cbytes: usize,
    messages: VecDeque<KernelMsg>,
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
    akuma_exec::process::current_process().map_or(0, |p| p.box_id)
}

pub(super) fn sys_msgget(key: i32, flags: i32) -> u64 {
    let box_id = current_box_id();
    crate::irq::with_irqs_disabled(|| {
        let mut table = MSGQUEUE_TABLE.lock();
        if key == IPC_PRIVATE {
            let msqid = NEXT_MSQID.fetch_add(1, Ordering::SeqCst);
            let mode = (flags & 0o777) as u32;
            table.insert((box_id, msqid), MsgQueue { key, mode, cbytes: 0, messages: VecDeque::new() });
            crate::tprint!(96, "[msgget] box={} IPC_PRIVATE -> msqid={}\n", box_id, msqid);
            msqid as u64
        } else {
            let found = table.iter()
                .find(|((bid, _), q)| *bid == box_id && q.key == key)
                .map(|((_, msqid), _)| *msqid);
            if let Some(msqid) = found {
                if flags & IPC_EXCL != 0 {
                    return EEXIST;
                }
                crate::tprint!(96, "[msgget] box={} key={} found msqid={}\n", box_id, key, msqid);
                msqid as u64
            } else if flags & IPC_CREAT != 0 {
                let msqid = NEXT_MSQID.fetch_add(1, Ordering::SeqCst);
                let mode = (flags & 0o777) as u32;
                table.insert((box_id, msqid), MsgQueue { key, mode, cbytes: 0, messages: VecDeque::new() });
                crate::tprint!(96, "[msgget] box={} IPC_CREAT key={} -> msqid={}\n", box_id, key, msqid);
                msqid as u64
            } else {
                ENOENT
            }
        }
    })
}

pub(super) fn sys_msgctl(msqid: u32, cmd: i32, buf: u64) -> u64 {
    let box_id = current_box_id();
    match cmd {
        IPC_RMID => {
            crate::irq::with_irqs_disabled(|| {
                MSGQUEUE_TABLE.lock().remove(&(box_id, msqid));
            });
            crate::tprint!(96, "[msgctl] box={} IPC_RMID msqid={}\n", box_id, msqid);
            0
        }
        IPC_STAT => {
            if !validate_user_ptr(buf, 112) {
                return EFAULT;
            }
            let (key, mode, cbytes, qnum) = crate::irq::with_irqs_disabled(|| {
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
            if unsafe { copy_to_user_safe(buf as *mut u8, ds.as_ptr(), 112).is_err() } {
                return EFAULT;
            }
            0
        }
        IPC_SET => {
            if !validate_user_ptr(buf, 112) {
                return EFAULT;
            }
            let mut ds = [0u8; 112];
            if unsafe { copy_from_user_safe(ds.as_mut_ptr(), buf as *const u8, 112).is_err() } {
                return EFAULT;
            }
            let mode = u16::from_ne_bytes([ds[20], ds[21]]) as u32;
            crate::irq::with_irqs_disabled(|| {
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

pub(super) fn sys_msgsnd(msqid: u32, msgp: u64, msgsz: usize, flags: i32) -> u64 {
    let box_id = current_box_id();
    if msgsz > MSGMAX {
        return EINVAL;
    }
    if !validate_user_ptr(msgp, 8 + msgsz) {
        return EFAULT;
    }
    let mut mtype_bytes = [0u8; 8];
    if unsafe { copy_from_user_safe(mtype_bytes.as_mut_ptr(), msgp as *const u8, 8).is_err() } {
        return EFAULT;
    }
    let mtype = i64::from_ne_bytes(mtype_bytes);
    if mtype <= 0 {
        return EINVAL;
    }
    let mut data = alloc::vec![0u8; msgsz];
    if msgsz > 0 && unsafe { copy_from_user_safe(data.as_mut_ptr(), (msgp + 8) as *const u8, msgsz).is_err() } {
        return EFAULT;
    }
    loop {
        let result = crate::irq::with_irqs_disabled(|| {
            let mut table = MSGQUEUE_TABLE.lock();
            let q = match table.get_mut(&(box_id, msqid)) {
                Some(q) => q,
                None => return Some(EINVAL),
            };
            if q.cbytes + msgsz > MSGMNB {
                if flags & IPC_NOWAIT != 0 {
                    return Some(EAGAIN);
                }
                return None; // need to retry
            }
            q.cbytes += msgsz;
            q.messages.push_back(KernelMsg { mtype, data: data.clone() });
            Some(0u64)
        });
        match result {
            Some(r) => return r,
            None => akuma_exec::threading::yield_now(),
        }
    }
}

pub(super) fn sys_msgrcv(msqid: u32, msgp: u64, msgsz: usize, msgtyp: i64, flags: i32) -> u64 {
    let box_id = current_box_id();
    if !validate_user_ptr(msgp, 8 + msgsz) {
        return EFAULT;
    }
    loop {
        let result = crate::irq::with_irqs_disabled(|| {
            let mut table = MSGQUEUE_TABLE.lock();
            let q = match table.get_mut(&(box_id, msqid)) {
                Some(q) => q,
                None => return Some(EINVAL),
            };
            // find matching message index
            let idx = if msgtyp == 0 {
                if q.messages.is_empty() { None } else { Some(0) }
            } else if msgtyp > 0 {
                q.messages.iter().position(|m| m.mtype == msgtyp)
            } else {
                // first message with lowest mtype <= |msgtyp|
                let abs_typ = (-msgtyp) as i64;
                let mut best: Option<(usize, i64)> = None;
                for (i, m) in q.messages.iter().enumerate() {
                    if m.mtype <= abs_typ {
                        if best.is_none() || m.mtype < best.unwrap().1 {
                            best = Some((i, m.mtype));
                        }
                    }
                }
                best.map(|(i, _)| i)
            };
            let idx = match idx {
                Some(i) => i,
                None => {
                    if flags & IPC_NOWAIT != 0 {
                        return Some(ENOMSG);
                    }
                    return None; // retry
                }
            };
            let msg = q.messages.remove(idx).unwrap();
            let actual_len = msg.data.len();
            if actual_len > msgsz {
                if flags & MSG_NOERROR == 0 {
                    // put it back
                    q.messages.insert(idx, msg);
                    return Some(E2BIG);
                }
                // truncate: copy msgsz bytes
                let mtype_bytes = msg.mtype.to_ne_bytes();
                unsafe {
                    if copy_to_user_safe(msgp as *mut u8, mtype_bytes.as_ptr(), 8).is_err() {
                        return Some(EFAULT);
                    }
                    if msgsz > 0 && copy_to_user_safe((msgp + 8) as *mut u8, msg.data.as_ptr(), msgsz).is_err() {
                        return Some(EFAULT);
                    }
                }
                q.cbytes -= actual_len;
                return Some(msgsz as u64);
            }
            q.cbytes -= actual_len;
            let mtype_bytes = msg.mtype.to_ne_bytes();
            unsafe {
                if copy_to_user_safe(msgp as *mut u8, mtype_bytes.as_ptr(), 8).is_err() {
                    return Some(EFAULT);
                }
                if actual_len > 0 && copy_to_user_safe((msgp + 8) as *mut u8, msg.data.as_ptr(), actual_len).is_err() {
                    return Some(EFAULT);
                }
            }
            Some(actual_len as u64)
        });
        match result {
            Some(r) => return r,
            None => akuma_exec::threading::yield_now(),
        }
    }
}

pub(crate) struct MsgQueueSnapshot {
    pub box_id: u64,
    pub key: i32,
    pub msqid: u32,
    pub mode: u32,
    pub cbytes: usize,
    pub qnum: usize,
}

pub(crate) fn list_msg_queues() -> Vec<MsgQueueSnapshot> {
    crate::irq::with_irqs_disabled(|| {
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

/// Called from sys_kill_box to remove all queues belonging to a box.
#[allow(dead_code)]
pub(super) fn cleanup_box_queues(box_id: u64) {
    crate::irq::with_irqs_disabled(|| {
        let mut table = MSGQUEUE_TABLE.lock();
        table.retain(|(bid, _), _| *bid != box_id);
    });
}
