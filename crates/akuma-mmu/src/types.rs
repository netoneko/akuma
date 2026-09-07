//! Pure types and constants for the MMU subsystem.
//!
//! No architecture-specific dependencies - fully host-testable.

#![allow(dead_code)]

/// Page geometry and the neutral permission vocabulary, re-exported from
/// `akuma-mmap`.
///
/// [`Prot`] is what a *region* records. The AArch64 bits it encodes to are
/// [`flags`] and [`user_flags`] below, which live **here**, with the walker that
/// writes them — they were in `akuma-mmap` until 2026-09-06, which made that
/// crate's code portable and its data not. See `akuma_mmap::types` for the
/// measurement: the two architectures' permission masks share exactly zero bits,
/// and AArch64's `AP_MASK` lands on x86's Dirty and PAT, so `is_write` on an x86
/// PTE would have answered "has this page been written?" instead of "may it be?".
pub use akuma_mmap::{PAGE_SHIFT, PAGE_SIZE, Prot};

/// Raw AArch64 stage-1 descriptor bits.
///
/// Moved down from `akuma-mmap` 2026-09-06. Nothing above the walker should name
/// these: a consumer that does is encoding a page table by hand, which is the
/// coupling that stopped the region crate being usable on x86_64.
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

/// The six AArch64 permission encodings, and the decode of a [`Prot`] into one.
///
/// These constants are unchanged from when they lived in `akuma-mmap` — the
/// `to_pte` round-trip test below pins every one of them, so this move cannot
/// have altered a single bit of what the hardware sees.
pub mod user_flags {
    use super::flags;
    use akuma_mmap::Prot;

    /// PROT_NONE: EL1-only access, EL0 gets no read/write/exec.
    pub const NONE: u64 = flags::AP_RO_EL1 | flags::UXN | flags::PXN;
    pub const RO: u64 = flags::AP_RO_ALL;
    pub const RW: u64 = flags::AP_RW_ALL;
    pub const EXEC: u64 = flags::AP_RO_ALL;
    pub const RW_NO_EXEC: u64 = flags::AP_RW_ALL | flags::UXN | flags::PXN;
    pub const RX: u64 = flags::AP_RO_ALL | flags::PXN;
    /// Read-only and **not** executable — the read-only sibling of
    /// [`RW_NO_EXEC`], for a page a process may read and nothing else.
    pub const RO_NO_EXEC: u64 = flags::AP_RO_ALL | flags::UXN | flags::PXN;

    /// The AArch64 encoding of a neutral [`Prot`].
    ///
    /// A **total** match, not a bit computation: `Prot::RO` and `Prot::RX` grant
    /// EL0 the same thing and differ only in `PXN`, so no arithmetic over
    /// read/write/exec can tell them apart. That is exactly why `Prot` is an
    /// opaque token rather than a `{read, write, exec}` triple — see
    /// `akuma_mmap::types`.
    #[must_use]
    pub fn to_pte(prot: Prot) -> u64 {
        match prot {
            Prot::NONE => NONE,
            Prot::RO => RO,
            Prot::RW => RW,
            Prot::RW_NO_EXEC => RW_NO_EXEC,
            Prot::RX => RX,
            Prot::RO_NO_EXEC => RO_NO_EXEC,
            // `Prot` is a closed set (`Prot::ALL`), but its inner tag is opaque
            // so the compiler cannot prove exhaustiveness here. Refusing EL0
            // everything is the safe answer for a value this build does not
            // know; `to_pte_covers_every_variant` makes it unreachable.
            _ => NONE,
        }
    }

