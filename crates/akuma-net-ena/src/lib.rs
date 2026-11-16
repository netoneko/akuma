//! ENA protocol primitives. No MMIO or DMA accesses occur in this crate.
//!
//! Register fields follow AWS's `ena_regs_defs.h`, revision
//! 77facd6719172a5f39ea5c810b126a8b40b7e4fe. See `AWS-LICENSE`.
#![no_std]
#![forbid(unsafe_code)]

/// ENA device register offsets, in bytes.
pub mod regs {
    pub const VERSION: u32 = 0x00;
    pub const CONTROLLER_VERSION: u32 = 0x04;
    pub const CAPS: u32 = 0x08;
    pub const AQ_BASE_LO: u32 = 0x10;
    pub const AQ_BASE_HI: u32 = 0x14;
    pub const AQ_CAPS: u32 = 0x18;
    pub const ACQ_BASE_LO: u32 = 0x20;
    pub const ACQ_BASE_HI: u32 = 0x24;
    pub const ACQ_CAPS: u32 = 0x28;
    pub const AQ_DB: u32 = 0x2c;
    pub const AENQ_CAPS: u32 = 0x34;
    pub const AENQ_BASE_LO: u32 = 0x38;
    pub const AENQ_BASE_HI: u32 = 0x3c;
    pub const AENQ_HEAD_DB: u32 = 0x40;
    pub const INTR_MASK: u32 = 0x4c;
    pub const DEV_CTL: u32 = 0x54;
    pub const DEV_STS: u32 = 0x58;
    pub const DEV_RESET: u32 = 1;
    pub const READY: u32 = 1;
    pub const RESET_IN_PROGRESS: u32 = 1 << 3;
    pub const RESET_FINISHED: u32 = 1 << 4;
    pub const FATAL_ERROR: u32 = 1 << 5;
}

/// Encode the depth and entry size of an admin or event queue.
/// Queue depth must be a nonzero power of two.
pub const fn queue_caps(depth: u16, entry_size: u16) -> Option<u32> {
    if depth == 0 || !depth.is_power_of_two() || entry_size == 0 {
        None
    } else {
        Some(depth as u32 | ((entry_size as u32) << 16))
    }
}

/// Decode the advertised DMA address width from the capabilities register.
pub const fn dma_address_width(caps: u32) -> u8 {
    ((caps >> 8) & 0xff) as u8
}

/// Cursor for ENA rings whose entries carry a phase bit.
///
/// The caller must initialize the backing ring to zero, check the phase before
/// consuming a completion, and synchronize DMA before reading its payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingCursor {
    depth: u16,
    index: u16,
    phase: bool,
}

impl RingCursor {
    pub const fn new(depth: u16) -> Option<Self> {
        if depth == 0 || !depth.is_power_of_two() {
            None
        } else {
            Some(Self { depth, index: 0, phase: true })
        }
    }

    pub const fn index(&self) -> u16 { self.index }

    pub const fn phase(&self) -> bool { self.phase }

    pub fn advance(&mut self) {
        self.index += 1;
        if self.index == self.depth {
            self.index = 0;
            self.phase = !self.phase;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_register_fields_and_invalid_depths() {
        assert_eq!(queue_caps(32, 64), Some(0x0040_0020));
        assert_eq!(queue_caps(0, 64), None);
        assert_eq!(queue_caps(3, 64), None);
        assert_eq!(queue_caps(32, 0), None);
    }

    #[test]
    fn dma_width_extracts_only_its_field() {
        assert_eq!(dma_address_width(0xabcd_30ef), 48);
    }

    #[test]
    fn ring_phase_changes_only_on_wrap() {
        assert_eq!(RingCursor::new(3), None);
        let mut cursor = RingCursor::new(2).unwrap();
        assert_eq!((cursor.index(), cursor.phase()), (0, true));
        cursor.advance();
        assert_eq!((cursor.index(), cursor.phase()), (1, true));
        cursor.advance();
        assert_eq!((cursor.index(), cursor.phase()), (0, false));
        cursor.advance();
        cursor.advance();
        assert_eq!((cursor.index(), cursor.phase()), (0, true));
    }
}
