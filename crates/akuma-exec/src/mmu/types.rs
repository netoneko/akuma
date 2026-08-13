//! Pure types and constants for the MMU subsystem.
//!
//! No architecture-specific dependencies - fully host-testable.

#![allow(dead_code)]

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SHIFT: usize = 12;
pub const ENTRIES_PER_TABLE: usize = 512;
pub const BITS_PER_LEVEL: usize = 9;

/// The fixed L0[1] device-mapping window, re-exported from
/// `akuma_primitives::addr`. It moved so `akuma-virtio` could reach
/// `DEV_VIRTIO_VA` without depending on this crate — the last edge keeping
/// `akuma-net` on it. See that module's header for why the table moved whole
/// rather than one constant at a time.
pub use akuma_primitives::addr::{
    DEV_FW_CFG_VA, DEV_GIC_CPU_VA, DEV_GIC_DIST_VA, DEV_GICR_RD_VA, DEV_GICR_SGI_VA, DEV_UART_VA,
    DEV_VIRTIO_VA,
};

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
    /// AP field mask (bits [7:6]) — isolates the access-permission bits from a PTE
    /// or a `user_flags` value so the two can be compared.
    pub const AP_MASK: u64 = 3 << 6;
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
    /// PROT_NONE: EL1-only access, EL0 gets no read/write/exec.
    pub const NONE: u64 = flags::AP_RO_EL1 | flags::UXN | flags::PXN;
    pub const RO: u64 = flags::AP_RO_ALL;
    pub const RW: u64 = flags::AP_RW_ALL;
    pub const EXEC: u64 = flags::AP_RO_ALL;
    pub const RW_NO_EXEC: u64 = flags::AP_RW_ALL | flags::UXN | flags::PXN;
    pub const RX: u64 = flags::AP_RO_ALL | flags::PXN;

    pub fn from_prot(prot: u32) -> u64 {
        if prot == 0 { return NONE; }
        match (prot & 0x2 != 0, prot & 0x4 != 0) {
            (true, _)      => RW_NO_EXEC,
            (false, true)  => RX,
            (false, false) => RO,
        }
    }

    pub fn is_none(flags: u64) -> bool {
        flags == NONE
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
        // prot 0 = PROT_NONE (no EL0 access)
        assert_eq!(user_flags::from_prot(0), user_flags::NONE);
        assert!(user_flags::is_none(user_flags::from_prot(0)));
        // prot 1 = PROT_READ
        assert_eq!(user_flags::from_prot(1), user_flags::RO);
        assert!(!user_flags::is_none(user_flags::from_prot(1)));
        // prot 2 = PROT_WRITE
        assert_eq!(user_flags::from_prot(2), user_flags::RW_NO_EXEC);
        // prot 4 = PROT_EXEC
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