    /// The neutral [`Prot`] an AArch64 `user_flags` word denotes — [`to_pte`]
    /// read backwards.
    ///
    /// # Why an inverse exists at all
    ///
    /// Because the `u64` is the currency `akuma-exec` still speaks. Its
    /// `UserAddressSpace` calls — `map_page`, `alloc_and_map`, `map_and_track`,
    /// `map_user_page_tracked` — take a PTE flag word, and `LazyRegion::flags`
    /// stores one; that is AArch64 machinery living in a crate both kernels
    /// compile. The x86 walker cannot consume those bits (the two permission
    /// masks share exactly zero, and AArch64's `AP_MASK` lands on x86's Dirty
    /// and PAT — `akuma_mmap::types` has the measurement), so it decodes them
    /// here first and encodes x86 bits itself.
    ///
    /// **This is not "AArch64 bits crossing an architecture boundary".** The
    /// word is produced by this module and consumed by this module; what
    /// crosses is a `Prot`. That is the `akuma-cow` shape — decoded booleans,
    /// never a raw PTE — which is exactly why that crate already serves both
    /// kernels. The u64 hop disappears when the `REDUCING_PLATFORM_DEPENDENCY.md`
    /// §1 migration moves `akuma-exec` and `LazyRegion` onto `Prot`; until then
    /// it is a named, total, round-trip-pinned seam rather than an accident.
    ///
    /// # Total, by predicate rather than by table
    ///
    /// Every `u64` gets an answer, including words this module never emitted —
    /// the three questions asked are the same three [`is_write`], [`is_exec`]
    /// and the `AP` field already answer, so a caller cannot construct a value
    /// that decodes to something the AArch64 walker would disagree with. The
    /// six [`to_pte`] outputs come back exactly as they went in
    /// (`from_pte_inverts_to_pte`).
    ///
    /// EL0-inaccessible words (`AP_RO_EL1`/`AP_RW_EL1`, which is what
    /// [`NONE`] is) decode to [`Prot::NONE`] — fail-closed, and the same answer
    /// `is_none` gives for the one such word this module emits.
    #[must_use]
    pub const fn from_pte(flags_val: u64) -> Prot {
        // EL0 reachable at all? `AP` is a 2-bit field and only the two `*_ALL`
        // encodings grant EL0 anything; everything else is EL1-only.
        let ap = flags_val & flags::AP_MASK;
        if ap != flags::AP_RW_ALL && ap != flags::AP_RO_ALL {
            return Prot::NONE;
        }
        if is_write(flags_val) {
            // RW is writable *and* executable; RW_NO_EXEC carries UXN.
            if is_exec(flags_val) { Prot::RW } else { Prot::RW_NO_EXEC }
        } else if is_exec(flags_val) {
            // RO and RX differ only in PXN — whether EL1 may fetch. Nothing
            // else separates them, which is why `Prot` is a token and not a
            // `{read, write, exec}` triple.
            if flags_val & flags::PXN == 0 { Prot::RO } else { Prot::RX }
        } else {
            Prot::RO_NO_EXEC
        }
    }

    /// Whether a mapping with these raw PTE flags lets EL0 **write**.
    ///
    /// Still takes a `u64` because its callers hold *page-table* flags —
    /// `lazy_map_flags`' output and `LazyRegion::flags`, both of which are
    /// AArch64 encodings that have not moved. A caller holding a [`Prot`] wants
    /// `Prot::is_write` instead.
    #[must_use]
    pub const fn is_write(flags_val: u64) -> bool {
        flags_val & flags::AP_MASK == flags::AP_RW_ALL
    }

    /// Whether EL0 may *fetch instructions* from a page with these raw flags.
    /// Reads `UXN` and nothing else — `AP` decides read/write and `PXN` decides
    /// EL1 fetch, neither of which is relevant to what EL0 can execute.
    #[must_use]
    pub const fn is_exec(flags_val: u64) -> bool {
        flags_val & flags::UXN == 0
    }

    #[must_use]
    pub const fn is_none(flags_val: u64) -> bool {
        flags_val == NONE
    }

    #[must_use]
    pub const fn is_read_only_to_user(flags_val: u64) -> bool {
        flags_val & flags::AP_MASK == flags::AP_RO_ALL
    }

