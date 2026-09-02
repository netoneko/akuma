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
                    let page_ptr = crate::mmu::phys_to_virt(page_frame.addr);
                    // SAFETY: `page_ptr` is the kernel mapping of a page just
                    // allocated here and not yet published to any page table, so
                    // this is the only reference to it; `dst_off + read_len` is
                    // bounded by the 4 KiB page by the two clamps above.
                    let page_buf = unsafe {
                        core::slice::from_raw_parts_mut(
                            page_ptr.cast::<u8>().add(dst_off),
                            read_len,
                        )
                    };
                    let rt = crate::runtime::runtime();
                    let got = if inode == 0 {
                        (rt.read_at)(path, file_off, page_buf)
                    } else {
                        (rt.read_at_by_inode)(path, inode, file_off, page_buf)
                    };
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
            let owner_pid = crate::process::read_current_pid().unwrap_or(0);
            let owner = crate::process::lookup_process_shared(owner_pid);
            // SAFETY (map_user_page): installing a mapping for our own address
            // space at a VA this loop has just confirmed unmapped, with a frame
            // nothing else references yet.
            //
            // `as_lock_owner` (the L0-owning thread-group leader) supplies the
            // lock; the frames are tracked against `owner` (this thread's
            // Process). They are the same Process for every normal thread — only
            // a vfork-child prefault sees them differ, and then the two locks are
            // genuinely distinct so the nested hold below cannot self-deadlock.
            let (table_frames, installed) = if let (Some(leader), Some(owner)) = (as_lock_owner, owner)
                && core::ptr::eq(leader, owner)
            {
                let mut g = leader.address_space.lock();
                let (tf, inst) = unsafe { crate::mmu::map_user_page(va, page_frame.addr, map_flags) };
                if inst {
                    g.track_user_frame(page_frame);
                }
                for t in &tf {
                    g.track_page_table_frame(*t);
                }
                (tf, inst)
            } else {
                let _leader_g = as_lock_owner.map(|l| l.address_space.lock());
                let (tf, inst) = unsafe { crate::mmu::map_user_page(va, page_frame.addr, map_flags) };
                if let Some(owner) = owner {
                    let mut g = owner.address_space.lock();
                    if inst {
                        g.track_user_frame(page_frame);
                    }
                    for t in &tf {
                        g.track_page_table_frame(*t);
                    }
                }
                (tf, inst)
            };
            // Frees run after the hold is released. Ownership rule, matching
            // `exceptions.rs`'s `ensure_user_page_mapped` sibling: the data
            // frame goes back to the PMM iff the PTE CAS race was lost (nothing
            // mapped it) or there is no owner to track it. A previous version
            // freed it iff `installed && owner.is_none()`, which leaked the
            // frame on every lost CAS race with a live owner.
            let tid = akuma_primitives::preempt::current_tid() as u32;
            if !installed || owner.is_none() {
                akuma_pmm::free_page(page_frame.addr, tid);
            }
            if owner.is_none() {
                for tf in table_frames {
                    akuma_pmm::free_page(tf.addr, tid);
                }
            }
        }
        va += 4096;
    }
    true
}

