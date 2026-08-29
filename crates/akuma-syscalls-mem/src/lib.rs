//! The memory-syscall family's pure decisions.
//!
//! `mmap` / `munmap` / `mremap` / `madvise` / `membarrier` each begin by deciding
//! something from their arguments alone, then spend the rest of the call acting on
//! that decision. This crate is the deciding half; `src/syscall/mem.rs` is the
//! acting half.
//!
//! # What is in here
//!
//! - [`mmap::plan`] — the mapping-kind decision: lazy vs eager, file-backed vs
//!   anonymous, shared-writable, `shared_anon`. Pure over `(prot, flags, fd, pages,
//!   eager_max_pages)`.
//! - [`mmap::fixed_addr_unaligned_einval`] / [`mmap::fixed_overlaps_kernel_va`] —
//!   `MAP_FIXED` validation.
//! - [`mmap::munmap_len`] — `munmap`'s sizing, including a Linux divergence.
//! - [`mremap::plan`] — the shrink short-circuit and the `MREMAP_MAYMOVE`-absent
//!   `ENOMEM`-vs-`EFAULT` split.
//! - [`madvise::action`] — the advice decode, including the deliberate
//!   `MADV_FREE` → `EINVAL`.
//! - [`madvise::dontneed_zero_range`] / [`madvise::dontneed_page_action`] —
//!   `MADV_DONTNEED`'s range and per-page rules.
//! - [`membarrier::command`] — the command decode.
//!
//! # What is deliberately NOT in here
//!
//! Everything with an effect, and everything needing state the arguments do not
//! carry. Specifically:
//!
//! - **No `Process` lookup, no lock, no frame, no page-table edit, no user-memory
//!   access.** The crate has two dependencies, both leaves, so none of it is
//!   reachable.
//! - **No `MmapRegion`.** This crate deliberately does *not* depend on
//!   `akuma-mmap`, and that is the load-bearing claim: the mapping-kind decision is
//!   a function of the argument bits, and it never sees a region list. If a future
//!   change makes this crate want `MmapRegion`, the seam is drawn in the wrong
//!   place and the fix is to move the seam.
//! - **The probes themselves.** [`mremap::Plan::Grow`] hands back `may_move` so the
//!   kernel can run the "is `old_addr` mapped?" probe **only when `MREMAP_MAYMOVE`
//!   is absent**, exactly as the pre-extraction code did. That probe is three
//!   lookups and a `vm_lock` acquisition; running it unconditionally would be a
//!   lock per call, not a style difference.
//! - **`dontneed_count_shared` / `dontneed_apply`.** They take a live
//!   `UserAddressSpace` and mutate page tables. Their own doc comments record that
//!   they were split out so the *boot suite* could drive them against a real
//!   CoW-shared frame; that decision stands.
//!
//! # Known divergences from Linux
//!
//! Preserved and pinned, not fixed — an extraction that quietly fixes something
//! cannot be A/B'd against what it replaced. Each has a test named to say what it
//! is. See `docs/reference/subsystems/syscalls/mem.md`.
//!
//! 1. [`mmap::munmap_len`] — `munmap(addr, 0)` unmaps **one page**; Linux returns
//!    `EINVAL`.
//! 2. [`madvise::dontneed_zero_range`] — an unaligned start rounds **down**, so the
//!    cleared range is a strict superset of Linux's, including the caller's partial
//!    head page. Linux rejects an unaligned start with `EINVAL`.
//! 3. [`madvise::action`] — `MADV_FREE` returns `EINVAL` (deliberate; Redis reads it
//!    correctly, and fabricating success sent it into a self-check it cannot pass).
//! 4. [`madvise::action`] — every other advice returns success without doing
//!    anything, including ones Linux implements.
//! 5. [`mremap::plan`] — a shrink returns the **old address** without unmapping the
//!    tail; Linux unmaps it.
//! 6. [`mmap::plan`] — `MAP_FIXED_NOREPLACE` is validated for alignment but then
//!    treated as `MAP_FIXED` without the "fail if occupied" check.
//! 7. [`membarrier::command`] — only three commands are recognised; Linux has more,
//!    and `MEMBARRIER_CMD_GLOBAL` (1) is among the unrecognised.
//!
//! # Testing
//!
//! Host tests only — that is the point. Every decision above previously cost a QEMU
//! boot to exercise.
//!
//! ```text
//! cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
//! ```

#![forbid(unsafe_code)]
#![no_std]

pub mod madvise;
pub mod membarrier;
pub mod mmap;
pub mod mremap;