    /// Is a page mapped with these flags eligible for the shared file-page cache?
    /// The gate is a parameter, not a config read.
    #[must_use]
    pub const fn is_shareable_mapping(flags_val: u64, shared_file_pages_enabled: bool) -> bool {
        shared_file_pages_enabled && is_read_only_to_user(flags_val)
    }

    /// The raw encoding an `mmap`/`mprotect` `prot` argument asks for.
    /// `Prot::from_prot` composed with [`to_pte`], kept as one call because most
    /// callers want a PTE.
    #[must_use]
    pub fn from_prot(prot: u32) -> u64 {
        to_pte(Prot::from_prot(prot))
    }
}

pub const ENTRIES_PER_TABLE: usize = 512;
pub const BITS_PER_LEVEL: usize = 9;

/// The fixed L0[1] device-mapping window, re-exported from
/// `akuma_primitives::addr`. It moved so `akuma-virtio` could reach
/// `DEV_VIRTIO_VA` without depending on this crate — the last edge keeping
/// `akuma-net` on it. See that module's header for why the table moved whole
/// rather than one constant at a time.
pub use akuma_primitives::addr::{
    DEV_GIC_CPU_VA, DEV_GIC_DIST_SIZE, DEV_GIC_DIST_VA, DEV_GICR_RD_VA,
    DEV_GICR_SGI_VA, DEV_UART_VA, DEV_VIRTIO_SIZE, DEV_VIRTIO_VA, DEV_WINDOW_NO_OVERLAP,
    DEV_WINDOW_SIZE, DEV_WINDOW_SPANS, DEV_WINDOW_VA,
};

