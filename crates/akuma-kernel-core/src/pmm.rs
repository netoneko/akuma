//! Shim over [`akuma_exec::pmm`], plus the one dump that could not move.
//!
//! The `PhysFrame`/`FrameSource` conversion wrappers moved to `akuma-exec` on
//! 2026-09-01 — it owns both types, and `src/syscall/` needed eight of them from
//! across a crate boundary (`docs/archive/SRC_SYSCALL_EXTRACTION.md`).
//!
//! `dp_counters_line` stayed: it reads `akuma_ext2`'s deferred-free counters,
//! which sit above `akuma-exec`, so the crate cannot see them.
//! `akuma-exceptions` takes it through `ExceptionHooks::dp_counters_line`.

pub use akuma_exec::pmm::*;

/// One-line dump of the demand-paging frame attribution counters.
///
/// Written into the caller's buffer. Takes a `&mut dyn Write` (instead of
/// returning a `String`) so it stays heap-free — this is reached from the
/// sync-EL1 crash handler, which must not touch the allocator (see
/// ALLOC_PRINT_AUDIT.md §6.3).
pub fn dp_counters_line(w: &mut dyn core::fmt::Write) {
    use core::sync::atomic::Ordering;
    let _ = write!(
        w,
        "file={} anon={} cow={} protnone={} eager={} freed={} ia_noexec={} fill_short={} unpub={} fpc_bad={} pn_file={} munmap_stale={} pf_fill_short={} pin={} pin_ovf={} defer={} defer_leak={}",
        akuma_pmm::DP_FILE_PAGES.load(Ordering::Relaxed),
        akuma_pmm::DP_ANON_PAGES.load(Ordering::Relaxed),
        akuma_pmm::DP_COW_PAGES.load(Ordering::Relaxed),
        akuma_pmm::DP_PROTNONE_PAGES.load(Ordering::Relaxed),
        akuma_pmm::EAGER_MMAP_PAGES.load(Ordering::Relaxed),
        akuma_pmm::USER_PAGES_FREED.load(Ordering::Relaxed),
        akuma_pmm::DP_IA_NOEXEC_FAULTS.load(Ordering::Relaxed),
        akuma_pmm::DP_FILE_FILL_SHORT.load(Ordering::Relaxed),
        akuma_pmm::DP_FILE_FILL_UNPUBLISHED.load(Ordering::Relaxed),
        akuma_pmm::DP_FILE_CACHE_MISMATCH.load(Ordering::Relaxed),
        akuma_pmm::DP_PROTNONE_FILE_REGION.load(Ordering::Relaxed),
        akuma_pmm::DP_MUNMAP_STALE_REGION_FRAME.load(Ordering::Relaxed),
        akuma_pmm::DP_PREFAULT_FILL_SHORT.load(Ordering::Relaxed),
        // Inode-lifecycle guards (SELFHOST_ZERO_PAGE_HUNT.md §14). `pin=` is the
        // number of inodes a live mapping is holding open — it rises and falls
        // with the build. The other three are the ones to watch: `pin_ovf=`
        // means the pin table ran out and every inode is now treated as pinned;
        // `defer=` is unlinked-but-still-mapped inodes awaiting their free, and
        // should drain to 0; `defer_leak=` is inodes leaked because the deferral
        // list was full and **must stay 0** — non-zero means raise the bound.
        akuma_primitives::inode_pin::pinned_inodes(),
        akuma_primitives::inode_pin::OVERFLOW.load(Ordering::Relaxed),
        akuma_ext2::deferred_free_pending(),
        akuma_ext2::DEFERRED_FREE_LEAKED.load(Ordering::Relaxed),
    );
}
