//! `mremap`'s move-vs-expand decision.

use akuma_primitives::errno::negated::{EFAULT, EINVAL, ENOMEM};
use akuma_syscalls_linux::flags::mremap::MREMAP_MAYMOVE;

/// What `sys_mremap` should do, decided before any process is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Return this errno immediately. Decided from arguments alone, so a
    /// kernel-test caller with no current process still gets the argument error
    /// rather than `ESRCH`.
    Fail(u64),
    /// The new size fits inside the pages the mapping already has: return
    /// `old_addr` unchanged.
    ///
    /// **Divergence 5.** A genuine shrink therefore keeps the tail mapped; Linux
    /// unmaps it. Preserved, not fixed.
    InPlace,
    /// Growth is needed. `may_move` says whether `MREMAP_MAYMOVE` was set.
    ///
    /// When it is **false** the kernel must probe whether `old_addr` is mapped and
    /// answer with [`no_move_errno`]. That probe costs three lookups and a
    /// `vm_lock` acquisition, so it is gated here rather than run unconditionally —
    /// the pre-extraction code gated it the same way and this preserves that.
    Grow { new_pages: usize, may_move: bool },
}

/// Decide from the arguments alone.
///
/// `va_limit` is `akuma_exec::process::user_access::USER_VA_LIMIT`, passed in rather
/// than imported so this crate stays a leaf.
///
/// Order is load-bearing: every check here happens **before** the kernel resolves a
/// process, so argument errors beat `ESRCH`.
#[must_use]
pub fn plan(old_addr: usize, old_size: usize, new_size: usize, flags: u32, va_limit: usize) -> Plan {
    if new_size == 0 {
        return Plan::Fail(EINVAL);
    }
    if old_addr & 0xFFF != 0 {
        return Plan::Fail(EINVAL);
    }
    if old_addr >= va_limit {
        return Plan::Fail(EFAULT);
    }
    let old_pages = old_size.div_ceil(4096);
    let new_pages = new_size.div_ceil(4096);
    if new_pages <= old_pages {
        return Plan::InPlace;
    }
    Plan::Grow { new_pages, may_move: flags & MREMAP_MAYMOVE != 0 }
}

/// The errno for a growth request that may not move, given whether `old_addr` is
/// mapped.
///
/// `ENOMEM` means "the mapping is real, there is just no room to grow it in place";
/// `EFAULT` means "there is no mapping there at all". Getting these the wrong way
/// round is a well-worn `mremap` bug, which is why the two-line rule is pinned.
#[must_use]
pub const fn no_move_errno(is_mapped: bool) -> u64 {
    if is_mapped { ENOMEM } else { EFAULT }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VA_LIMIT: usize = 1 << 48;

    /// Argument errors are decided before anything else, so `sys_mremap` can return
    /// them without a current process.
    #[test]
    fn argument_errors_come_first() {
        assert_eq!(plan(0x1000, 0x1000, 0, MREMAP_MAYMOVE, VA_LIMIT), Plan::Fail(EINVAL));
        assert_eq!(plan(0x1001, 0x1000, 0x2000, MREMAP_MAYMOVE, VA_LIMIT), Plan::Fail(EINVAL));
        assert_eq!(plan(VA_LIMIT, 0x1000, 0x2000, MREMAP_MAYMOVE, VA_LIMIT), Plan::Fail(EFAULT));
    }

    /// A zero new size is `EINVAL` even when the address is also bad — the size
    /// check is first, and the errno the caller sees depends on that order.
    #[test]
    fn zero_new_size_beats_a_bad_address() {
        assert_eq!(plan(0x1001, 0x1000, 0, 0, VA_LIMIT), Plan::Fail(EINVAL));
    }

    /// The VA-limit test is `>=`: the limit itself is not a usable address.
    #[test]
    fn va_limit_is_exclusive() {
        assert_eq!(plan(VA_LIMIT, 0x1000, 0x2000, 0, VA_LIMIT), Plan::Fail(EFAULT));
        assert!(matches!(
            plan(VA_LIMIT - 0x1000, 0x1000, 0x2000, 0, VA_LIMIT),
            Plan::Grow { .. }
        ));
    }

    /// Growth within the same page count is a no-op: the pages are already there.
    #[test]
    fn growth_inside_the_last_page_is_in_place() {
        assert_eq!(plan(0x1000, 1, 4096, 0, VA_LIMIT), Plan::InPlace);
        assert_eq!(plan(0x1000, 4096, 4096, 0, VA_LIMIT), Plan::InPlace);
    }

    /// **Divergence 5.** A real shrink returns the old address and leaves the tail
    /// mapped; Linux unmaps it.
    #[test]
    fn diverge_shrink_is_in_place_and_leaves_the_tail_mapped() {
        assert_eq!(plan(0x1000, 16 * 4096, 4096, MREMAP_MAYMOVE, VA_LIMIT), Plan::InPlace);
    }

    /// One byte past the page boundary is a real growth, and the page count is
    /// rounded up, not truncated.
    #[test]
    fn growth_past_the_page_boundary_reports_rounded_pages() {
        assert_eq!(
            plan(0x1000, 4096, 4097, MREMAP_MAYMOVE, VA_LIMIT),
            Plan::Grow { new_pages: 2, may_move: true }
        );
    }

    /// `may_move` is carried, not acted on — it exists so the kernel can skip the
    /// mapped-ness probe when `MREMAP_MAYMOVE` is set. A regression that dropped
    /// the flag would make every growing `mremap` take a `vm_lock` it does not need.
    #[test]
    fn may_move_is_reported_so_the_probe_stays_gated() {
        assert_eq!(
            plan(0x1000, 4096, 8192, 0, VA_LIMIT),
            Plan::Grow { new_pages: 2, may_move: false }
        );
        assert_eq!(
            plan(0x1000, 4096, 8192, MREMAP_MAYMOVE, VA_LIMIT),
            Plan::Grow { new_pages: 2, may_move: true }
        );
    }

    /// The errno split that `mremap` implementations classically get backwards.
    #[test]
    fn no_move_maps_mapped_to_enomem_and_unmapped_to_efault() {
        assert_eq!(no_move_errno(true), ENOMEM);
        assert_eq!(no_move_errno(false), EFAULT);
        assert_ne!(ENOMEM, EFAULT);
    }
}
