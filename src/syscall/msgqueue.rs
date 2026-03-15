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

static MSGQUEUE_TABLE: Spinlock<BTreeMap<u32, MsgQueue>> = Spinlock::new(BTreeMap::new());
static NEXT_MSQID: AtomicU32 = AtomicU32::new(1);

pub(super) fn sys_msgget(key: i32, flags: i32) -> u64 {
    crate::irq::with_irqs_disabled(|| {
        let mut table = MSGQUEUE_TABLE.lock();
        if key == IPC_PRIVATE {
            let msqid = NEXT_MSQID.fetch_add(1, Ordering::SeqCst);
            let mode = (flags & 0o777) as u32;
            table.insert(msqid, MsgQueue { key, mode, cbytes: 0, messages: VecDeque::new() });
            crate::tprint!(96, "[msgget] IPC_PRIVATE -> msqid={}\n", msqid);
            msqid as u64
        } else {
            let found = table.iter().find(|(_, q)| q.key == key).map(|(id, _)| *id);
            if let Some(msqid) = found {
                if flags & IPC_EXCL != 0 {
                    return EEXIST;
                }
                crate::tprint!(96, "[msgget] key={} found msqid={}\n", key, msqid);
                msqid as u64
            } else if flags & IPC_CREAT != 0 {
                let msqid = NEXT_MSQID.fetch_add(1, Ordering::SeqCst);
                let mode = (flags & 0o777) as u32;
                table.insert(msqid, MsgQueue { key, mode, cbytes: 0, messages: VecDeque::new() });
                crate::tprint!(96, "[msgget] IPC_CREAT key={} -> msqid={}\n", key, msqid);
                msqid as u64
            } else {
                ENOENT
            }
        }
    })
}

pub(super) fn sys_msgctl(msqid: u32, cmd: i32, buf: u64) -> u64 {
    match cmd {
        IPC_RMID => {
            crate::irq::with_irqs_disabled(|| {
                MSGQUEUE_TABLE.lock().remove(&msqid);
            });
            crate::tprint!(96, "[msgctl] IPC_RMID msqid={}\n", msqid);
            0
        }
        IPC_STAT => {
            if !validate_user_ptr(buf, 112) {
                return EFAULT;
            }
            let (key, mode, cbytes, qnum) = crate::irq::with_irqs_disabled(|| {
                let table = MSGQUEUE_TABLE.lock();
                if let Some(q) = table.get(&msqid) {
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
                if let Some(q) = table.get_mut(&msqid) {
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
            let q = match table.get_mut(&msqid) {
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
    if !validate_user_ptr(msgp, 8 + msgsz) {
        return EFAULT;
    }
    loop {
        let result = crate::irq::with_irqs_disabled(|| {
            let mut table = MSGQUEUE_TABLE.lock();
            let q = match table.get_mut(&msqid) {
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
