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

    /// Whether a mapping with these flags lets EL0 *fetch instructions* from the page.
    ///
    /// This is the predicate that decides whether a demand-paged frame needs the
    /// `dc cvau` + `ic ivau` sequence: I-cache maintenance is only load-bearing for a
    /// page some PE will fetch from. It reads `UXN` and nothing else, deliberately —
    /// `AP` decides read/write, `PXN` decides EL1 fetch, and neither is relevant to
    /// what EL0 can execute.
    pub const fn is_exec(flags: u64) -> bool {
        flags & flags::UXN == 0
    }
}

/// Which EL0 abort a demand-paging fault arrived through.
///
/// The data-abort and instruction-abort arms of `rust_sync_el0_handler_inner` share
/// **one** demand-paging body (`exceptions.rs`'s `demand_page_lazy_region`), and this
/// is the seam between them: one body, two documented entry points. Every difference
/// between the arms is decided by the methods below rather than by keeping a second
/// copy of the ~330-line body — see `docs/archive/COW_PILE_AUDIT.md` §6 and §12.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FaultAccess {
    /// `EC_DATA_ABORT_LOWER` — a load or a store wanted the page.
    Data,
    /// `EC_INST_ABORT_LOWER` — an instruction fetch wanted the page.
    Instruction,
}

impl FaultAccess {
    /// Flags for a page whose lazy region records **none** (`flags == 0`), and for
    /// every anonymous page regardless of what the region records.
    ///
    /// The fault itself is the only evidence available in that case, and it is good
    /// evidence: a load/store wants data, an instruction fetch wants text. Anonymous
    /// pages take this unconditionally in both arms — the historical shape, kept
    /// because an anonymous instruction fetch (a JIT writing then jumping into a
    /// `MAP_ANONYMOUS` page) has no other way to become executable.
    pub const fn default_map_flags(self) -> u64 {
        match self {
            Self::Data => user_flags::RW_NO_EXEC,
            Self::Instruction => user_flags::RX,
        }
    }

    /// Log prefix for this arm's demand-paging diagnostics (`DA-DP` / `IA-DP`).
    ///
    /// The two spellings stay distinct because every archived investigation greps for
    /// one or the other; merging the body must not merge the log tags.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Data => "DA-DP",
            Self::Instruction => "IA-DP",
        }
    }
}

/// PTE flags for one demand-paged page of a lazy region.
///
/// `region_flags` is what the region recorded (`LazyRegion::flags`): an `mmap` PROT
/// translated by [`user_flags::from_prot`], or an ELF segment's `p_flags`. It wins
/// whenever it says anything at all — a region is a statement about permissions and
/// the fault is not. `file_backed` is false for anonymous pages, which have never
/// consulted `region_flags` on either arm.
pub const fn lazy_map_flags(access: FaultAccess, region_flags: u64, file_backed: bool) -> u64 {
    if file_backed && region_flags != 0 {
        region_flags
    } else {
        access.default_map_flags()
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
    fn user_flags_is_exec_reads_only_uxn() {
        assert!(user_flags::is_exec(user_flags::RX));
        assert!(user_flags::is_exec(user_flags::EXEC));
        assert!(!user_flags::is_exec(user_flags::RW_NO_EXEC));
        assert!(!user_flags::is_exec(user_flags::NONE));
        // `RO`/`RW` carry no UXN, so they are exec by this predicate — that is the
        // AArch64 encoding, not an oversight: a PTE without UXN *is* EL0-executable.
        assert!(user_flags::is_exec(user_flags::RO));
        // PXN (EL1 fetch) must not influence the answer.
        assert!(user_flags::is_exec(user_flags::RO | flags::PXN));
        assert!(!user_flags::is_exec(user_flags::RO | flags::UXN));
    }

    /// The full policy table of the merged DA/IA demand-paging body: for every
    /// (entry point × source × recorded flags) case, which PTE flags the page gets
    /// and whether the frame needs I-cache maintenance.
    ///
    /// This is the whole behavioural surface of that merge — the body itself is
    /// identical between the two arms once these two answers are supplied
    /// (`docs/archive/COW_PILE_AUDIT.md` §12).
    #[test]
    fn lazy_map_flags_policy_table() {
        use FaultAccess::{Data, Instruction};
        let rx = user_flags::RX;
        let rw = user_flags::RW_NO_EXEC;

        // A file region that recorded flags: the region wins on BOTH arms, which is
        // what makes a non-exec instruction fetch possible in the first place.
        assert_eq!(lazy_map_flags(Data, rx, true), rx);
        assert_eq!(lazy_map_flags(Instruction, rx, true), rx);
        assert_eq!(lazy_map_flags(Data, rw, true), rw);
        assert_eq!(lazy_map_flags(Instruction, rw, true), rw);
        assert_eq!(lazy_map_flags(Instruction, user_flags::RO, true), user_flags::RO);

        // A file region that recorded nothing: the fault decides.
        assert_eq!(lazy_map_flags(Data, 0, true), rw);
        assert_eq!(lazy_map_flags(Instruction, 0, true), rx);

        // Anonymous: the fault decides even when the region recorded flags. Both arms
        // have always ignored `region_flags` here.
        assert_eq!(lazy_map_flags(Data, rx, false), rw);
        assert_eq!(lazy_map_flags(Instruction, rw, false), rx);
        assert_eq!(lazy_map_flags(Data, 0, false), rw);
        assert_eq!(lazy_map_flags(Instruction, 0, false), rx);

        // I-cache maintenance follows the *mapping*, not the entry point: an
        // instruction fetch into a non-exec file region maps non-exec and needs no
        // maintenance, because nothing can fetch from it until the permission-fault
        // arm upgrades it to RX and maintains it there.
        assert!(user_flags::is_exec(lazy_map_flags(Instruction, 0, true)));
        assert!(!user_flags::is_exec(lazy_map_flags(Instruction, rw, true)));
        assert!(user_flags::is_exec(lazy_map_flags(Data, rx, true)));
        assert!(!user_flags::is_exec(lazy_map_flags(Data, 0, true)));
    }

    #[test]
    fn fault_access_tags_stay_distinct() {
        assert_eq!(FaultAccess::Data.tag(), "DA-DP");
        assert_eq!(FaultAccess::Instruction.tag(), "IA-DP");
        assert_ne!(FaultAccess::Data.tag(), FaultAccess::Instruction.tag());
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
