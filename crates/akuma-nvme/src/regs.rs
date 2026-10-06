//! Controller registers (NVMe base spec §3.1): offsets, `CAP` decode, the `CC`
//! value this driver enables with, `CSTS`, `AQA` and doorbell placement.

/// Controller Capabilities (64-bit).
pub const CAP: usize = 0x00;
/// Version.
pub const VS: usize = 0x08;
/// Interrupt Mask Set — written all-ones: this driver polls.
pub const INTMS: usize = 0x0C;
/// Controller Configuration.
pub const CC: usize = 0x14;
/// Controller Status.
pub const CSTS: usize = 0x1C;
/// Admin Queue Attributes.
pub const AQA: usize = 0x24;
/// Admin Submission Queue base (64-bit).
pub const ASQ: usize = 0x28;
/// Admin Completion Queue base (64-bit).
pub const ACQ: usize = 0x30;
/// First doorbell.
pub const DOORBELL_BASE: usize = 0x1000;

/// `CAP`, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cap {
    /// Maximum queue entries, **0-based** as the register holds it.
    pub mqes: u16,
    /// Queues must be physically contiguous.
    pub cqr: bool,
    /// Worst-case `CSTS.RDY` transition time, in 500 ms units.
    pub to: u8,
    /// Doorbell stride: doorbells are `4 << dstrd` bytes apart.
    pub dstrd: u8,
    /// The NVM command set is supported (`CSS` bit 0).
    pub css_nvm: bool,
    /// Minimum memory page size, as `2^(12 + mpsmin)`.
    pub mpsmin: u8,
    /// Maximum memory page size, as `2^(12 + mpsmax)`.
    pub mpsmax: u8,
}

impl Cap {
    #[must_use]
    pub const fn decode(v: u64) -> Self {
        Self {
            mqes: (v & 0xffff) as u16,
            cqr: (v >> 16) & 1 != 0,
            to: ((v >> 24) & 0xff) as u8,
            dstrd: ((v >> 32) & 0xf) as u8,
            css_nvm: (v >> 37) & 1 != 0,
            mpsmin: ((v >> 48) & 0xf) as u8,
            mpsmax: ((v >> 52) & 0xf) as u8,
        }
    }

    /// Entries a queue may hold (1-based).
    #[must_use]
    pub const fn max_queue_entries(self) -> u32 {
        self.mqes as u32 + 1
    }

    /// `CAP.TO` in milliseconds. A controller reporting 0 gets 500 ms rather
    /// than none: a zero budget would read every controller as dead.
    #[must_use]
    pub const fn ready_timeout_ms(self) -> u64 {
        if self.to == 0 { 500 } else { self.to as u64 * 500 }
    }

    /// Bytes between consecutive doorbells.
    #[must_use]
    pub const fn doorbell_stride(self) -> usize {
        4 << self.dstrd
    }

    /// 4 KiB pages — the only page size this driver programs — are allowed.
    #[must_use]
    pub const fn supports_4k_pages(self) -> bool {
        self.mpsmin == 0
    }

    /// Submission queue `qid`'s tail doorbell, as a BAR offset.
    #[must_use]
    pub const fn sq_tail_doorbell(self, qid: u16) -> usize {
        DOORBELL_BASE + (2 * qid as usize) * self.doorbell_stride()
    }

    /// Completion queue `qid`'s head doorbell, as a BAR offset.
    #[must_use]
    pub const fn cq_head_doorbell(self, qid: u16) -> usize {
        DOORBELL_BASE + (2 * qid as usize + 1) * self.doorbell_stride()
    }
}

/// `CC.EN`.
pub const CC_EN: u32 = 1;
/// `CC.SHN` mask, and its "normal shutdown" value.
pub const CC_SHN_MASK: u32 = 0b11 << 14;
pub const CC_SHN_NORMAL: u32 = 0b01 << 14;

/// The `CC` this driver enables the controller with: NVM command set, 4 KiB
/// pages, round-robin arbitration, 64-byte SQ entries (`IOSQES = 6`) and
/// 16-byte CQ entries (`IOCQES = 4`).
#[must_use]
pub const fn cc_enable() -> u32 {
    CC_EN | (6 << 16) | (4 << 20)
}

/// `CSTS`, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Csts {
    pub ready: bool,
    /// Controller Fatal Status: nothing more will complete.
    pub fatal: bool,
    /// `SHST`: 0 normal, 1 shutdown in progress, 2 shutdown complete.
    pub shutdown: u8,
}

impl Csts {
    #[must_use]
    pub const fn decode(v: u32) -> Self {
        Self { ready: v & 1 != 0, fatal: v & 2 != 0, shutdown: ((v >> 2) & 3) as u8 }
    }

    /// An all-ones read is a device that has gone (surprise removal, or a BAR
    /// that no longer decodes) — never a real status.
    #[must_use]
    pub const fn is_absent(raw: u32) -> bool {
        raw == u32::MAX
    }
}

/// `CSTS.SHST` "shutdown processing complete".
pub const SHST_COMPLETE: u8 = 2;

/// `AQA` for admin queues of the given sizes (1-based, at most 4096 each).
#[must_use]
pub const fn aqa(sq_entries: u16, cq_entries: u16) -> u32 {
    ((cq_entries as u32 - 1) << 16) | (sq_entries as u32 - 1)
}
