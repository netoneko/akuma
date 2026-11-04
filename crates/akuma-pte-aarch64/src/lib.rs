//! The AArch64 user-leaf encoding a tokio kernel writes, and the questions asked
//! of one.
//!
//! The counterpart of `akuma-pte-amd64` for the aarch64 half of
//! `akuma-tokio-kernel` (B21). A leaf here is what `akuma_pagetable::aarch64`
//! installs at L3: `VALID | PAGE | AF | nG | SH_INNER | AttrIndx(3)` - the same
//! base the AArch64 kernel's `UserAddressSpace::map_page` writes, so a page means
//! the same thing to the hardware under either kernel - plus the access bits and
//! two software bits:
//!
//! | bits | field |
//! |---|---|
//! | 7:6 | `AP`: `01` EL0+EL1 RW, `11` EL0+EL1 RO, `10` EL1 RO only (`PROT_NONE`) |
//! | 53 | `PXN`: always set - the kernel never executes a user page |
//! | 54 | `UXN`: set unless the mapping is executable |
//! | 55 | [`COW`] (software): read-only because a fork shared it |
//! | 56 | [`WRITTEN`] (software): the W^X record |
//!
//! # `WRITTEN` is conservative
//!
//! amd64's W^X record is the dirty bit, which the CPU sets on a user store. An
//! AArch64 core without hardware dirty-bit management never records one, so
//! [`user_leaf`] sets [`WRITTEN`] on every page it maps writable. Pinned
//! divergence: `mprotect(PROT_READ|PROT_EXEC)` of a page mapped writable and
//! never stored to is refused here under W^X and allowed on amd64. A page that
//! was written is refused on both, which is the case W^X exists for.

#![no_std]
#![forbid(unsafe_code)]

/// Bit 0: valid.
pub const VALID: u64 = 1 << 0;
/// Bit 1 at L3: a page (with [`VALID`]).
pub const PAGE: u64 = 1 << 1;
/// `AttrIndx` = 3: the kernel's `MAIR` slot for normal write-back memory
/// (`akuma_mmu::types::MAIR_NORMAL_WB`).
pub const ATTR_NORMAL_WB: u64 = 3 << 2;
/// Inner shareable.
pub const SH_INNER: u64 = 3 << 8;
/// Access flag (set, so the first access does not fault).
pub const AF: u64 = 1 << 10;
/// Not global: the translation belongs to one address space's ASID.
pub const NG: u64 = 1 << 11;
/// `AP[1]`: EL0 may access.
pub const AP_EL0: u64 = 1 << 6;
/// `AP[2]`: read-only.
pub const AP_RO: u64 = 1 << 7;
/// Privileged execute-never.
pub const PXN: u64 = 1 << 53;
/// Unprivileged execute-never.
pub const UXN: u64 = 1 << 54;
/// Software: a copy-on-write share.
pub const COW: u64 = 1 << 55;
/// Software: written since mapped (see the crate docs).
pub const WRITTEN: u64 = 1 << 56;
/// The output address, bits 47:12.
pub const ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;

const BASE: u64 = VALID | PAGE | AF | NG | SH_INNER | ATTR_NORMAL_WB | PXN;

/// The leaf mapping `pa` for user code. `user == false` is `PROT_NONE`: the frame
/// stays mapped for the kernel (read-only), every EL0 access faults.
#[must_use]
pub const fn user_leaf(pa: u64, write: bool, exec: bool, user: bool) -> u64 {
    let mut e = BASE | (pa & ADDR_MASK);
    if !user {
        return e | AP_RO | UXN;
    }
    e |= AP_EL0;
    if write {
        e |= WRITTEN;
    } else {
        e |= AP_RO;
    }
    if !exec {
        e |= UXN;
    }
    e
}

/// Is `e` a valid page?
#[must_use]
pub const fn is_valid(e: u64) -> bool {
    e & (VALID | PAGE) == VALID | PAGE
}

