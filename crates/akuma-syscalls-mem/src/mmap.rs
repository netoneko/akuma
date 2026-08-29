//! `mmap`'s mapping-kind decision, `MAP_FIXED` validation, and `munmap`'s sizing.

use akuma_syscalls_linux::flags::map::{
    MAP_ANONYMOUS, MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_NORESERVE, MAP_POPULATE, MAP_SHARED,
};
use akuma_syscalls_linux::flags::prot::{PROT_NONE, PROT_WRITE};

/// What kind of mapping a request asks for.
///
/// Every field is a function of the argument bits plus the page count — no probe,
/// no process, no region list. The kernel computes this once at the top of
/// `sys_mmap` and then acts on it.
// Six bools, deliberately. `clippy::struct_excessive_bools` wants a state machine,
// but these are not states — they are independent facts about one request, and most
// combinations occur: a mapping can be file-backed AND shared-writable AND not lazy
// at the same time. Collapsing them into an enum would have to enumerate the
// product, which is how the pre-extraction code's `is_lazy && !is_file_backed &&
// !map_populate` chains got hard to read in the first place. Allowed at the item so
// the crate still has no crate-level allow block.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// `PROT_NONE` anonymous: a pure address-space reservation. Its pages are
    /// zero-on-demand by definition, so it is always lazy.
    pub is_lazy_reservation: bool,
    /// Backed by a file rather than anonymous memory.
    pub is_file_backed: bool,
    /// Writable `MAP_SHARED` on a file: writes through the mapping must become
    /// visible in the file. Akuma has no unified page cache, so these are mapped
    /// **eagerly** and written back on `munmap`/`msync`/exit.
    pub is_shared_writable: bool,
    /// Demand-page this mapping instead of allocating every frame up front.
    pub use_lazy: bool,
    /// `MAP_SHARED | MAP_ANONYMOUS`: must survive `fork` as one object rather than
    /// being CoW-copied.
    pub shared_anon: bool,
    /// Eligible for the file-backed lazy path, *if* the kernel's
    /// `MMAP_FILE_BACKED_LAZY` config allows it. A writable `MAP_SHARED` file
    /// mapping is excluded: it must stay resident so its pages can be written back.
    pub file_lazy_eligible: bool,
}

/// Decide the mapping kind.
///
/// `pages` is the *page* count (`len.div_ceil(4096)`), and `eager_max_pages` is the
/// kernel's `config::MMAP_EAGER_MAX_PAGES` — passed in rather than imported because
/// it is a build-time knob and this crate has no build config.
///
/// The eager/lazy rule is the "lazy zero-on-demand population" win from
/// `docs/archive/COW_OPTIMIZATIONS.md`: pages never touched are never allocated,
/// which is what stopped `rustc` running the tree near OOM on eager over-commit.
/// Small mappings stay eager.
#[must_use]
pub fn plan(prot: u32, flags: u32, fd: i32, pages: usize, eager_max_pages: usize) -> Plan {
    // `PROT_NONE` is genuinely 0, so this must be `==`, not `&` — see the
    // `prot_none_is_zero` assertion in `akuma-syscalls-linux`.
    let is_lazy_reservation = prot == PROT_NONE && (flags & MAP_ANONYMOUS != 0);
    let is_file_backed = flags & MAP_ANONYMOUS == 0 && fd >= 0;
    let is_shared_writable =
        (flags & MAP_SHARED != 0) && is_file_backed && (prot & PROT_WRITE != 0);
    let map_populate = flags & MAP_POPULATE != 0;

    // MAP_POPULATE asks for eager pre-faulting, so it suppresses laziness outright.
    let use_lazy = !is_file_backed
        && !map_populate
        && (is_lazy_reservation || (flags & MAP_NORESERVE != 0) || pages > eager_max_pages);

    Plan {
        is_lazy_reservation,
        is_file_backed,
        is_shared_writable,
        use_lazy,
        shared_anon: (flags & MAP_SHARED != 0) && !is_file_backed,
        file_lazy_eligible: is_file_backed && !is_shared_writable,
    }
}