pub const MAIR_DEVICE_NGNRNE: u64 = 0;
pub const MAIR_NORMAL_NC: u64 = 1;
pub const MAIR_NORMAL_WT: u64 = 2;
pub const MAIR_NORMAL_WB: u64 = 3;


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

    /// Every `Prot` variant encodes to the **literal bits** `user_flags` produced
    /// before the vocabulary moved out of `akuma-mmap` (2026-09-06).
    ///
    /// Spelled as hex literals on purpose rather than as `user_flags::RW` — a
    /// test written against the constants would pass even if the constants
    /// themselves drifted, which is the whole thing this is guarding. These
    /// numbers were read off the pre-move source.
    #[test]
    fn prot_roundtrips_to_todays_bits() {
        use user_flags::to_pte;
        // AP_RO_EL1|UXN|PXN = (2<<6) | (1<<54) | (1<<53)
        assert_eq!(to_pte(Prot::NONE), 0x0060_0000_0000_0080);
        // AP_RO_ALL = 3<<6
        assert_eq!(to_pte(Prot::RO), 0xC0);
        assert_eq!(to_pte(Prot::EXEC), 0xC0, "EXEC was byte-identical to RO");
        // AP_RW_ALL = 1<<6
        assert_eq!(to_pte(Prot::RW), 0x40);
        // AP_RW_ALL|UXN|PXN
        assert_eq!(to_pte(Prot::RW_NO_EXEC), 0x0060_0000_0000_0040);
        // AP_RO_ALL|PXN
        assert_eq!(to_pte(Prot::RX), 0x0020_0000_0000_00C0);
        // AP_RO_ALL|UXN|PXN
        assert_eq!(to_pte(Prot::RO_NO_EXEC), 0x0060_0000_0000_00C0);
    }

    /// [`user_flags::from_pte`] is the exact inverse of `to_pte` over its six
    /// outputs. This is the property the x86 walker rests on: it decodes a
    /// flag word `akuma-exec` handed it and must recover the protection the
    /// AArch64 walker would have written, or the two kernels grant different
    /// things from the same call.
    #[test]
    fn from_pte_inverts_to_pte() {
        for p in Prot::ALL {
            assert_eq!(user_flags::from_pte(user_flags::to_pte(p)), p, "{p:?}");
        }
        // `EXEC` is an alias of `RO`, so it round-trips to `RO` by identity
        // rather than as a seventh variant.
        assert_eq!(user_flags::from_pte(user_flags::to_pte(Prot::EXEC)), Prot::RO);
    }

    /// `from_pte` is **total**, and fails closed on anything EL0 cannot reach.
    /// The words below are not ones `to_pte` emits — they are what a caller
    /// composing raw `flags::` constants can produce, and each must decode to
    /// something no more permissive than the bits say.
    #[test]
    fn from_pte_is_total_and_fails_closed() {
        // EL1-only access permissions: no EL0 rights at all.
        assert_eq!(user_flags::from_pte(flags::AP_RW_EL1), Prot::NONE);
        assert_eq!(user_flags::from_pte(flags::AP_RO_EL1), Prot::NONE);
        // Zero — `LazyRegion`'s "unrecorded" sentinel — has AP = AP_RW_EL1 (0),
        // so it decodes to NONE rather than to a writable page. Callers that
        // mean "not recorded" substitute a default *before* they get here
        // (`akuma_exec::process::lazy_prefault`); this is the backstop.
        assert_eq!(user_flags::from_pte(0), Prot::NONE);
        // Bits outside AP/UXN/PXN are ignored, not smuggled through: a PTE with
        // its address, VALID, AF and shareability bits set decodes on
        // permissions alone.
        let live_pte = 0x4_2000_0000
            | flags::VALID
            | flags::TABLE
            | flags::AF
            | flags::SH_INNER
            | user_flags::RW_NO_EXEC;
        assert_eq!(user_flags::from_pte(live_pte), Prot::RW_NO_EXEC);
    }

    /// `to_pte` has a `_ =>` arm it should never reach. Prove it: every variant
    /// in `Prot::ALL` must land on a distinct, non-fallback encoding.
    #[test]
    fn to_pte_covers_every_variant() {
        let mut seen = alloc::vec::Vec::new();
        for p in Prot::ALL {
            let pte = user_flags::to_pte(p);
            if p != Prot::NONE {
                assert_ne!(pte, user_flags::NONE, "{p:?} fell through to the NONE arm");
            }
            assert!(!seen.contains(&pte), "{p:?} collides with an earlier variant");
            seen.push(pte);
        }
        assert_eq!(seen.len(), 6);
    }

    /// The neutral predicates and the raw ones must agree, variant by variant.
    /// This is the join between the two vocabularies, and the place a future
    /// divergence would show up first.
    #[test]
    fn neutral_and_raw_predicates_agree() {
        for p in Prot::ALL {
            let pte = user_flags::to_pte(p);
            assert_eq!(p.is_write(), user_flags::is_write(pte), "is_write {p:?}");
            assert_eq!(p.is_exec(), user_flags::is_exec(pte), "is_exec {p:?}");
            assert_eq!(p.is_none(), user_flags::is_none(pte), "is_none {p:?}");
            assert_eq!(
                p.is_read_only_to_user(),
                user_flags::is_read_only_to_user(pte),
                "is_read_only_to_user {p:?}"
            );
            for gate in [true, false] {
                assert_eq!(
                    p.is_shareable_mapping(gate),
                    user_flags::is_shareable_mapping(pte, gate),
                    "is_shareable_mapping {p:?} gate={gate}"
                );
            }
        }
    }

    /// `from_prot` must route through `Prot` and land on the same encoding the
    /// old direct implementation did, for every `prot` bit pattern.
    #[test]
    fn from_prot_matches_the_old_direct_encoding() {
        for prot in 0u32..8 {
            let want = if prot == 0 {
                user_flags::NONE
            } else if prot & 0x2 != 0 {
                user_flags::RW_NO_EXEC
            } else if prot & 0x4 != 0 {
                user_flags::RX
            } else {
                user_flags::RO
            };
            assert_eq!(user_flags::from_prot(prot), want, "prot={prot:#x}");
        }
    }
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