/// The frame a valid page maps.
#[must_use]
pub const fn frame(e: u64) -> Option<u64> {
    if is_valid(e) { Some(e & ADDR_MASK) } else { None }
}

/// May EL0 write through `e`?
#[must_use]
pub const fn writable(e: u64) -> bool {
    is_valid(e) && e & AP_EL0 != 0 && e & AP_RO == 0
}

/// May EL0 reach `e` at all (false for `PROT_NONE`)?
#[must_use]
pub const fn user(e: u64) -> bool {
    e & AP_EL0 != 0
}

/// May EL0 execute through `e`?
#[must_use]
pub const fn exec(e: u64) -> bool {
    e & UXN == 0
}

/// Is `e` a copy-on-write share?
#[must_use]
pub const fn cow_marked(e: u64) -> bool {
    e & COW != 0
}

/// Has the page been written since it was mapped?
#[must_use]
pub const fn written(e: u64) -> bool {
    e & WRITTEN != 0
}

/// `e`, recorded as written.
#[must_use]
pub const fn with_written(e: u64) -> u64 {
    e | WRITTEN
}

/// `e`, writable and unmarked: the space takes the frame in place.
#[must_use]
pub const fn take(e: u64) -> u64 {
    (e & !(AP_RO | COW)) | AP_EL0
}

/// `e`'s attributes on frame `pa`, writable and unmarked: the private copy.
#[must_use]
pub const fn take_at(e: u64, pa: u64) -> u64 {
    take((e & !ADDR_MASK) | (pa & ADDR_MASK))
}

/// `e`, read-only and marked: a share the next write must break.
#[must_use]
pub const fn share_marked(e: u64) -> u64 {
    e | AP_RO | COW
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_base_as_the_aarch64_kernel() {
        // `UserAddressSpace::map_page`: VALID|TABLE|AF|NG|attr_index(3)|SH_INNER|user_flags.
        let rw = user_leaf(0x4000_1000, true, false, true);
        assert_eq!(rw & 0xfff & !(3 << 6), VALID | PAGE | AF | NG | SH_INNER | ATTR_NORMAL_WB);
        assert_eq!(frame(rw), Some(0x4000_1000));
    }

    #[test]
    fn permissions_round_trip() {
        for (w, x) in [(false, false), (true, false), (false, true), (true, true)] {
            let e = user_leaf(0x1000, w, x, true);
            assert_eq!((writable(e), exec(e), user(e)), (w, x, true));
            assert!(e & PXN != 0, "the kernel never executes a user page");
        }
        let none = user_leaf(0x1000, true, true, false);
        assert!(!user(none) && !writable(none) && !exec(none));
        assert_eq!(frame(none), Some(0x1000), "PROT_NONE keeps its frame");
    }

    #[test]
    fn written_is_conservative() {
        assert!(written(user_leaf(0x1000, true, false, true)));
        assert!(!written(user_leaf(0x1000, false, true, true)));
        assert!(written(with_written(user_leaf(0x1000, false, false, true))));
    }

    #[test]
    fn cow_edits() {
        let rw = user_leaf(0x5000, true, false, true);
        let shared = share_marked(rw);
        assert!(cow_marked(shared) && !writable(shared));
        let taken = take(shared);
        assert!(!cow_marked(taken) && writable(taken));
        assert_eq!(frame(taken), Some(0x5000));
        let copied = take_at(shared, 0x9000);
        assert_eq!(frame(copied), Some(0x9000));
        assert!(writable(copied) && !cow_marked(copied));
        assert_eq!(copied & !ADDR_MASK, taken & !ADDR_MASK, "a copy keeps every other attribute");
    }

    #[test]
    fn invalid_entries_answer_no() {
        assert_eq!(frame(0), None);
        assert!(!writable(0) && !is_valid(VALID), "L3 0b01 is reserved, not a page");
    }
}