/// Whether a `MAP_FIXED` / `MAP_FIXED_NOREPLACE` request is `EINVAL` for **page
/// misalignment**.
///
/// The kernel calls this *before* resolving the current process, deliberately: a
/// kernel-test caller with no current task would otherwise get `ESRCH` where Linux
/// gives `EINVAL`. That ordering is asserted by
/// `test_mmap_einval_through_handle_syscall` in the boot suite, which cannot move
/// here — the property is about where the call sits, not what it returns.
#[must_use]
pub fn fixed_addr_unaligned_einval(addr: usize, flags: u32) -> bool {
    let is_fixed = (flags & MAP_FIXED) != 0;
    let is_fixed_noreplace = (flags & MAP_FIXED_NOREPLACE) != 0;
    (is_fixed || is_fixed_noreplace) && addr != 0 && (addr & 0xFFF) != 0
}

/// Whether a `MAP_FIXED` mapping would overlap the kernel identity-map VA range.
///
/// `kernel_va_start` and `kernel_va_end` are passed in because the end **scales
/// with detected RAM** — it is a runtime value, not a constant, and a fixed 2 GB
/// window would miss overlaps on a larger machine.
///
/// The Go runtime commits its heap arenas with `MAP_FIXED`; without this guard a
/// process can map user pages over the kernel's physical-RAM identity map and
/// corrupt it silently.
/// # Overflow
///
/// `pages * 4096` used to overflow for a `len` near `usize::MAX`, wrapping
/// `map_end` back down to `addr` so the guard answered "no overlap" for a mapping
/// spanning the whole address space — including the kernel identity map this exists
/// to protect. `saturating_mul` since 2026-08-29; a mapping that big now saturates
/// to `usize::MAX` and correctly reports an overlap.
/// `docs/archive/AKUMA_EXTRACT_MMAP.md` §10.1 defect B.
#[must_use]
pub fn fixed_overlaps_kernel_va(
    addr: usize,
    len: usize,
    kernel_va_start: usize,
    kernel_va_end: usize,
) -> bool {
    let pages = len.div_ceil(4096);
    let map_end = addr.saturating_add(pages.saturating_mul(4096));
    addr < kernel_va_end && map_end > kernel_va_start
}

/// Whether `mmap` should refuse this length outright with `ENOMEM`.
///
/// `sys_mmap` takes `len` from a user register and converts it to a page count that
/// drives `for i in 0..pages { … }` loops on the `MAP_FIXED` path. Without this a
/// caller could name a length spanning the address space and pin a core in the
/// kernel. Linux answers `ENOMEM` for a length it cannot map; so do we.
#[must_use]
pub const fn len_too_large(len: usize, va_limit: usize) -> bool {
    len > va_limit
}

