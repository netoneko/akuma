//! The vCPU's shared "run page": what `RUN` leaves for the VMM to read.
//!
//! **Provenance: a hypothesis, tested by behaviour.** The census cannot show this layout (only the
//! VMM reads it), so it is written down from general knowledge of the interface and confirmed (or
//! refuted) by whether the unmodified Firecracker acts on it: a serial byte appearing proves the
//! port-I/O fields, a driver answering proves the MMIO ones. The first run that does is recorded in
//! `docs/archive/AKUMA_FIRECRACKER_KVM.md`. The page is 4 KiB of structure; the mapping is 8 KiB.

/// Bytes of the run structure's page.
pub const PAGE: usize = 4096;

/// Offsets into the page.
pub mod off {
    pub const IMMEDIATE_EXIT: usize = 1;
    pub const EXIT_REASON: usize = 8;
    pub const READY_FOR_INTERRUPT_INJECTION: usize = 12;
    pub const IF_FLAG: usize = 13;
    pub const CR8: usize = 16;
    pub const APIC_BASE: usize = 24;
    /// The exit union.
    pub const UNION: usize = 32;
    // io: direction u8, size u8, port u16, count u32, data_offset u64
    pub const IO_DIRECTION: usize = UNION;
    pub const IO_SIZE: usize = UNION + 1;
    pub const IO_PORT: usize = UNION + 2;
    pub const IO_COUNT: usize = UNION + 4;
    pub const IO_DATA_OFFSET: usize = UNION + 8;
    // mmio: phys_addr u64, data [u8; 8], len u32, is_write u8
    pub const MMIO_PHYS_ADDR: usize = UNION;
    pub const MMIO_DATA: usize = UNION + 8;
    pub const MMIO_LEN: usize = UNION + 16;
    pub const MMIO_IS_WRITE: usize = UNION + 20;
}

/// Where a port-I/O exit's data sits in the page (the union ends before this).
pub const IO_DATA: usize = 0x400;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Exit {
    Unknown = 0,
    Io = 2,
    Hlt = 5,
    Mmio = 6,
    Shutdown = 8,
    FailEntry = 9,
    Intr = 10,
    InternalError = 17,
}

pub const IO_IN: u8 = 0;
pub const IO_OUT: u8 = 1;

fn w16(p: &mut [u8], o: usize, v: u16) {
    p[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn w32(p: &mut [u8], o: usize, v: u32) {
    p[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn w64(p: &mut [u8], o: usize, v: u64) {
    p[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

pub fn set_exit(p: &mut [u8; PAGE], e: Exit) {
    w32(p, off::EXIT_REASON, e as u32);
}

/// A port-I/O exit. For an `OUT` the data (`size` bytes of `val`) is placed at [`IO_DATA`]; for an
/// `IN` the VMM writes it there before the next `RUN`.
pub fn set_io(p: &mut [u8; PAGE], dir: u8, size: u8, port: u16, val: u32) {
    set_exit(p, Exit::Io);
    p[off::IO_DIRECTION] = dir;
    p[off::IO_SIZE] = size;
    w16(p, off::IO_PORT, port);
    w32(p, off::IO_COUNT, 1);
    w64(p, off::IO_DATA_OFFSET, IO_DATA as u64);
    p[IO_DATA..IO_DATA + 4].copy_from_slice(&val.to_le_bytes());
}

/// The value the VMM left for an `IN` of `size` bytes.
#[must_use]
pub fn io_in_value(p: &[u8; PAGE], size: u8) -> u32 {
    let mut b = [0u8; 4];
    let n = usize::from(size).min(4);
    b[..n].copy_from_slice(&p[IO_DATA..IO_DATA + n]);
    u32::from_le_bytes(b)
}

/// An MMIO exit: `size` (1, 2, 4 or 8) bytes at `addr`; for a store the value is in the data.
pub fn set_mmio(p: &mut [u8; PAGE], addr: u64, size: u8, is_write: bool, val: u64) {
    set_exit(p, Exit::Mmio);
    w64(p, off::MMIO_PHYS_ADDR, addr);
    p[off::MMIO_DATA..off::MMIO_DATA + 8].copy_from_slice(&val.to_le_bytes());
    w32(p, off::MMIO_LEN, u32::from(size));
    p[off::MMIO_IS_WRITE] = u8::from(is_write);
}

/// The value the VMM left for an MMIO load.
#[must_use]
pub fn mmio_value(p: &[u8; PAGE]) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&p[off::MMIO_DATA..off::MMIO_DATA + 8]);
    u64::from_le_bytes(b)
}

#[must_use]
pub fn immediate_exit(p: &[u8; PAGE]) -> bool {
    p[off::IMMEDIATE_EXIT] != 0
}
