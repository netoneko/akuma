//! Pure types and constants for the MMU subsystem.
//!
//! No architecture-specific dependencies - fully host-testable.

#![allow(dead_code)]

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SHIFT: usize = 12;
pub const ENTRIES_PER_TABLE: usize = 512;
pub const BITS_PER_LEVEL: usize = 9;

pub const DEV_GIC_DIST_VA: usize = 0x80_0000_0000;
pub const DEV_GIC_CPU_VA: usize  = 0x80_0000_1000;
pub const DEV_UART_VA: usize     = 0x80_0000_2000;
pub const DEV_FW_CFG_VA: usize   = 0x80_0000_3000;
pub const DEV_VIRTIO_VA: usize   = 0x80_0000_4000;

pub const MAIR_DEVICE_NGNRNE: u64 = 0;
pub const MAIR_NORMAL_NC: u64 = 1;
pub const MAIR_NORMAL_WT: u64 = 2;
pub const MAIR_NORMAL_WB: u64 = 3;

pub mod flags {
    pub const VALID: u64 = 1 << 0;
    pub const TABLE: u64 = 1 << 1;
    pub const BLOCK: u64 = 0 << 1;
    pub const AF: u64 = 1 << 10;
    pub const SH_INNER: u64 = 3 << 8;
    pub const SH_OUTER: u64 = 2 << 8;
    pub const AP_RW_EL1: u64 = 0 << 6;
    pub const AP_RW_ALL: u64 = 1 << 6;
    pub const AP_RO_EL1: u64 = 2 << 6;
    pub const AP_RO_ALL: u64 = 3 << 6;
    pub const USER: u64 = 1 << 6;
    pub const PXN: u64 = 1 << 53;
    pub const UXN: u64 = 1 << 54;
    pub const NG: u64 = 1 << 11;
}

#[inline]
pub const fn attr_index(idx: u64) -> u64 {
    (idx & 0x7) << 2
}

pub const BLOCK_1GB: usize = 1 << 30;
pub const BLOCK_2MB: usize = 1 << 21;

#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [u64; ENTRIES_PER_TABLE],
}

impl PageTable {
    pub const fn new() -> Self {
        Self { entries: [0; ENTRIES_PER_TABLE] }
    }
}

pub mod user_flags {
    use super::flags;
    pub const RO: u64 = flags::AP_RO_ALL;
    pub const RW: u64 = flags::AP_RW_ALL;
    pub const EXEC: u64 = flags::AP_RO_ALL;
    pub const RW_NO_EXEC: u64 = flags::AP_RW_ALL | flags::UXN | flags::PXN;
    pub const RX: u64 = flags::AP_RO_ALL | flags::PXN;

    pub fn from_prot(prot: u32) -> u64 {
        match (prot & 0x2 != 0, prot & 0x4 != 0) {
            (true, _)      => RW_NO_EXEC,
            (false, true)  => RX,
            (false, false) => RO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attr_index_values() {
        assert_eq!(attr_index(0), 0);
        assert_eq!(attr_index(1), 4);
        assert_eq!(attr_index(7), 28);
        assert_eq!(attr_index(8), 0); // 8 & 0x7 == 0
    }

    #[test]
    fn page_table_new_all_entries_zero() {
        let pt = PageTable::new();
        for (i, &e) in pt.entries.iter().enumerate() {
            assert_eq!(e, 0, "entry {} should be 0", i);
        }
    }

    #[test]
    fn user_flags_from_prot() {
        // prot 0 = read-only
        assert_eq!(user_flags::from_prot(0), user_flags::RO);
        // prot 2 = write
        assert_eq!(user_flags::from_prot(2), user_flags::RW_NO_EXEC);
        // prot 4 = exec
        assert_eq!(user_flags::from_prot(4), user_flags::RX);
    }

    #[test]
    fn constants_sanity() {
        assert_eq!(PAGE_SIZE, 4096);
        assert_eq!(PAGE_SHIFT, 12);
        assert_eq!(ENTRIES_PER_TABLE, 512);
        assert_eq!(BITS_PER_LEVEL, 9);
        assert_eq!(BLOCK_1GB, 1 << 30);
        assert_eq!(BLOCK_2MB, 1 << 21);
    }
}