/// The byte length `munmap` actually unmaps for a requested `len`.
///
/// **Divergence 1.** `munmap(addr, 0)` unmaps **one page** here; Linux returns
/// `EINVAL` for a zero length and unmaps nothing. Preserved, not fixed.
#[must_use]
pub const fn munmap_len(len: usize) -> usize {
    if len > 0 { (len + 4095) & !4095 } else { 4096 }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EAGER_MAX: usize = 16;

    fn anon(prot: u32, extra: u32, pages: usize) -> Plan {
        plan(prot, MAP_ANONYMOUS | MAP_PRIVATE | extra, -1, pages, EAGER_MAX)
    }
    use akuma_syscalls_linux::flags::map::MAP_PRIVATE;
    use akuma_syscalls_linux::flags::prot::PROT_READ;

    /// A `PROT_NONE` anonymous mapping is a reservation: always lazy, whatever its
    /// size. Reserving address space must not cost frames — that is the whole point
    /// of the call.
    #[test]
    fn prot_none_anonymous_is_always_lazy() {
        assert!(anon(PROT_NONE, 0, 1).is_lazy_reservation);
        assert!(anon(PROT_NONE, 0, 1).use_lazy);
        assert!(anon(PROT_NONE, 0, 1_000_000).use_lazy);
    }

    /// The size threshold, both sides of it. Small anonymous mappings stay eager so
    /// the common `malloc` arena does not pay a fault per page.
    #[test]
    fn anonymous_goes_lazy_only_above_the_eager_threshold() {
        assert!(!anon(PROT_READ | PROT_WRITE, 0, EAGER_MAX).use_lazy);
        assert!(!anon(PROT_READ | PROT_WRITE, 0, EAGER_MAX - 1).use_lazy);
        assert!(anon(PROT_READ | PROT_WRITE, 0, EAGER_MAX + 1).use_lazy);
    }

    /// `MAP_NORESERVE` forces lazy regardless of size — the caller has said it does
    /// not want the commitment.
    #[test]
    fn noreserve_forces_lazy_below_the_threshold() {
        assert!(anon(PROT_READ | PROT_WRITE, MAP_NORESERVE, 1).use_lazy);
    }

    /// `MAP_POPULATE` beats every other reason to be lazy, including `MAP_NORESERVE`
    /// and a size over the threshold. The caller asked for the pages now.
    #[test]
    fn populate_defeats_every_lazy_reason() {
        assert!(!anon(PROT_READ | PROT_WRITE, MAP_POPULATE, EAGER_MAX + 1).use_lazy);
        assert!(!anon(PROT_READ | PROT_WRITE, MAP_POPULATE | MAP_NORESERVE, 1).use_lazy);
        // …including a PROT_NONE reservation, which is otherwise unconditionally lazy.
        let p = anon(PROT_NONE, MAP_POPULATE, 1);
        assert!(p.is_lazy_reservation && !p.use_lazy);
    }

    /// A file-backed mapping never takes the anonymous lazy path, at any size:
    /// its pages need file content, not zeroes. It may still take the *file* lazy
    /// path, which is a different mechanism and a different flag.
    #[test]
    fn file_backed_never_uses_the_anonymous_lazy_path() {
        let p = plan(PROT_READ, MAP_PRIVATE, 3, 1_000_000, EAGER_MAX);
        assert!(p.is_file_backed);
        assert!(!p.use_lazy);
        assert!(p.file_lazy_eligible);
    }

    /// `fd >= 0` alone does not make a mapping file-backed — `MAP_ANONYMOUS` wins.
    /// musl passes `fd = -1` for anonymous maps but callers pass 0 too.
    #[test]
    fn anonymous_flag_beats_a_valid_fd() {
        let p = plan(PROT_READ, MAP_ANONYMOUS | MAP_PRIVATE, 7, 1, EAGER_MAX);
        assert!(!p.is_file_backed);
        assert!(!p.is_shared_writable);
    }

    /// Writable `MAP_SHARED` on a file is the writeback case: eager, and excluded
    /// from the file lazy path so every page stays resident to be flushed.
    #[test]
    fn shared_writable_file_is_eager_and_not_lazy_eligible() {
        let p = plan(PROT_READ | PROT_WRITE, MAP_SHARED, 3, 1_000_000, EAGER_MAX);
        assert!(p.is_shared_writable);
        assert!(!p.use_lazy);
        assert!(!p.file_lazy_eligible);
        // Read-only MAP_SHARED has no writes to flush, so it stays on the cheap path.
        let ro = plan(PROT_READ, MAP_SHARED, 3, 1_000_000, EAGER_MAX);
        assert!(!ro.is_shared_writable);
        assert!(ro.file_lazy_eligible);
    }

    /// `shared_anon` is the anonymous case only. File-backed `MAP_SHARED` is a
    /// different mechanism (writeback), and marking it `shared_anon` would send it
    /// down `share_rw_range` at fork instead.
    #[test]
    fn shared_anon_is_anonymous_shared_only() {
        assert!(plan(PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 1, EAGER_MAX).shared_anon);
        assert!(!plan(PROT_READ | PROT_WRITE, MAP_SHARED, 3, 1, EAGER_MAX).shared_anon);
        assert!(!plan(PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 1, EAGER_MAX).shared_anon);
    }

    /// The alignment guard fires for both fixed flavours, and only when an address
    /// was actually requested. `addr == 0` with `MAP_FIXED` means "no hint".
    #[test]
    fn fixed_unaligned_is_einval_for_both_flavours() {
        assert!(fixed_addr_unaligned_einval(0x1001, MAP_FIXED));
        assert!(fixed_addr_unaligned_einval(0x1001, MAP_FIXED_NOREPLACE));
        assert!(!fixed_addr_unaligned_einval(0x1000, MAP_FIXED));
        assert!(!fixed_addr_unaligned_einval(0, MAP_FIXED));
        // Without a fixed flag an unaligned addr is a hint, not an error.
        assert!(!fixed_addr_unaligned_einval(0x1001, MAP_PRIVATE));
    }

    /// The exact errno-shaped argument from crash14 — a sign-extended `-22` — must
    /// reach the alignment guard rather than being mistaken for a valid hint.
    #[test]
    fn fixed_unaligned_catches_the_crash14_errno_address() {
        assert!(fixed_addr_unaligned_einval(0xffff_ffff_ffff_ffea, MAP_FIXED));
        assert!(!fixed_addr_unaligned_einval(0xffff_ffff_ffff_ffea, MAP_PRIVATE));
    }

    /// Overlap is a half-open range test at both ends: a mapping ending exactly at
    /// the kernel start does not overlap, one ending a byte later does.
    #[test]
    fn kernel_va_overlap_is_half_open() {
        const S: usize = 0x4000_0000;
        const E: usize = 0x8000_0000;
        assert!(!fixed_overlaps_kernel_va(S - 0x1000, 0x1000, S, E));
        assert!(fixed_overlaps_kernel_va(S - 0x1000, 0x1001, S, E));
        assert!(!fixed_overlaps_kernel_va(E, 0x1000, S, E));
        assert!(fixed_overlaps_kernel_va(E - 0x1000, 0x1000, S, E));
        // A mapping spanning the whole window overlaps it.
        assert!(fixed_overlaps_kernel_va(0, E + 0x1000, S, E));
    }

    /// `kernel_va_end` scales with detected RAM, so the same request can be legal on
    /// a small machine and illegal on a large one. That is the reason it is a
    /// parameter and not a constant.
    #[test]
    fn kernel_va_overlap_follows_the_ram_dependent_end() {
        const S: usize = 0x4000_0000;
        let addr = 0xC000_0000;
        assert!(!fixed_overlaps_kernel_va(addr, 0x1000, S, 0x8000_0000));
        assert!(fixed_overlaps_kernel_va(addr, 0x1000, S, 0x1_0000_0000));
    }

    /// **REGRESSION GUARD (fixed 2026-08-29).** `pages * 4096` used to overflow for
    /// a huge `len`, wrapping `map_end` back to `addr` so the guard reported "no
    /// overlap" for a mapping covering the entire address space — the kernel
    /// identity map included. It now saturates and answers correctly.
    #[test]
    fn huge_len_still_reports_the_kernel_va_overlap() {
        assert!(
            fixed_overlaps_kernel_va(0x1000, usize::MAX, 0x4000_0000, 0x8000_0000),
            "a mapping spanning the address space overlaps the kernel window"
        );
        // Monotonic: growing the length can never turn an overlap into a non-overlap.
        assert!(fixed_overlaps_kernel_va(0x1000, 1 << 40, 0x4000_0000, 0x8000_0000));
        assert!(fixed_overlaps_kernel_va(0x1000, 1 << 62, 0x4000_0000, 0x8000_0000));
    }

    /// A mapping genuinely below the window still reports no overlap — the fix must
    /// not turn the guard into "always true".
    #[test]
    fn saturation_does_not_make_the_guard_unconditional() {
        assert!(!fixed_overlaps_kernel_va(0x1000, 0x1000, 0x4000_0000, 0x8000_0000));
        assert!(!fixed_overlaps_kernel_va(0x8000_0000, 0x1000, 0x4000_0000, 0x8000_0000));
    }

    /// A length larger than the whole user address space is `ENOMEM`, not a loop.
    #[test]
    fn oversized_length_is_refused() {
        const LIMIT: usize = 1 << 48;
        assert!(len_too_large(usize::MAX, LIMIT));
        assert!(len_too_large(LIMIT + 1, LIMIT));
        assert!(!len_too_large(LIMIT, LIMIT));
        assert!(!len_too_large(4096, LIMIT));
    }

    /// **Divergence 1.** `munmap(addr, 0)` unmaps one page; Linux returns `EINVAL`.
    #[test]
    fn diverge_munmap_zero_length_unmaps_one_page() {
        assert_eq!(munmap_len(0), 4096);
    }

    /// Non-zero lengths round up to a whole page.
    #[test]
    fn munmap_len_rounds_up_to_a_page() {
        assert_eq!(munmap_len(1), 4096);
        assert_eq!(munmap_len(4096), 4096);
        assert_eq!(munmap_len(4097), 8192);
    }
}
