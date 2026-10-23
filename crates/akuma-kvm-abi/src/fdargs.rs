//! Arguments of the requests that carry file descriptors or device flags.

use crate::{rd32, rd64};

pub const IRQFD_LEN: usize = 32;
pub const IOEVENTFD_LEN: usize = 64;

/// `IRQFD`: when the eventfd is signalled, raise `gsi`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Irqfd {
    pub fd: u32,
    pub gsi: u32,
    pub flags: u32,
    pub resample_fd: u32,
}

impl Irqfd {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        Some(Self { fd: rd32(b, 0)?, gsi: rd32(b, 4)?, flags: rd32(b, 8)?, resample_fd: rd32(b, 12)? })
    }
}

/// `IOEVENTFD`: a write of `len` bytes to `addr` (matching `datamatch` when flag bit 0 is set) signals `fd`.
/// Observed: `datamatch 0, addr 0xc0001050 (a virtio-mmio queue-notify register), len 4, flags 1`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ioeventfd {
    pub datamatch: u64,
    pub addr: u64,
    pub len: u32,
    pub fd: i32,
    pub flags: u32,
}

pub const IOEVENTFD_DATAMATCH: u32 = 1;

impl Ioeventfd {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        Some(Self {
            datamatch: rd64(b, 0)?,
            addr: rd64(b, 8)?,
            len: rd32(b, 16)?,
            fd: rd32(b, 20)?.cast_signed(),
            flags: rd32(b, 24)?,
        })
    }

    /// Does a write of `len` bytes of `data` to `addr` fire this event?
    #[must_use]
    pub fn matches(&self, addr: u64, len: u32, data: u64) -> bool {
        self.addr == addr
            && self.len == len
            && (self.flags & IOEVENTFD_DATAMATCH == 0 || self.datamatch == data)
    }
}
