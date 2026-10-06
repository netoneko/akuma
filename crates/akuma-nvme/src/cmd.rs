//! Submission entries (64 bytes, as sixteen dwords) and completion entries
//! (16 bytes), plus the queue bookkeeping a polled driver needs.

/// Bytes per submission entry (`CC.IOSQES = 6`).
pub const SQE_BYTES: usize = 64;
/// Bytes per completion entry (`CC.IOCQES = 4`).
pub const CQE_BYTES: usize = 16;

/// A submission queue entry, dword by dword.
pub type Sqe = [u32; 16];

/// Admin command opcodes this driver issues.
pub mod admin {
    pub const DELETE_IO_SQ: u8 = 0x00;
    pub const CREATE_IO_SQ: u8 = 0x01;
    pub const DELETE_IO_CQ: u8 = 0x04;
    pub const CREATE_IO_CQ: u8 = 0x05;
    pub const IDENTIFY: u8 = 0x06;
}

/// NVM I/O command opcodes this driver issues.
pub mod io {
    pub const FLUSH: u8 = 0x00;
    pub const WRITE: u8 = 0x01;
    pub const READ: u8 = 0x02;
}

/// Identify `CNS` values.
pub mod cns {
    pub const NAMESPACE: u32 = 0x00;
    pub const CONTROLLER: u32 = 0x01;
}

/// The common header: opcode, command id, namespace, PRP1/PRP2.
#[must_use]
pub const fn sqe(opcode: u8, cid: u16, nsid: u32, prp1: u64, prp2: u64) -> Sqe {
    let mut e = [0u32; 16];
    e[0] = opcode as u32 | ((cid as u32) << 16); // FUSE = 0, PSDT = 0 (PRPs)
    e[1] = nsid;
    e[6] = prp1 as u32;
    e[7] = (prp1 >> 32) as u32;
    e[8] = prp2 as u32;
    e[9] = (prp2 >> 32) as u32;
    e
}

/// Identify, into one 4 KiB page at `prp1`.
#[must_use]
pub const fn identify(cid: u16, cns: u32, nsid: u32, prp1: u64) -> Sqe {
    let mut e = sqe(admin::IDENTIFY, cid, nsid, prp1, 0);
    e[10] = cns;
    e
}

/// Create I/O Completion Queue `qid` of `entries` (1-based), physically
/// contiguous at `base`, **interrupts disabled** (this driver polls).
#[must_use]
pub const fn create_io_cq(cid: u16, qid: u16, entries: u16, base: u64) -> Sqe {
    let mut e = sqe(admin::CREATE_IO_CQ, cid, 0, base, 0);
    e[10] = ((entries as u32 - 1) << 16) | qid as u32;
    e[11] = 1; // PC = 1, IEN = 0
    e
}

/// Create I/O Submission Queue `qid` of `entries` (1-based) at `base`,
/// completing to `cqid`.
#[must_use]
pub const fn create_io_sq(cid: u16, qid: u16, entries: u16, base: u64, cqid: u16) -> Sqe {
    let mut e = sqe(admin::CREATE_IO_SQ, cid, 0, base, 0);
    e[10] = ((entries as u32 - 1) << 16) | qid as u32;
    e[11] = ((cqid as u32) << 16) | 1; // PC = 1, QPRIO = urgent/ignored under RR
    e
}

/// Delete I/O Submission Queue `qid`.
#[must_use]
pub const fn delete_io_sq(cid: u16, qid: u16) -> Sqe {
    let mut e = sqe(admin::DELETE_IO_SQ, cid, 0, 0, 0);
    e[10] = qid as u32;
    e
}

/// Delete I/O Completion Queue `qid`.
#[must_use]
pub const fn delete_io_cq(cid: u16, qid: u16) -> Sqe {
    let mut e = sqe(admin::DELETE_IO_CQ, cid, 0, 0, 0);
    e[10] = qid as u32;
    e
}

/// Read or Write `blocks` (1..=65536) logical blocks starting at `slba`.
#[must_use]
pub const fn rw(write: bool, cid: u16, nsid: u32, slba: u64, blocks: u32, prp1: u64, prp2: u64) -> Sqe {
    let op = if write { io::WRITE } else { io::READ };
    let mut e = sqe(op, cid, nsid, prp1, prp2);
    e[10] = slba as u32;
    e[11] = (slba >> 32) as u32;
    e[12] = (blocks - 1) & 0xffff; // NLB is 0-based; FUA = 0, LR = 0
    e
}

/// Flush the namespace's volatile write cache.
#[must_use]
pub const fn flush(cid: u16, nsid: u32) -> Sqe {
    sqe(io::FLUSH, cid, nsid, 0, 0)
}

/// A completion's status field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    /// Status Code Type: 0 generic, 1 command specific, 2 media/data integrity.
    pub sct: u8,
    /// Status Code.
    pub sc: u8,
    /// Do Not Retry.
    pub dnr: bool,
}

impl Status {
    #[must_use]
    pub const fn ok(self) -> bool {
        self.sct == 0 && self.sc == 0
    }
}

/// A completion entry, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cqe {
    pub result: u32,
    pub sq_head: u16,
    pub sq_id: u16,
    pub cid: u16,
    pub phase: bool,
    pub status: Status,
}

impl Cqe {
    #[must_use]
    pub const fn decode(d: [u32; 4]) -> Self {
        let s = d[3] >> 17;
        Self {
            result: d[0],
            sq_head: d[2] as u16,
            sq_id: (d[2] >> 16) as u16,
            cid: d[3] as u16,
            phase: (d[3] >> 16) & 1 != 0,
            status: Status { sc: (s & 0xff) as u8, sct: ((s >> 8) & 0x7) as u8, dnr: (s >> 14) & 1 != 0 },
        }
    }
}

/// A submission queue's tail.
#[derive(Clone, Copy, Debug)]
pub struct SqTail {
    tail: u16,
    entries: u16,
}

impl SqTail {
    #[must_use]
    pub const fn new(entries: u16) -> Self {
        Self { tail: 0, entries }
    }

    /// The slot the next entry goes in.
    #[must_use]
    pub const fn slot(self) -> u16 {
        self.tail
    }

    /// Step past the slot just filled; returns the value for the doorbell.
    pub fn advance(&mut self) -> u16 {
        self.tail = if self.tail + 1 == self.entries { 0 } else { self.tail + 1 };
        self.tail
    }
}

/// A completion queue's head and the phase a *new* entry carries.
///
/// The controller inverts the phase bit it writes each time it wraps, and
/// memory starts zeroed, so on the first pass a new entry has phase 1. An entry
/// whose phase differs from [`CqHead::phase`] is stale — last lap's, or never
/// written.
#[derive(Clone, Copy, Debug)]
pub struct CqHead {
    head: u16,
    entries: u16,
    phase: bool,
}

impl CqHead {
    #[must_use]
    pub const fn new(entries: u16) -> Self {
        Self { head: 0, entries, phase: true }
    }

    #[must_use]
    pub const fn slot(self) -> u16 {
        self.head
    }

    #[must_use]
    pub const fn phase(self) -> bool {
        self.phase
    }

    /// Is `cqe` (read from [`CqHead::slot`]) a new completion?
    #[must_use]
    pub const fn is_new(self, cqe: &Cqe) -> bool {
        cqe.phase == self.phase
    }

    /// Consume the entry at the head; returns the value for the head doorbell.
    pub fn advance(&mut self) -> u16 {
        self.head += 1;
        if self.head == self.entries {
            self.head = 0;
            self.phase = !self.phase;
        }
        self.head
    }
}
