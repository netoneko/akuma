//! `prefault_user_range` — the demand-paging half of the user-memory boundary.
//!
//! Split from `process/user_access.rs` on 2026-09-02 when the rest of that file
//! became the `akuma-user-access` crate (`AKUMA_EXEC_AUDIT.md` §6 item A). This
//! half stayed because it needs a `Process`, the address-space lock and the
//! lazy-region table, none of which a crate below `akuma-exec` can name. It is
//! registered into `akuma_user_access::set_prefault_hook` from `akuma_exec::init`
//! and reached from there by `validate_user_range(_, Prefault::Yes)`.

use core::sync::atomic::Ordering;

/// opt-out list made `getrandom`'s prologue join that class), so the BKL no longer
/// serializes its PTE installs against a peer core. Each page's PTE install +
/// frame bookkeeping therefore runs under the address space's `as_lock`, the same
/// fix Phase 7e applied to the three signal-path PTE sites. Frame allocation and
/// the file fill stay OUTSIDE that hold: the hold masks IRQs and `as_lock` is a
/// non-reentrant `Spinlock`, so block I/O must never run under it, and the PMM's
/// pressure path (`reclaim_clean_file_pages`) re-enters `as_lock` per page.
#[must_use]
pub fn prefault_user_range(start: usize, len: usize) -> bool {
    let page_start = start & !0xFFF;
    let page_end = (start + len + 0xFFF) & !0xFFF;
    // The `as_lock` guarding THIS address space's page tables lives on the
    // thread-group leader that owns the L0 root — a CLONE_VM sibling gets a fresh
    // `as_lock` from `fork_process`, so holding the current thread's would exclude
    // nothing (`process/bkl_guard.rs`'s rule 1). Resolved once for the whole range;
    // it cannot change under us, this is our own address space.
    let as_lock_owner = crate::process::address_space_owner_pid_for_fault()
        .and_then(crate::process::lookup_process_shared);
    let mut va = page_start;
    while va < page_end {
        if !crate::mmu::is_current_user_page_mapped(va) {
            let Some((flags, source, _region_start, _region_size)) =
                crate::process::lazy_region_lookup(va)
            else {
                return false;
            };
            let map_flags = match &source {
                crate::process::LazySource::File { .. } => {
                    if flags == 0 { crate::mmu::user_flags::RW_NO_EXEC } else { flags }
                }
                _ => crate::mmu::user_flags::RW_NO_EXEC,
            };
            let Some(page_addr) = akuma_pmm::alloc_page_zeroed() else {
                return false;
            };
            let page_frame = crate::PhysFrame::new(page_addr);
            if let crate::process::LazySource::File {
                ref path, inode, file_offset, filesz, segment_va, ..
            } = source
            {
                let pg_data_start = core::cmp::max(va, segment_va);
                let pg_data_end = core::cmp::min(va + 0x1000, segment_va + filesz);
                if pg_data_start < pg_data_end {
                    let dst_off = pg_data_start - va;
                    let file_off = file_offset + (pg_data_start - segment_va);
                    let read_len = pg_data_end - pg_data_start;
                    let rt = crate::runtime::runtime();
                    // `with_phys_bytes_mut` hands `f` a `&mut [u8]` view of the
                    // freshly-allocated, not-yet-mapped frame: the only reference
                    // for its duration, and `dst_off + read_len` is bounded by
                    // the 4 KiB page by the two clamps above. `None` (frame out
                    // of range — impossible for an `alloc_page_zeroed` result)
                    // reads back as a short fill and takes the branch below.
                    let got = crate::mmu::with_phys_bytes_mut(
                        page_frame.addr,
                        dst_off,
                        read_len,
                        |page_buf| {
                            if inode == 0 {
                                (rt.read_at)(path, file_off, page_buf)
                            } else {
                                (rt.read_at_by_inode)(path, inode, file_off, page_buf)
                            }
                        },
                    )
                    .unwrap_or(Err(0));
                    // The range is already clamped to `filesz`, so anything less
                    // than `read_len` is a defect, not EOF — same contract as
                    // the demand-fault fill's `[FILL-SHORT]` in
                    // `src/exceptions.rs`. But this site is the one that
                    // instrument could never see: the page installed below is
                    // PRESENT, so no later fault re-fills it, and the zero tail
                    // of `alloc_page_zeroed`'s frame reads back as file data.
                    // The result used to be dropped (`let _ =`), which made the
                    // defect silent; count and name it instead.
                    if got != Ok(read_len) {
                        akuma_pmm::DP_PREFAULT_FILL_SHORT.fetch_add(1, Ordering::Relaxed);
                        crate::safe_print!(224,
                            "[FILL-SHORT/prefault] pid={} inode={} file_off={:#x} want={} got={:?} va={:#x} — page installed zero-filled\n",
                            crate::process::read_current_pid().unwrap_or(0), inode, file_off, read_len, got, va);
                    }
                }
            }
            // Everything above (alloc + file fill) ran with no `as_lock` held.
            // The PTE install and the frame bookkeeping must be atomic against
            // a peer core's page-table edit on this same page — in particular
            // against `reclaim_clean_file_pages`, whose `try_evict_ro_page`
            // clears a live RO PTE and only returns the frame if it is already
            // tracked. Installing outside the hold lets it observe the
            // mapped-but-untracked instant: it clears our PTE, declines to free
            // (untracked), and we then track a frame that is no longer mapped —
            // a re-fault leak. One hold per page, never spanning the loop.
            //
            // The installed L0 is the thread-group leader's — every `CLONE_VM`
            // sibling *and* a vfork child share it — so the lock and the frame
            // tracking both go there: the page lives in the leader's L0 and is
            // the leader's to free. That collapses what used to be a common
            // path plus a raw `unsafe { map_user_page }` "lock A, record in B"
            // arm for the vfork edge (`AKUMA_EXEC_AUDIT.md` §6.E group 3).
            // `map_user_page_tracked` re-checks `installed_l0 == self.l0_phys()`
            // and, on a lost CAS race, leaves the frame tracked to be freed at
            // teardown rather than here.
            let Some(leader) = as_lock_owner else {
                // No resolvable address-space owner (a dying process) — fail
                // closed, the caller gets EFAULT rather than a page in a table
                // we could not lock. Unreachable on the syscall prefault path.
                akuma_pmm::free_page(page_frame.addr, akuma_primitives::preempt::current_tid() as u32);
                return false;
            };
            let _installed = leader.address_space.lock().map_user_page_tracked(va, page_frame, map_flags);
        }
        va += 4096;
    }
    true
}

