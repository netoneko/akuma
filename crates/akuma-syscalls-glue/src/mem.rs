use super::*;
use akuma_primitives::{GuardToggle, ToggledGuard};
use akuma_exec::process::MmapRegion;

/// The `no-bkl-mm` carve-out (Phase 5 of
/// docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md), as a [`GuardToggle`] marker.
///
/// Correctness rests on the state `sys_mprotect`/`sys_madvise`/`sys_munmap`/
/// `sys_mremap`/`sys_mmap` mutate already carrying its own fine-grained lock —
/// `Process::as_lock` (page-table PTE edits, via `Process::with_address_space`),
/// `Process::vm_lock` (`mmap_regions`, which is also what VA placement reads, via
/// `vm_with_regions`/`vm_alloc_mmap`; lazy regions are in `mmap_regions` too), PMM/
/// `FRAME_TRACKER` (never held across a yield), and `akuma_exec::shm`'s table — so
/// the BKL is redundant for them. Two gaps this phase's audit found (a plain
/// unguarded mmap free list, since removed, and `sys_mmap`'s OOM/reclaim sweep
/// mutating page tables with no address-space-lock hold) were closed as a
/// prerequisite; see `Process::vm_alloc_mmap` and
/// `reclaim_clean_file_pages`'s per-page `proc.address_space.lock()`.
///
/// Unlike the VFS/net carve-outs, none of `as_lock`/`vm_lock`/
/// PMM need to know a BKL-free window is calling them — their IRQ-masking is
/// already unconditional (not gated on being reachable from a dropped-BKL window
/// the way `PreemptGuard` is, see BKL_VFS_CARVE_OUT.md §19.3), so there is no
/// AB-BA nested-IRQ hazard analogous to the one that gate exists to close.
pub(super) struct MmBkl;

impl GuardToggle for MmBkl {
    const COMPILED_IN: bool = cfg!(kernel_bkl_smp);
    #[inline]
    fn enabled() -> bool {
        #[cfg(kernel_bkl_smp)]
        {
            akuma_bkl::policy::mm_bkl_drop_enabled()
        }
        #[cfg(not(kernel_bkl_smp))]
        {
            false
        }
    }
    #[inline]
    fn enter() {
        akuma_exec::bkl::dropped_window_open();
    }
    #[inline]
    fn exit() {
        akuma_exec::bkl::dropped_window_close();
    }
}

/// RAII guard that runs a memory-management syscall **without** the Big Kernel Lock.
///
/// Constructed at the top of `sys_mprotect`/`sys_madvise`/`sys_munmap`/
/// `sys_mremap`/`sys_mmap` — but **after** the early-error/arg-validation returns: an
/// early `EINVAL` on a malformed length never touches the state this guard exists to
/// protect, so it shouldn't pay for a drop+reacquire. See [`MmBkl`] for why dropping
/// the lock there is safe.
pub(super) type MmBklGuard = ToggledGuard<MmBkl>;

// ── Linux mmap flag constants ────────────────────────────────────────────────
//
// Lifted from `sys_mmap` to module scope so the same bits are used by both
// `sys_mmap` and the diagnostic helpers below; moved on from there to
// `akuma-syscalls-linux` on 2026-08-27, which is where the "values match Linux
// AArch64" claim is now actually checked. Re-exported so
// `crate::mem::MAP_FIXED` (kernel tests) keeps its spelling.

pub use akuma_syscalls_linux::flags::map::{
    MAP_ANONYMOUS, MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_NORESERVE, MAP_POPULATE, MAP_PRIVATE,
    MAP_SHARED, MAP_STACK,
};

// ── `MADV_DONTNEED` share-breaking audit ─────────────────────────────────────
//
// Akuma's `MADV_DONTNEED` used to zero the *physical frame* in place. That agrees
// with Linux for a page owned by one address space and disagrees — destructively —
// for a shared one: the peer's live page went to zeroes too. That was the
// null-`Rc` mechanism (docs/archive/CARGO_HEAP_NULL_RC.md), demonstrated
// deterministically by `userspace/forktest/c_stress/madvshared.c` and fixed
// 2026-08-14 by [`dontneed_page_action`] — a shared page now gets a private zero
// frame of its own and the peer keeps its data.
//
// These counters stay because they are how the fix is observed from the outside:
// `dontneed_share_break` climbing on a fork-heavy workload is the corruption that
// is no longer happening.

/// `MADV_DONTNEED` calls whose start address was not page-aligned.
///
/// Linux rejects these with `EINVAL`; rounding the start DOWN pulls the
/// caller's live head page into the zeroed range. Still unfixed — deliberately
/// a separate cycle (`CARGO_HEAP_NULL_RC.md` § "The fix", follow-on 1); it has
/// never read non-zero.
pub static DONTNEED_UNALIGNED: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Pages that took the share-breaking path instead of being zeroed in place.
    ///
    /// Their frame was shared with another address space (`cow_ref >= 2` — a
/// post-fork CoW page or a `file_page_cache` page) and which therefore took the
/// share-breaking path instead of being zeroed in place.
///
/// Before 2026-08-14 every one of these wiped a frame some other address space
/// still maps.
pub static DONTNEED_SHARED_FRAME: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Shared pages left **untouched** for want of a replacement frame.
    ///
    /// Either
/// the PMM had none (`MADV_DONTNEED` is advisory, so failing to zero beats
/// wiping a peer), or a fork landed between this handler's classify and apply
/// passes and made a page shared that was private a moment earlier.
///
/// Expected 0; a climbing value means memory pressure is reaching this path.
pub static DONTNEED_SHARE_BREAK_SKIPPED: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// `MADV_DONTNEED` pages that were **file-backed**, and so had their mapping
/// dropped (Linux's behaviour) instead of being zeroed in place or replaced
/// with a private zero frame.
///
/// Before this, both of the other actions made an mmap'd file read back as
/// zeros permanently — see `MADV_DONTNEED_SHARED_FRAME.md` "Still open" item 2,
/// and `FPCACHE_ZERO_PAGE_POISONING.md` for the hunt that ended here.
pub static DONTNEED_FILE_BACKED: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// `MADV_DONTNEED`'s range rule and per-page rule, re-exported from
/// `akuma-syscalls-mem`.
///
/// Both are functions of their arguments, so they moved to the crate with the rest
/// of the family's decisions and took a wider set of host tests with them —
/// including the two overflow defects the move found (see the crate's
/// `preserved_overflow_*` tests). `DontneedAction` keeps its kernel-side spelling so
/// the ~250 lines of handler below and the boot suite are unchanged.
///
/// `dontneed_count_shared` and `dontneed_apply` deliberately did NOT move: they take
/// a live `UserAddressSpace` and mutate page tables, and the defect they exist to
/// catch is cross-address-space, which no host test can see.
pub use akuma_syscalls_mem::madvise::{
    PageAction as DontneedAction, dontneed_page_action, dontneed_zero_range,
};

/// One-line summary of the `MADV_DONTNEED` audit counters for the PSTATS block.
/// Writes into the caller's buffer instead of returning a `String` — same
/// heap-free rationale as `file_page_cache::stats_line`.
pub fn dontneed_audit_line(w: &mut dyn core::fmt::Write) {
    use core::sync::atomic::Ordering;
    let _ = writeln!(
        w,
        "[MADV] dontneed_unaligned={} dontneed_shared_frame={} dontneed_skipped={}",
        DONTNEED_UNALIGNED.load(Ordering::Relaxed),
        DONTNEED_SHARED_FRAME.load(Ordering::Relaxed),
        DONTNEED_SHARE_BREAK_SKIPPED.load(Ordering::Relaxed),
    );
}

/// One writable `MAP_SHARED` file region, captured for write-back.
///
/// Every mapper maps **one frame** per
/// file page, from the shared writable table (`akuma_exec::shm`), so a store is
/// visible to every other mapper at once. The region's `shared_write` record
/// names the file, and its pages are written back on `munmap`, `msync`,
/// `MADV_DONTNEED`, `execve` and exit. `msync` is the POSIX flush point linkers
/// like `rust-lld` use right before renaming their output buffer
/// (docs/AKUMA_SELF_HOSTING.md §7d). Both kernels since 2026-11-12
/// (`docs/archive/AKUMA_ONE_SHARED_WRITABLE_TABLE.md`); AArch64 used to give each
/// mapper private frames and write them back, so two mappers never saw each
/// other's stores.
struct WriteBack {
    rec: akuma_mmap::SharedWriteBack,
    start_va: usize,
    end_va: usize,
}

/// Every writable `MAP_SHARED` file region overlapping `[start, end)`, captured
/// under the region lock. Allocates only when there is one.
fn snapshot_shared_write(proc: &akuma_exec::process::Process, start: usize, end: usize) -> Vec<WriteBack> {
    proc.vm_with_regions(|r| {
        akuma_exec::process::regions_overlapping(r, start, end)
            .iter()
            .filter(|x| x.start_va < end)
            .filter_map(|x| {
                x.shared_write.as_ref().map(|sw| WriteBack {
                    rec: sw.clone(),
                    start_va: x.start_va,
                    end_va: x.start_va + x.len_bytes(),
                })
            })
            .collect()
    })
}

/// Write back the present pages of `w` inside `[lo, hi)`.
///
/// Bounded by the file's size **now**, not at `mmap`: parity-db grows a file
/// with `ftruncate` and writes through the reserve past the old end, and a page
/// past the current end must not extend the file. Through the recorded path when
/// it still names this inode, else by inode — after an `unlink` (SQLite deletes
/// its `-shm` before unmapping it) or an unlink-and-recreate, the path is gone or
/// names another file, and the mapping's pin keeps the inode alive
/// (`chromeprobe`'s shmvar c/d). One page is translated at a time and copied
/// out before the write, so no page-table lock is held across I/O.
fn flush_shared_write(proc: &akuma_exec::process::Process, w: &WriteBack, lo: usize, hi: usize) {
    let rec = &w.rec;
    let Ok(meta) = akuma_vfs_glue::fs::metadata_open_file(&rec.path, rec.mount_id, rec.inode) else { return };
    let size = meta.size as usize;
    if size <= rec.offset {
        return;
    }
    let by_path = akuma_vfs_glue::resolve_file_id(&rec.path) == Some((rec.mount_id, rec.inode));
    let hi = hi.min(w.end_va).min(w.start_va.saturating_add(size - rec.offset));
    let mut failed = 0usize;
    let mut va = lo.max(w.start_va) & !0xFFF;
    while va < hi {
        if let Some(pa) = proc.with_address_space(|a| a.translate(va)) {
            let off = rec.offset + (va - w.start_va);
            // Written from the frame itself, never a copy: the write's
            // `shm::wrote` then sees its source is the frame and skips copying
            // it back. A snapshot copied back would undo every store a peer
            // made since it was taken (`shmwrite` rung 10 lost increments).
            let ok = akuma_exec::mmu::with_phys_bytes_mut(pa & !0xFFF, 0, (size - off).min(4096), |bytes| {
                if by_path {
                    akuma_vfs_glue::write_at(&rec.path, off, bytes).is_ok()
                } else {
                    akuma_vfs_glue::fs::write_at_open_file(rec.mount_id, rec.inode, off, bytes).is_ok()
                }
            });
            failed += usize::from(ok != Some(true));
        }
        va += 4096;
    }
    if failed > 0 {
        akuma_primitives::safe_print!(96, "[MM-WB] {} write-back page(s) FAILED\n", failed);
    }
}

/// Write back, then drop, this address space's writable `MAP_SHARED` file pages
/// in `[start, end)`.
///
/// `munmap`'s and `MADV_DONTNEED`'s first step. Once unmapped, a page's bytes exist only in the
/// table, so the flush comes first; then each entry whose frame this address
/// space just gave up is reaped if nobody else maps it. The next touch (after `DONTNEED`) faults the page back in.
pub(super) fn release_shared_write(proc: &akuma_exec::process::Process, start: usize, end: usize) {
    let snapshot = snapshot_shared_write(proc, start, end);
    if snapshot.is_empty() {
        return;
    }
    for w in &snapshot {
        flush_shared_write(proc, w, start, end);
        let (lo, hi) = (w.start_va.max(start), w.end_va.min(end));
        let mut released = Vec::new();
        proc.with_address_space(|a| a.unmap_present(lo, hi, &mut released));
        for &frame in &released {
            akuma_exec::pmm::free_page_at(frame, akuma_pmm::FreeSite::MunmapRegion);
        }
        let mut pas: Vec<usize> = released.iter().map(|f| f.addr).collect();
        pas.sort_unstable();
        let from = w.rec.offset + (lo - w.start_va);
        akuma_exec::shm::reap_released(w.rec.mount_id, w.rec.inode, from, from + (hi - lo), &pas);
    }
}

/// Unmap every writable `MAP_SHARED` file mapping of `proc`, as `munmap` would.
///
/// For `execve` and exit, whose address-space teardown would neither write the
/// pages back nor release the table's entries. Call with the other threads gone
/// where the caller can, so nothing faults a page back in behind the walk.
pub fn release_shared_write_mappings(proc: &akuma_exec::process::Process) {
    let extents: Vec<(usize, usize)> = proc.vm_with_regions(|r| {
        r.iter()
            .filter(|x| x.shared_write.is_some())
            .map(|x| (x.start_va, x.start_va + x.len_bytes()))
            .collect()
    });
    for (start, end) in extents {
        unmap_range(proc, start, end);
    }
}

/// Does `uaddr` fall inside a writable `MAP_SHARED` file mapping of `tgid`?
///
/// The memory genuinely shared between address spaces by a file, which
/// `futex_key_tgid` uses to decide whether a non-private futex may share a key
/// namespace with another process — see its doc comment for why keying every
/// non-private op globally was a cross-process lost wakeup.
pub(super) fn is_shared_file_mapping(tgid: u32, uaddr: usize) -> bool {
    akuma_exec::process::lookup_process_shared(tgid).is_some_and(|p| {
        p.vm_with_regions(|r| {
            akuma_exec::process::region_index_containing(r, uaddr).is_some_and(|i| r[i].shared_write.is_some())
        })
    })
}

/// `msync(addr, len, flags)`: write back the shared writable file mappings in range.
///
/// Synchronously and in full (no dirty
/// tracking: I/O spent, never bytes miswritten). `flags` is accepted —
/// `MS_ASYNC` is a no-op on Linux too, and there is no other cache to
/// invalidate against. A range with no such mapping is `0` without I/O.
pub(super) fn sys_msync(addr: usize, len: usize, _flags: u32) -> u64 {
    if addr == 0 || addr & 0xFFF != 0 {
        return EINVAL;
    }
    if len == 0 {
        return 0;
    }
    let Some(proc) = current_owner() else { return ESRCH };
    let end = addr.saturating_add(len);
    for w in &snapshot_shared_write(proc, addr, end) {
        flush_shared_write(proc, w, addr, end);
    }
    0
}

/// `MAP_FIXED` alignment validation, re-exported from `akuma-syscalls-mem`.
///
/// Called *before* `lookup_process` in `sys_mmap`, deliberately: a kernel-test
/// caller with no current task must get `EINVAL`, not `ESRCH`. That ordering is
/// what `test_mmap_einval_through_handle_syscall` asserts, and it stays in the boot
/// suite because it is a property of where the call sits, not of what it returns.
pub use akuma_syscalls_mem::mmap::fixed_addr_unaligned_einval as mmap_fixed_addr_unaligned_einval;

/// Whether a `MAP_FIXED` mapping would overlap the kernel identity-map VA range.
///
/// Thin forwarder: the crate takes the window as arguments because `kernel_va_end()`
/// **scales with detected RAM**, so the guard cannot be expressed as a constant.
/// Supplying it here keeps the crate a leaf and lets the host tests sweep RAM sizes,
/// which is how the overflow defect in it became visible.
#[must_use]
pub fn mmap_fixed_overlaps_kernel_va(addr: usize, len: usize) -> bool {
    let hole = akuma_exec::process::types::ProcessMemory::kernel_hole();
    !hole.is_empty() && akuma_syscalls_mem::mmap::fixed_overlaps_kernel_va(addr, len, hole.start, hole.end)
}

pub(super) fn sys_brk(new_brk: usize) -> u64 {
    let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process_shared(current_pid).map_or(current_pid, |p| p.tgid);
    if let Some(proc) = akuma_exec::process::lookup_process_shared(owner_pid) {
        if new_brk == 0 { proc.get_brk() as u64 } else { proc.set_brk(new_brk) as u64 }
    } else { 0 }
}

/// Resolve `(inode, filesz)` for a file-backed lazy region.
///
/// **`filesz` describes FILE data, not the mapping**, and the difference is a
/// correctness boundary rather than a detail. `mmap` may legally map more than the
/// file holds — the tail past EOF reads as zeros for *this* mapping and SIGBUSes on
/// write — so a page beyond EOF has a zero-fill tail whose length belongs to the
/// mapping, not to the file. `file_page_cache`'s eligibility rules say exactly that
/// ("Fully covered by file data … two mappers can legitimately disagree about its
/// contents"), and the fault path decides *fully covered* by testing against `filesz`.
///
/// Passing the mmap length here defeated that test: every page between EOF and the
/// end of the mapping was classed as fully covered, read short (`read_at_by_inode`
/// clamps to the real size and returns `Ok(0)` past it), left zeroed by
/// `alloc_pages_zeroed` — and then **published to the shared cache under
/// `(inode, file_off)`**, making those zeros authoritative for every other process
/// that maps the same file. That is the ODHT-header corruption in
/// `docs/archive/FPCACHE_ZERO_PAGE_POISONING.md`.
///
/// If the size cannot be read, the mapping still works (the read extent falls back to
/// `len`) but `inode` is zeroed, which disables both `lookup_and_ref` and `insert` —
/// without a size there is no way to tell a real page from a past-EOF one, and
/// refusing to share is the safe direction.
///
/// The **mount id** rides along with the inode for the same reason it does on an
/// fd: an inode number is ambiguous across two mounted filesystems, and this pair
/// is what the page cache keys on (F-1,
/// `docs/archive/EXT2_WRITEBACK_DESIGN.md`). Resolved together, from one call, so
/// the two can never describe different mounts.
fn resolve_file_extent(path: &str, fd_mount: u32, fd_inode: u32, offset: usize, len: usize) -> (u32, u32, usize) {
    // The fd's own identity wins over the path's: a file unlinked after `open`
    // has no path to resolve, and "no identity" meant a shared writable mapping
    // got private pages — Chromium's font bytes, written into an unlinked
    // shared-memory file, never reached the renderer (`shmregionprobe.c`,
    // ryzen 2026-10-08).
    if fd_inode != 0
        && let Ok(m) = akuma_vfs_glue::metadata_open_file(path, fd_mount, fd_inode)
    {
        return (fd_mount, fd_inode, core::cmp::min(len, (m.size as usize).saturating_sub(offset)));
    }
    match akuma_vfs_glue::file_size(path) {
        Ok(file_len) => {
            let (mount_id, inode) = akuma_vfs_glue::resolve_file_id(path).unwrap_or((0, 0));
            (
                mount_id,
                inode,
                core::cmp::min(len, (file_len as usize).saturating_sub(offset)),
            )
        }
        Err(_) => (0, 0, len),
    }
}

/// Fallback when an eager mmap can't get its frames even after reclaiming clean
/// file pages: reserve the region lazily (demand-paged) instead of returning
/// ENOMEM. A lazy region is just a VA reservation that always succeeds; its pages
/// fault in later through the reclaim-aware fault path. Safe for both anonymous
/// and file-backed mappings. Returns the mapped address (or ENOMEM only if a
/// file-backed fd can't be resolved).
fn mmap_eager_to_lazy_fallback(
    proc: &akuma_exec::process::Process,
    is_file_backed: bool, fd: i32, offset: usize, len: usize,
    mmap_addr: usize, pages: usize, page_flags: u64,
) -> u64 {
    if is_file_backed {
        if let Some(akuma_exec::process::FileDescriptor::File(ref f)) = proc.get_fd(fd as u32) {
            let path = f.path.clone();
            // On-disk metadata read — dropped-BKL window like the other read paths
            // (Phase 2e of the no-bkl-vfs carve-out).
            let (mount_id, inode, filesz) = {
                let _vfs_window = super::fs::VfsBklGuard::new();
                resolve_file_extent(&path, f.mount_id(), f.inode(), offset, len)
            };
            // A lazy file region reads by `(mount, inode)` only; a file without
            // that identity has nothing to demand-page from.
            if inode == 0 || mount_id == 0 {
                akuma_pmm::note_user_enomem("mmap/eager-file", len);
                proc.vm_release_reservation(mmap_addr, pages * 4096);
                return ENOMEM;
            }
            let source = akuma_exec::process::LazySource::file(
                mount_id, inode, offset, filesz, mmap_addr,
            );
            let count = akuma_exec::process::push_lazy_region_with_source(
                proc.tgid, mmap_addr, pages * 4096, page_flags, source);
            if akuma_config::MEM_SYSCALL_TRACE_ENABLED {
                akuma_primitives::tprint!(128, "[mmap] eager OOM -> lazy-file fallback pid={} pages={} ({} regions)\n",
                proc.pid, pages, count);
            }
            return mmap_addr as u64;
        }
        akuma_pmm::note_user_enomem("mmap/eager-file", len);
        proc.vm_release_reservation(mmap_addr, pages * 4096);
        return ENOMEM;
    }
    let count = akuma_exec::process::push_lazy_region(proc.tgid, mmap_addr, pages * 4096, page_flags);
    if akuma_config::MEM_SYSCALL_TRACE_ENABLED {
        akuma_primitives::tprint!(128, "[mmap] eager OOM -> lazy fallback pid={} pages={} ({} regions)\n",
        proc.pid, pages, count);
    }
    mmap_addr as u64
}

/// Can this path's bytes be mapped? Not a `/dev` node (no inode holds its
/// bytes: a mapping would be zeros posing as the file) and not a directory.
fn mappable_path(path: &str) -> bool {
    let dev_node = path.starts_with("/dev/") && akuma_vfs_glue::dev_node(path).is_some();
    let dir = akuma_vfs_glue::metadata(path).is_ok_and(|m| m.is_dir);
    !dev_node && !dir
}

pub(super) fn sys_mmap(addr: usize, len: usize, prot: u32, flags: u32, fd: i32, offset: usize) -> u64 {
    if len == 0 { return EINVAL; }
    // A length larger than the user address space cannot be mapped, and must be
    // refused BEFORE it is turned into a page count — `pages` drives
    // `for i in 0..pages` loops on the MAP_FIXED path below, so an unchecked
    // `len` was an unbounded loop reachable from userspace. Linux answers ENOMEM.
    // See docs/archive/AKUMA_EXTRACT_MMAP.md §10.1 defect B.
    if akuma_syscalls_mem::mmap::len_too_large(len, user_va_limit() as usize) {
        return ENOMEM;
    }
    let pages = len.div_ceil(4096);
    let byte_len = pages * 4096;
    // The region's neutral record and the page table's AArch64 encoding — one
    // `u64` until 2026-09-06, two vocabularies since. See `akuma_mmap::types`.
    let page_prot = akuma_exec::mmu::Prot::from_prot(prot);
    let page_flags = akuma_exec::mmu::user_flags::to_pte(page_prot);

    let _ = MAP_STACK; // silence unused-import lint; flag accepted but ignored

    // The mapping-kind decision — lazy vs eager, file-backed vs anonymous,
    // shared-writable, shared-anon — is a function of the argument bits and the page
    // count, so it lives in `akuma-syscalls-mem` where it is host-tested against the
    // threshold, `MAP_POPULATE`'s precedence and the `MAP_SHARED` matrix. Everything
    // below acts on it.
    let plan = akuma_syscalls_mem::mmap::plan(
        prot, flags, fd, pages, akuma_config::MMAP_EAGER_MAX_PAGES,
    );
    // `MAP_FIXED_NOREPLACE` wins over `MAP_FIXED`, as on Linux.
    let is_fixed_noreplace = flags & MAP_FIXED_NOREPLACE != 0;
    let is_fixed = flags & MAP_FIXED != 0 && !is_fixed_noreplace;
    let _ = MAP_POPULATE; // consumed by `plan`; subsumed by the lazy fallback below

    // Everything decidable from the arguments is refused before the process
    // lookup, so a kernel-test caller with no current task sees Linux's errno
    // rather than ESRCH (asserted by both boot suites).
    if mmap_fixed_addr_unaligned_einval(addr, flags) {
        return EINVAL;
    }
    if is_fixed || is_fixed_noreplace {
        // The null page is never a real fixed placement (it would make a null
        // dereference succeed), and a fixed range must stay in the user half.
        // Saturating: `len` is a user register, and a wrap would read as "fits".
        if addr < 4096 || addr.saturating_add(byte_len) > akuma_exec::mmu::USER_VA_END {
            return EINVAL;
        }
        // The AArch64 kernel's RAM identity map: the Go runtime commits its
        // heap arenas with MAP_FIXED, and a user page there corrupts the kernel.
        if mmap_fixed_overlaps_kernel_va(addr, byte_len) {
            if akuma_config::MEM_SYSCALL_TRACE_ENABLED {
                akuma_primitives::tprint!(128, "[mmap] REJECT MAP_FIXED kernel VA: addr=0x{:x} len=0x{:x}\n",
                    addr, byte_len);
            }
            return EINVAL;
        }
    }
    if plan.is_file_backed && offset & 0xFFF != 0 {
        return EINVAL;
    }

    let Some(proc) = current_owner() else { return ESRCH };

    // The descriptor, for a file mapping: what to read, and whether it may be
    // mapped at all. A socket, pipe, directory or `/dev` node has no bytes to
    // map; serving zeros instead is the silent lie this refuses. `/dev/zero`
    // is the exception Linux makes: anonymous memory.
    let file: Option<(alloc::string::String, u32, u32)> = if plan.is_file_backed {
        match proc.get_fd(fd as u32) {
            Some(akuma_exec::process::FileDescriptor::File(f)) => {
                if !mappable_path(&f.path) {
                    return EACCES;
                }
                if plan.is_shared_writable && !akuma_syscalls_linux::flags::open::may_write(f.flags) {
                    return EACCES;
                }
                Some((f.path.clone(), f.mount_id(), f.inode()))
            }
            Some(akuma_exec::process::FileDescriptor::DevZero) => None,
            _ => return EACCES,
        }
    } else {
        None
    };
    // A writable `MAP_SHARED` file mapping is served from the shared writable
    // table, keyed by inode: with no inode there is nothing to share or write
    // back to, and that is a refusal rather than private pages posing as the file.
    let mut shared_write = match &file {
        Some((path, mount_id, inode)) if plan.is_shared_writable => {
            if *inode == 0 {
                return EACCES;
            }
            Some(akuma_mmap::SharedWriteBack { mount_id: *mount_id, inode: *inode, offset, path: path.clone() })
        }
        _ => None,
    };
    let _mm_window = MmBklGuard::new();

    let reservation = || MmapRegion::demand_paged(addr, pages, Some(akuma_exec::mmu::Prot::NONE));
    let mmap_addr = if is_fixed {
        // Whatever was mapped here goes, exactly as `munmap` would remove
        // it, and a reservation takes its place so the placer cannot hand
        // the range to a sibling while this call fills it.
        unmap_range(proc, addr, addr + byte_len);
        proc.vm_install_region(reservation());
        addr
    } else if is_fixed_noreplace {
        // Exactly `addr`, or EEXIST if anything is there: a region, or a page
        // no region records. The page walk is capped at 256 MiB; past that
        // only the region list is asked.
        const WALK_CAP_PAGES: usize = 65_536;
        let mapped = proc.with_address_space(|a| (0..pages.min(WALK_CAP_PAGES)).any(|i| a.is_mapped(addr + i * 4096)));
        let end = addr + byte_len;
        let reserved = !mapped && proc.vm_with_regions(|r| {
            let clear = !akuma_exec::process::regions_overlapping(r, addr, end)
                .iter()
                .any(|x| x.start_va < end && addr < x.start_va + x.len_bytes());
            if clear {
                akuma_exec::process::insert_region_sorted(r, reservation());
            }
            clear
        });
        if !reserved {
            return EEXIST;
        }
        addr
    } else if let Some(a) = proc.vm_alloc_mmap(byte_len) { a } else {
        if akuma_config::MEM_SYSCALL_TRACE_ENABLED {
            akuma_primitives::safe_print!(192, "[mmap] REJECT: pid={} size=0x{:x} next=0x{:x} limit=0x{:x}\n",
            proc.pid, byte_len,
            proc.memory.next_mmap.load(core::sync::atomic::Ordering::Relaxed),
            proc.memory.mmap_limit.load(core::sync::atomic::Ordering::Relaxed));
        }
        return ENOMEM;
    };

    let is_file_backed = file.is_some();

    // Anonymous private mappings above MMAP_EAGER_MAX_PAGES are demand-paged
    // (zero-fill on first touch); MAP_POPULATE suppresses that (`plan`).
    //
    // **A `MAP_SHARED|MAP_ANONYMOUS` mapping is never lazy.** Fork shares it by
    // identity by walking the parent's present pages; a page not faulted in
    // yet is not one, so each side would demand-page a private frame and the
    // mapping would behave as `MAP_PRIVATE`. Sharing a page that does not exist
    // yet needs a backing object neither kernel has, so it is populated now.
    if plan.use_lazy && !plan.shared_anon {
        let count = akuma_exec::process::push_lazy_region(proc.tgid, mmap_addr, byte_len, page_flags);
        if akuma_config::MEM_SYSCALL_TRACE_ENABLED {
            akuma_primitives::tprint!(192, "[mmap] pid={} len=0x{:x} prot=0x{:x} flags=0x{:x} = 0x{:x} (lazy, {} regions)\n",
                proc.pid, len, prot, flags, mmap_addr, count);
        }
        return mmap_addr as u64;
    }

    // Demand-page file mappings (MMAP_FILE_BACKED_LAZY, on by default): eagerly
    // mapping a large shared library costs its whole size before the process
    // can start. A lazy file region reads by `(mount, inode)` only, so a file
    // without that identity (no inode, or a size that would not read) takes the
    // eager path below, which reads once at mmap time.
    if akuma_config::MMAP_FILE_BACKED_LAZY && plan.file_lazy_eligible
        && let Some((path, fd_mount, fd_inode)) = &file
        && let (mount_id, inode, filesz) = {
            // Path→inode resolution reads ext2 metadata (real I/O on a cold cache) —
            // take the dropped-BKL window for it like every other on-disk read path
            // (Phase 2e of the no-bkl-vfs carve-out).
            let _vfs_window = super::fs::VfsBklGuard::new();
            resolve_file_extent(path, *fd_mount, *fd_inode, offset, len)
        }
        && inode != 0 && mount_id != 0 {
            let source = akuma_exec::process::LazySource::file(mount_id, inode, offset, filesz, mmap_addr);
            let region = akuma_exec::process::lazy_mmap_region(mmap_addr, byte_len, page_flags, &source);
            let shared = shared_write.is_some();
            let region = match shared_write.take() {
                Some(rec) => region.shared_writable(rec),
                None => region,
            };
            let count = akuma_exec::process::push_lazy_mmap_region(proc.tgid, region);
            if akuma_config::MEM_SYSCALL_TRACE_ENABLED {
                akuma_primitives::tprint!(192, "[mmap] pid={} fd={} file={} off={} len=0x{:x} = 0x{:x} (lazy-file{}, {} regions)\n",
                    proc.pid, fd, path, offset, len, mmap_addr,
                    if shared { ", shared-writable" } else { "" }, count);
            }
            return mmap_addr as u64;
        }
    if shared_write.is_some() {
        // The table keys pages by inode; without one there is nothing coherent
        // to serve.
        proc.vm_release_reservation(mmap_addr, byte_len);
        return EACCES;
    }

    // Batch-allocate all pages in a single PMM lock acquisition, then map
    // them with no_flush and issue a single TLB flush after the loop.
    let frame_batch = if let Some(b) = akuma_exec::pmm::alloc_pages_zeroed(pages) { b } else {
        // The eager batch uses the *critical* allocator, which (unlike the
        // demand-paging fault path) does not evict. Under memory pressure that
        // makes a small eager mmap fail outright — userspace `new`/`malloc`
        // then gets ENOMEM and aborts with std::bad_alloc. So mirror the fault
        // path: evict clean file-backed pages (e.g. model weights mmap'd larger
        // than RAM) and retry once.
        let reclaimed = akuma_exec::process::reclaim_clean_file_pages(pages + akuma_exec::pmm::USER_PAGE_RESERVE);
        if reclaimed > 0 {
            if let Some(b) = akuma_exec::pmm::alloc_pages_zeroed(pages) {
                b
            } else {
                // Still short of a contiguous eager batch: fall back to a lazy
                // (demand-paged) region, which always succeeds as a VA
                // reservation and faults in via the reclaim-aware path. Safe
                // for both anonymous and file-backed mappings.
                return mmap_eager_to_lazy_fallback(proc, is_file_backed, fd, offset, len, mmap_addr, pages, page_flags);
            }
        } else {
            return mmap_eager_to_lazy_fallback(proc, is_file_backed, fd, offset, len, mmap_addr, pages, page_flags);
        }
    };

    // Fill file-backed frames from disk BEFORE installing them. The frames are still
    // private here (unmapped, untracked), so no sibling thread can observe — or munmap
    // out from under us — a half-filled page. That privacy is what lets the read loop
    // run in a dropped-BKL window (Phase 2e of the no-bkl-vfs carve-out,
    // docs/archive/BKL_VFS_CARVE_OUT.md): it has exactly the proven profile of the
    // demand-fault Pass B fill — VFS/ext2/block locks only, no BKL-protected state. The
    // old order (map as RW_NO_EXEC → fill → fix up flags) required the BKL across the
    // whole fill precisely because the pages were already visible to the process.
    // In practice this arm serves writable MAP_SHARED mappings (e.g. llama's
    // --kv-cache-file): MMAP_FILE_BACKED_LAZY routes read-only file mmaps to the
    // demand-paged path on every profile.
    if is_file_backed {
        if let Some((path, mount_id, inode)) = &file {
            let _vfs_window = super::fs::VfsBklGuard::new();
            let mut file_off = offset;
            let mut bytes_read = 0usize;
            // One staging page for the whole fill, allocated outside the loop — see
            // `writeback_shared_pages` for why this is not per-iteration.
            let mut fill_buf = alloc::vec![0u8; 4096];
            for (i, frame) in frame_batch.iter().enumerate() {
                let chunk = core::cmp::min(4096, len.saturating_sub(i * 4096));
                if chunk == 0 { break; }
                // Read into the staging buffer, then publish into the frame. The
                // frame is freshly allocated and not yet in any page table, so
                // nothing can observe the intermediate state.
                let page_buf = &mut fill_buf[..chunk];
                // By inode when the fd has one (a `/proc` file renders by path).
                match akuma_vfs_glue::fs::read_at_open_file(path, *mount_id, *inode, file_off, page_buf) {
                    Ok(n) => {
                        if !akuma_exec::mmu::copy_to_phys(frame.addr, &fill_buf[..n]) {
                            break; // the PMM just handed us this frame; cannot happen
                        }
                        bytes_read += n;
                        file_off += n;
                        if n < chunk { break; }
                    }
                    Err(_) => break,
                }
            }
            if akuma_config::SYSCALL_DEBUG_IO_ENABLED {
                akuma_primitives::safe_print!(256, "[mmap] pid={} fd={} file={} off={} len={} = 0x{:x} (read {} bytes)\n",
                    proc.pid, fd, &path, offset, len, mmap_addr, bytes_read);
            }
        }
    } else if akuma_config::MEM_SYSCALL_TRACE_ENABLED {
        akuma_primitives::tprint!(128, "[mmap] pid={} len=0x{:x} prot=0x{:x} flags=0x{:x} = 0x{:x} (eager)\n",
            proc.pid, len, prot, flags, mmap_addr);
    }

    let frames = frame_batch;
    // Page-table install under `as_lock` (shared-kernel SMP): frames were already
    // allocated above (alloc must stay OUTSIDE the hold — the PMM OOM/reclaim path
    // can unmap pages and re-enter `as_lock`) and already carry their file content,
    // so they install with the final `page_flags` directly (no RW_NO_EXEC window +
    // permission fix-up pass). Excludes concurrent BKL-free faults. No I/O here.
    proc.with_address_space(|aspace| {
        // `map_user_page_no_flush` REFUSES a VA whose PTE is already valid and
        // reports that as `false`. Discarding the flag is how a VA-lifetime bug
        // becomes silent memory corruption: the caller keeps the previous
        // occupant's page *and its permissions* while believing it received fresh
        // zeroed anonymous memory, and the freshly allocated frame below is
        // tracked but never mapped. A later store then takes a write permission
        // fault on a page with no lazy region (eager mmaps register none) and no
        // CoW reference — the `[WPF] ... cow_ref=0 lazy_self=NONE` signature that
        // killed cargo mid-build. Count the refusals and name the range; the
        // condition is an invariant violation, never normal traffic.
        let mut declined = 0usize;
        let mut first_va = 0usize;
        for (i, frame) in frames.iter().enumerate() {
            let va = mmap_addr + i * 4096;
            if !aspace.map_user_page_tracked_no_flush(va, *frame, page_flags) {
                if declined == 0 {
                    first_va = va;
                }
                declined += 1;
            }
        }
        // Single TLB flush for the entire mmap range (still under the hold).
        let _ = akuma_exec::mmu::flush_tlb_range(mmap_addr, pages, akuma_exec::mmu::TlbTarget::AllCores);
        if declined > 0 {
            // Resolve the surviving (stale) PA for the first refusal — the walk is
            // only paid on the broken path.
            let ttbr0 = akuma_cpu::sysreg::ttbr0_el1();
            let l0 = akuma_exec::mmu::phys_to_virt((ttbr0 & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
            let stale_pa = akuma_exec::mmu::translate_user_va(l0, first_va).unwrap_or(0) & !0xFFF;
            akuma_primitives::safe_print!(255,
                "[MMAP-STALE-PTE] pid={} tgid={} base={:#x} pages={} declined={} first_va={:#x} \
                 stale_pa={:#x} prot={:#x} flags={:#x}\n",
                proc.pid, proc.tgid, mmap_addr, pages, declined, first_va, stale_pa, prot, flags);
        }
    });
    akuma_pmm::dp_count(&akuma_pmm::EAGER_MMAP_PAGES, pages);

    // `MAP_SHARED|MAP_ANONYMOUS` must survive fork as one object rather than being
    // CoW-copied — see `MmapRegion::shared_anon` and `process::share_rw_range`. A
    // writable file `MAP_SHARED` is the table's and never reaches here.
    let region = MmapRegion::owned_with_prot(mmap_addr, frames, page_prot);
    let region = if plan.shared_anon { region.shared_anon() } else { region };
    // Over the reservation, in address order (`akuma_mmap::insert_region_sorted`).
    proc.vm_install_region(region);

    mmap_addr as u64
}

pub(super) fn sys_mremap(old_addr: usize, old_size: usize, new_size: usize, flags: u32) -> u64 {
    // `MREMAP_FIXED` asks for a specific address that replaces whatever is
    // there. The placer chooses the address here, and quietly returning a
    // different one is a confident wrong answer to an explicit request, so it
    // is refused (amd64's rule; the AArch64 kernel used to ignore the flag).
    if flags & akuma_syscalls_linux::flags::mremap::MREMAP_FIXED != 0 {
        return EINVAL;
    }
    // Argument validation and the shrink short-circuit are pure, and they run
    // BEFORE any process lookup on purpose — a kernel-test caller with no current
    // task must see the argument errno, not ESRCH.
    let (new_pages, may_move) = match akuma_syscalls_mem::mremap::plan(
        old_addr, old_size, new_size, flags, user_va_limit() as usize,
    ) {
        akuma_syscalls_mem::mremap::Plan::Fail(errno) => return errno,
        // A shrink returns the old address with the tail still mapped — a preserved
        // divergence from Linux, pinned by the crate's
        // `diverge_shrink_is_in_place_and_leaves_the_tail_mapped`.
        akuma_syscalls_mem::mremap::Plan::InPlace => return old_addr as u64,
        akuma_syscalls_mem::mremap::Plan::Grow { new_pages, may_move } => (new_pages, may_move),
    };
    let old_end = old_addr + old_size.div_ceil(4096) * 4096;
    let proc = current_owner();
    let _mm_window = MmBklGuard::new();

    // Gated on `!may_move`: the probe is a page walk and a region scan, too
    // much for every growing call. No mapping there is EFAULT, Linux's answer
    // and more informative than ESRCH, so it runs before the process check.
    if !may_move {
        let is_mapped = proc.is_some_and(|p| {
            p.with_address_space(|a| a.is_mapped(old_addr)) || p.vm_with_regions(|r| r.iter().any(|reg| reg.contains(old_addr)))
        });
        return akuma_syscalls_mem::mremap::no_move_errno(is_mapped);
    }
    let Some(proc) = proc else { return ESRCH };
    let Some(new_addr) = proc.vm_alloc_mmap(new_pages * 4096) else { return ENOMEM };

    // The new region is the old one moved: the same protection — including
    // whether the old one recorded any (turning "said nothing" into PROT_NONE
    // killed `rustc`, `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md`) — and
    // the same file, sharing and write-back records, advanced to `old_addr`'s
    // place, since `mremap` may name a mapping's middle. It is demand-paged:
    // the present pages move below, and the rest (the grown tail included)
    // fault in from the same source the old mapping had.
    let region = proc.vm_with_regions(|r| {
        let old = r.iter().find(|reg| reg.contains(old_addr));
        let skip = old.map_or(0, |o| (old_addr - o.start_va) / 4096);
        let mut region = MmapRegion::demand_paged(new_addr, new_pages, old.and_then(MmapRegion::recorded_prot));
        if let Some(f) = old.and_then(|o| o.file) {
            region = region.file_backed(f.advance(skip));
        }
        if let Some(sw) = old.and_then(|o| o.shared_write.as_ref()) {
            region = region.shared_writable(sw.advance(skip));
        }
        if old.is_some_and(|o| o.shared_anon) {
            region = region.shared_anon();
        }
        region
    });
    proc.vm_install_region(region);

    // Re-point each present page rather than copy it: no frames allocated, a
    // CoW page stays CoW, a shared page stays shared.
    if !proc.with_address_space(|a| a.move_present(old_addr, old_end, new_addr)) {
        // Out of page-table frames partway: what moved is mapped at the new
        // address and the region covers it; the rest demand-pages.
        akuma_pmm::note_user_enomem("mremap/tables", new_pages * 4096);
    }
    // Retire the source. Its present pages are gone already, so this finds
    // only what was never moved, and clips the old record.
    unmap_range(proc, old_addr, old_end);
    new_addr as u64
}

/// The RAM frame mapped at `va`, if any. A device page (`/dev/fb0`, mapped
/// write-combining on amd64) is not the program's memory to zero or replace —
/// Linux's `DONTNEED` on it drops and refaults the same pixels — so it reads as
/// absent here, and both passes leave it alone.
fn ram_frame(aspace: &akuma_exec::mmu::UserAddressSpace, va: usize) -> Option<usize> {
    aspace.translate(va).map(|pa| pa & !0xFFF).filter(|&pa| akuma_pmm::contains(pa, 4096))
}

/// How many pages of `[aligned_addr, +pages)` need a private replacement frame —
/// pass 1 of [`madvise_dontneed_range`], and the number of frames to allocate
/// before pass 2 takes `as_lock` again.
///
/// Allocation-free by construction: the caller runs this inside the `as_lock`
/// hold, which masks IRQs, so touching the heap here would be a lock-order
/// violation waiting to happen.
pub fn dontneed_count_shared(
    aspace: &akuma_exec::mmu::UserAddressSpace,
    aligned_addr: usize,
    pages: usize,
) -> usize {
    let mut n = 0usize;
    for i in 0..pages {
        let pa = ram_frame(aspace, aligned_addr + i * 4096);
        let cow = pa.map_or(0, akuma_exec::pmm::cow_ref_get);
        if dontneed_page_action(pa.is_some(), cow) == DontneedAction::BreakSharing {
            n += 1;
        }
    }
    n
}

/// What pass 2 did, for the audit counters and for returning the unused spares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DontneedOutcome {
    /// Replacement frames consumed — the first `used` entries of `spares` are now
    /// installed in the address space and must NOT be freed by the caller.
    pub used: usize,
    /// Pages whose share was broken (a peer's frame left intact).
    pub broke: usize,
    /// Shared pages left untouched for want of a replacement frame.
    pub skipped: usize,
}

/// Pass 2 of [`madvise_dontneed_range`]: apply the per-page rule, consuming
/// `spares` in order for the pages that need a private frame and pushing every
/// frame the caller must return to the PMM onto `to_free`.
///
/// Split out of the closure so the boot suite can drive it against a real
/// `UserAddressSpace` and a real CoW-shared frame — the defect is a
/// *cross-address-space* one, and a test on either ledger alone cannot see it.
pub fn dontneed_apply(
    aspace: &mut akuma_exec::mmu::UserAddressSpace,
    aligned_addr: usize,
    pages: usize,
    spares: &[akuma_exec::pmm::PhysFrame],
    to_free: &mut alloc::vec::Vec<akuma_exec::pmm::PhysFrame>,
    flags: u64,
) -> DontneedOutcome {
    let mut out = DontneedOutcome::default();
    for i in 0..pages {
        let va = aligned_addr + i * 4096;
        let pa = ram_frame(aspace, va);
        let cow = pa.map_or(0, akuma_exec::pmm::cow_ref_get);
        match dontneed_page_action(pa.is_some(), cow) {
            DontneedAction::Nothing => {}
            DontneedAction::ZeroInPlace => {
                aspace.zero_mapped_page(va);
            }
            DontneedAction::BreakSharing => {
                let Some(new_frame) = spares.get(out.used).copied() else {
                    out.skipped += 1;
                    continue;
                };
                out.used += 1;
                // Clear the PTE and give up this address space's reference to the
                // shared frame. `unmap_and_free_page` reports `Some` only when it
                // dropped the frame's LAST VA here — the same `released_last_va`
                // gate `complete_cow_break` uses, and the only thing standing
                // between this and the §5.6 refcount underflow. `pmm::free_page`
                // then routes through `cow_ref_dec` and declines to free the
                // physical frame while the peer still holds it.
                if let Some(old) = aspace.unmap_and_free_page(va) {
                    to_free.push(old);
                }
                // `flags` is the region's recorded protection (`RW_NO_EXEC` when
                // it records none), never the old PTE's bits: a CoW-shared page
                // is mapped RO whatever the region says, and copying that forward
                // made a replacement that took a write fault with `cow_ref=0`,
                // the unrecoverable `[WPF] … lazy_self=NONE` shape.
                if aspace.map_page(va, new_frame.addr, flags).is_ok()
                {
                    aspace.track_user_frame(new_frame);
                    out.broke += 1;
                } else {
                    to_free.push(new_frame);
                }
            }
        }
    }
    out
}

/// `MADV_DONTNEED` over `[aligned_addr, +pages)`, region by region.
fn madvise_dontneed_range(
    proc: &akuma_exec::process::Process,
    aligned_addr: usize,
    pages: usize,
) {
    use core::sync::atomic::Ordering;
    const BATCH: usize = 16;

    let end = aligned_addr + pages * 4096;
    // A page of the shared writable table would be zeroed for every mapper.
    // Linux drops the mapping and keeps the page cache, and so does this: the
    // range is written back and this address space's claim dropped, and the
    // next touch faults the shared page back in.
    release_shared_write(proc, aligned_addr, end);

    // **Only pages inside a recorded region, or the `brk` heap, are touched.**
    // The ELF image is mapped by the loader and is in no region; zeroing its
    // text or data would destroy the running program with no way to re-read
    // it, where Linux would refault it from the file. Walked a batch of
    // regions at a time, so no allocation and no lock across the page work.
    let mut cursor = aligned_addr;
    while cursor < end {
        let mut batch = [(0usize, 0usize, false, 0u64); BATCH];
        let mut n = 0;
        proc.vm_with_regions(|r| {
            let first = r.partition_point(|reg| reg.start_va + reg.len_bytes() <= cursor);
            for reg in r.get(first..).unwrap_or_default() {
                if reg.start_va >= end || n == BATCH {
                    break;
                }
                let flags = reg
                    .recorded_prot()
                    .map_or(akuma_exec::mmu::user_flags::RW_NO_EXEC, akuma_exec::mmu::user_flags::to_pte);
                batch[n] = (
                    reg.start_va.max(cursor),
                    (reg.start_va + reg.len_bytes()).min(end),
                    reg.lazy && reg.file.is_some(),
                    flags,
                );
                n += 1;
            }
        });
        let Some(&(_, last_hi, _, _)) = batch[..n].last() else { break };
        for &(lo, hi, file, flags) in &batch[..n] {
            if file {
                dontneed_drop_file_pages(proc, lo, hi);
                DONTNEED_FILE_BACKED.fetch_add((hi - lo) / 4096, Ordering::Relaxed);
            } else {
                dontneed_anon(proc, lo, (hi - lo) / 4096, flags);
            }
        }
        cursor = last_hi;
    }

    let heap_lo = (proc.initial_brk.load(Ordering::Relaxed) + 0xFFF) & !0xFFF;
    let heap_hi = (proc.get_brk() + 0xFFF) & !0xFFF;
    let (lo, hi) = (heap_lo.max(aligned_addr), heap_hi.min(end));
    if lo < hi {
        dontneed_anon(proc, lo, (hi - lo) / 4096, akuma_exec::mmu::user_flags::RW_NO_EXEC);
    }
}

/// `MADV_DONTNEED` over a lazy file-backed span: neither zeroing in place nor
/// breaking sharing is right for one — both leave the mapping reading as zeros
/// for the rest of its life (`MADV_DONTNEED_SHARED_FRAME.md` "Still open" item
/// 2). Linux drops the mapping and lets the next touch re-read the file, and a
/// lazy file region is exactly what makes that safe here. Frames are freed
/// outside the hold, a fixed batch at a time.
fn dontneed_drop_file_pages(proc: &akuma_exec::process::Process, lo: usize, hi: usize) {
    const CHUNK: usize = 64;
    let mut va = lo;
    while va < hi {
        let mut dropped = [None; CHUNK];
        let stop = hi.min(va + CHUNK * 4096);
        proc.with_address_space(|aspace| {
            for (slot, page) in dropped.iter_mut().zip((va..stop).step_by(4096)) {
                *slot = aspace.unmap_and_free_page(page);
            }
        });
        for frame in dropped.into_iter().flatten() {
            akuma_exec::pmm::free_page(frame);
        }
        va = stop;
    }
}

/// `MADV_DONTNEED` over an anonymous span `[aligned_addr, +pages)`: every mapped
/// page reads back as zeroes afterwards, **and no page any other address space
/// maps is written to**. `flags` maps a replacement frame.
///
/// The second half is the whole point. This handler used to `memset` the physical
/// frame in place, which is correct only while the frame has exactly one holder.
/// After a `fork` it does not: parent and child share the frame CoW, and zeroing
/// it destroyed the peer's live page — 0 of 4096 bytes surviving, measured. That
/// is the null-`Rc` crash in cargo's `-j4` self-host build
/// (docs/archive/CARGO_HEAP_NULL_RC.md): cargo forks per rustc invocation, so its
/// heap is full of CoW-shared frames, and jemalloc reaches `MADV_DONTNEED` by
/// probing `MADV_FREE` and falling back on its `EINVAL`. `madvshared` is the
/// deterministic probe; `cowstale`/`bssfork` are the no-regression check.
///
/// A shared page is handled by **breaking the share, not by dropping the
/// mapping**. Linux drops the mapping and lets the next touch refault, and the
/// proposal proposed exactly that — but it does not survive contact with this
/// kernel: eager `mmap`s (anything ≤ `config::MMAP_EAGER_MAX_PAGES`, which is
/// every page `madvshared` and most allocators' small runs touch) register **no**
/// lazy region, so `ensure_user_page_mapped` has nothing to demand-page from and
/// the next touch is a SIGSEGV instead of a zero page. Installing a private zero
/// frame gives the caller the identical observable result — mapped, readable,
/// all zeroes — with no dependency on the region bookkeeping, so it is uniform
/// across eager and lazy mappings alike.
///
/// Two-pass because of the lock rules: frames must be allocated **outside**
/// `as_lock` (the PMM's reclaim path re-enters it and the `Spinlock` is not
/// reentrant), and the page state that says how many frames are needed can only
/// be read from the page tables. Pass 1 counts under the hold, the allocation
/// happens between the holds, pass 2 re-reads every page and acts on what it
/// finds *then* — so a peer that broke CoW in the window is simply seen as
/// unshared, and a page that became shared in the window (a concurrent `fork`)
/// finds no spare frame and is skipped rather than wiped.
fn dontneed_anon(
    proc: &akuma_exec::process::Process,
    aligned_addr: usize,
    pages: usize,
    flags: u64,
) {
    use core::sync::atomic::Ordering;

    // Pass 1 — count the pages that need a private replacement frame. No
    // allocation: `as_lock` is held with IRQs masked, so this closure must not
    // touch the heap.
    let shared_pages =
        proc.with_address_space(|aspace| dontneed_count_shared(aspace, aligned_addr, pages));

    // Between the holds: allocate. `alloc_pages_zeroed` is the batch path (one
    // PMM acquisition); if it cannot serve the whole batch, take what the
    // reclaim-aware per-page allocator will give and skip the remainder — the
    // advice is advisory, and not zeroing a page is a far better failure than
    // zeroing someone else's.
    let mut spares: alloc::vec::Vec<akuma_exec::pmm::PhysFrame> = if shared_pages == 0 {
        alloc::vec::Vec::new()
    } else if let Some(v) = akuma_exec::pmm::alloc_pages_zeroed(shared_pages) {
        v
    } else {
        let mut v = alloc::vec::Vec::with_capacity(shared_pages);
        for _ in 0..shared_pages {
            match akuma_exec::pmm::alloc_page_zeroed_user() {
                Some(f) => v.push(f),
                None => break,
            }
        }
        v
    };
    for frame in &spares {
        akuma_exec::pmm::track_frame(*frame, akuma_exec::runtime::FrameSource::UserData);
    }

    // Old frames whose last VA in this address space went away, and any
    // replacement that could not be installed. Freed after the hold; the `Vec` is
    // pre-reserved so the pushes inside it never hit the allocator.
    let mut to_free: alloc::vec::Vec<akuma_exec::pmm::PhysFrame> =
        alloc::vec::Vec::with_capacity(spares.len() * 2);

    // Pass 2 — apply. `translate` / `zero_mapped_page` / `unmap_and_free_page`
    // all read page-table state, so the hold is what keeps a concurrent BKL-free
    // fault from editing the tables underneath them.
    let DontneedOutcome { used, broke, skipped } = proc.with_address_space(|aspace| {
        dontneed_apply(aspace, aligned_addr, pages, &spares, &mut to_free, flags)
    });

    for frame in spares.drain(used..) {
        akuma_exec::pmm::free_page_at(frame, akuma_pmm::FreeSite::MadviseDontneed);
    }
    for frame in to_free {
        akuma_exec::pmm::free_page_at(frame, akuma_pmm::FreeSite::MadviseDontneed);
    }
    if broke > 0 {
        DONTNEED_SHARED_FRAME.fetch_add(broke, Ordering::Relaxed);
    }
    if skipped > 0 {
        DONTNEED_SHARE_BREAK_SKIPPED.fetch_add(skipped, Ordering::Relaxed);
    }
}

pub(super) fn sys_madvise(addr: usize, len: usize, advice: i32) -> u64 {
    use akuma_syscalls_mem::madvise::Action;

    // Range validation FIRST — before the advice decode and before any process
    // lookup, so a bad range is EINVAL rather than ESRCH (the same ordering rule
    // `sys_mmap` follows). `len` arrives straight from a user register; without
    // this, `madvise(addr, -1, MADV_DONTNEED)` produced a ~4.5e15 page count and an
    // unbounded loop inside an `MmBklGuard` window.
    // See docs/archive/AKUMA_EXTRACT_MMAP.md §10.1 defect A.
    if !akuma_syscalls_mem::madvise::range_fits_user_va(addr, len, user_va_limit() as usize) {
        return EINVAL;
    }

    match akuma_syscalls_mem::madvise::action(advice) {
        Action::Willneed => {
            // Pre-fault pages in lazy regions that aren't yet mapped.
            // This is advisory; OOM during pre-faulting is silently ignored.
            let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process_shared(current_pid).map_or(current_pid, |p| p.tgid);
            let proc = match akuma_exec::process::lookup_process_shared(owner_pid) {
                Some(p) => p,
                None => return 0,
            };
            let _mm_window = MmBklGuard::new();
            let aligned_addr = addr & !0xFFF;
            let end = (addr.saturating_add(len) + 0xFFF) & !0xFFF;

            // Collect (va, flags) pairs for pages in lazy regions not yet mapped.
            //
            // ANONYMOUS (LazySource::Zero) pages only: installing a zeroed frame IS
            // their fill. A FILE-backed lazy page must be left to the demand-fault
            // path, which reads the file into the frame — pre-mapping a zeroed frame
            // here *destroys* its content (the page is now "present", so the fill
            // never runs). That was a real corruption: llama.cpp's mmap loader calls
            // posix_madvise(WILLNEED) over the whole model mapping, and every weight
            // page this loop touched first became zeroes — garbage inference with
            // mmap, clean with --no-mmap (caught 2026-07-25 by the Phase 2e llama
            // end-to-end validation; docs/archive/BKL_VFS_CARVE_OUT.md §10).
            let mut prefault: alloc::vec::Vec<(usize, u64)> = alloc::vec::Vec::new();
            let mut va = aligned_addr;
            while va < end {
                if !akuma_exec::mmu::is_current_user_page_mapped(va)
                    && let Some((flags, akuma_exec::process::LazySource::Zero, _, _)) =
                        akuma_exec::process::lazy_region_lookup_for_pid(proc.tgid, va)
                    {
                        prefault.push((va, flags));
                    }
                va += 4096;
            }
            if prefault.is_empty() {
                return 0;
            }

            // Batch-allocate and map with deferred TLB flush.
            let frames = match akuma_exec::pmm::alloc_pages_zeroed(prefault.len()) {
                Some(v) => v,
                None => return 0, // advisory — ignore OOM
            };
            // Prefault install under `as_lock` (shared-kernel SMP); frames were
            // allocated above (alloc stays outside the hold).
            proc.with_address_space(|aspace| {
                for (idx, (page_va, flags)) in prefault.into_iter().enumerate() {
                    let frame = frames[idx];
                    // The return is deliberately dropped: MADV_WILLNEED is advisory,
                    // so losing the race to an already-mapped page is a no-op, not an
                    // error. The frame is tracked either way.
                    let _ = aspace.map_user_page_tracked_no_flush(page_va, frame, flags);
                }
                // Flush the entire requested range (covers all newly mapped pages).
                let _ = akuma_exec::mmu::flush_tlb_range(
                    aligned_addr, (end - aligned_addr) / 4096, akuma_exec::mmu::TlbTarget::AllCores);
            });
            0
        }
        Action::Dontneed => {
            if len == 0 {
                return 0;
            }
            // `ESRCH` rather than a silent 0: it is what says the walk was reached.
            let Some(proc) = current_owner() else { return ESRCH };
            let _mm_window = MmBklGuard::new();
            let (aligned_addr, pages) = dontneed_zero_range(addr, len);
            if addr & 0xFFF != 0 {
                // Linux returns EINVAL for an unaligned start; rounding it DOWN
                // instead means the partial head page — the caller's live data — is
                // inside the range about to be zeroed. Counted, not fixed: a
                // separate divergence with its own verification cycle, and it has
                // never read non-zero (`CARGO_HEAP_NULL_RC.md`, follow-on 1).
                DONTNEED_UNALIGNED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            madvise_dontneed_range(proc, aligned_addr, pages);
            0
        }
        // Akuma does not implement MADV_FREE, so say so instead of fabricating
        // success. Linux itself returns EINVAL for advice it doesn't support, and
        // callers that care read it correctly: Redis probes MADV_FREE and treats
        // EINVAL as "older kernel, presumably unaffected" and starts, where the
        // fabricated 0 sent it down a THP-corruption self-check it cannot pass
        // without /proc/<pid>/smaps (docs/archive/LONG_ROAD_TO_REDIS.md §5).
        //
        // KNOWN CONSEQUENCE, watch it: allocators that probe MADV_FREE (jemalloc,
        // mimalloc) fall back to MADV_DONTNEED on EINVAL, and this kernel's
        // MADV_DONTNEED diverges from Linux — it zeroes the *physical frame* where
        // Linux drops the *mapping*, so on a frame shared by CoW-after-fork or the
        // file page cache it also wipes the peer's live copy. That divergence
        // predates this change; what changes is how much traffic reaches it. The
        // `DONTNEED_SHARED_FRAME` / `DONTNEED_UNALIGNED` counters above (see the
        // audit block at the top of this file, reported in PSTATS) exist to make
        // exactly that measurable — if `DONTNEED_SHARED_FRAME` starts climbing,
        // fixing MADV_DONTNEED to break sharing rather than zero in place is the
        // prerequisite, not backing this out.
        // `MADV_FREE` -> EINVAL and every unrecognised advice -> success are both
        // decided by the crate and pinned there (`diverge_madv_free_is_einval_not_success`,
        // `diverge_unknown_advice_reports_success`). The long rationale above is why.
        Action::Fail(errno) => errno,
        Action::Ignore => 0,
    }
}

/// `membarrier(2)`.
///
/// The command decode is `akuma_syscalls_mem::membarrier`; the barrier itself stays
/// here because it is inline assembly, which that crate forbids — the right split,
/// not an inconvenience.
pub fn membarrier_cmd(cmd: u32) -> u64 {
    use akuma_syscalls_mem::membarrier::{Command, command};
    match command(cmd) {
        Command::Barrier => {
            akuma_cpu::barrier::dsb_ish();
            akuma_cpu::barrier::isb();
            0
        }
        // Query / Register / Invalid all answer without the kernel acting.
        other => other.immediate_result().unwrap_or(0),
    }
}

/// `mprotect(addr, len, prot)`, both kernels.
///
/// The regions split on exactly the range (never record a sub-range's
/// protection against a whole region: `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md`),
/// then every present page is re-permissioned by the walker's
/// `reprotect_present`. A page whose frame is still copy-on-write shared is
/// mapped without write (`akuma_cow::marks_cow`), so the first store breaks
/// the sharing — unless the region is shared **by identity** (`MAP_SHARED`,
/// anonymous or file), where CoW would mean "stop sharing". AArch64 used to
/// grant the write in place, so a forked page `mprotect`-ed writable was
/// written through to the parent (`docs/archive/AKUMA_MPROTECT_CONVERGENCE.md`).
pub(super) fn sys_mprotect(addr: usize, len: usize, prot: u32) -> u64 {
    if addr & 0xFFF != 0 { return EINVAL; }
    if len == 0 { return 0; }
    let byte_len = len.div_ceil(4096) * 4096;
    let Some(end) = addr.checked_add(byte_len).filter(|e| *e <= akuma_exec::mmu::USER_VA_END) else {
        return EINVAL;
    };
    let new_prot = akuma_exec::mmu::Prot::from_prot(prot);
    let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process_shared(current_pid).map_or(current_pid, |p| p.tgid);
    if akuma_config::MEM_SYSCALL_TRACE_ENABLED {
        akuma_primitives::tprint!(128, "[mprotect] pid={} owner={} addr=0x{:x} len=0x{:x} prot={:#x}\n",
            current_pid, owner_pid, addr, byte_len, prot);
    }
    let Some(proc) = akuma_exec::process::lookup_process_shared(owner_pid) else { return ESRCH };
    let _mm_window = MmBklGuard::new();
    akuma_exec::process::update_eager_region_flags(proc.tgid, addr, byte_len, new_prot);
    // The extents shared by identity, collected under the region hold and
    // allocating only when there is one; almost every `mprotect` is a private
    // allocator page and leaves this empty. Released before `as_lock`.
    let identity_shared: Vec<(usize, usize)> = proc.vm_with_regions(|r| {
        akuma_exec::process::regions_overlapping(r, addr, end)
            .iter()
            .filter(|r| r.shared_anon || r.shared_write.is_some())
            .map(|r| (r.start_va, r.start_va + r.len_bytes()))
            .collect()
    });
    let write = new_prot.is_write();
    proc.with_address_space(|aspace| {
        aspace.reprotect_present(addr, end, new_prot, |va, pa| {
            let shared = identity_shared.iter().any(|&(a, b)| va >= a && va < b);
            akuma_cow::marks_cow(shared, write, akuma_pmm::cow_ref_get(pa))
        })
    });
    // A page gaining execute must not run stale instructions: AArch64's
    // I-cache is not coherent with stores. x86's is.
    #[cfg(target_arch = "aarch64")]
    if prot & 0x4 != 0 {
        let mut va = addr;
        while va < end {
            let mut off = 0usize;
            while off < 4096 {
                akuma_cpu::cache::dc_cvau(va + off);
                off += 64;
            }
            va += 4096;
        }
        akuma_cpu::barrier::dsb_ish();
        akuma_cpu::cache::ic_iallu();
        akuma_cpu::barrier::dsb_ish();
        akuma_cpu::barrier::isb();
    }
    0
}

/// The process whose address space the caller runs in: a thread's group
/// leader, which owns the region list `CLONE_VM` siblings share.
pub(super) fn current_owner() -> Option<&'static akuma_exec::process::Process> {
    let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process_shared(current_pid).map_or(current_pid, |p| p.tgid);
    akuma_exec::process::lookup_process_shared(owner_pid)
}

pub(super) fn sys_munmap(addr: usize, len: usize) -> u64 {
    // Linux's order: alignment, then the range, then whether there is anyone
    // to unmap from.
    if addr & 0xFFF != 0 {
        return EINVAL;
    }
    // `munmap(addr, 0)` unmaps ONE page here where Linux returns EINVAL — a
    // preserved divergence, pinned by the crate's
    // `diverge_munmap_zero_length_unmaps_one_page`.
    let unmap_len = akuma_syscalls_mem::mmap::munmap_len(len);
    let Some(end) = addr.checked_add(unmap_len).filter(|e| *e <= akuma_exec::mmu::USER_VA_END) else {
        return EINVAL;
    };
    let Some(proc) = current_owner() else { return ESRCH };
    let _mm_window = MmBklGuard::new();
    unmap_range(proc, addr, end);
    0
}

/// [`sys_munmap`]'s body for a kernel's own callers (`MAP_FIXED`, `mremap`'s
/// move, exit's shared-mapping release): unmap `[start, end)` of the current
/// process. No-op without one.
pub fn unmap_current(start: usize, end: usize) {
    if let Some(proc) = current_owner() {
        unmap_range(proc, start, end);
    }
}

/// Remove every mapping in `[addr, end)` from `proc` and free the frames
/// nothing else maps. `munmap`'s body, and what `MAP_FIXED` does to the range
/// it lands on. The range is free address space again the moment its regions
/// are gone: the placer reads the region list.
///
/// Three steps, each in its own lock hold (regions, then address space, then
/// the PMM; never nested the other way):
///
/// 1. **Write back** shared writable file pages while they are still mapped,
///    and drop them from the table (`release_shared_write`).
/// 2. **Records**: every region in the range, eager and lazy, is detached,
///    clipped at both ends; a file no region maps afterwards loses its pin.
/// 3. **Pages**: one sweep of the whole range by live PTE. Never by the
///    record, which goes stale when a CoW break or `MADV_DONTNEED` replaces a
///    frame, and a range may hold pages no region claims (a `MAP_FIXED` over
///    the ELF image). The ledger decides which frames this address space held
///    last; the PMM's share count then decides whether to free.
///
/// `MAP_FIXED` used to clear the PTEs itself and leave the old record under
/// the new one (`docs/archive/AKUMA_MAP_FIXED_EAGER_LEAK.md`); an unmap that
/// began mid-region used to free only the first region's pages
/// (`docs/archive/CARGO_HEAP_NULL_RC.md`, D8/D9). Both kernels have run this
/// one body since 2026-11-12 (`docs/archive/AKUMA_ONE_MUNMAP.md`).
fn unmap_range(proc: &akuma_exec::process::Process, addr: usize, end: usize) {
    if end <= addr {
        return;
    }
    release_shared_write(proc, addr, end);
    let pieces = akuma_exec::process::detach_regions_in_range(proc, addr, end);
    if akuma_config::TRACE_MUNMAP {
        for p in pieces.iter().filter(|p| !p.lazy) {
            akuma_primitives::tprint!(128, "[munmap] pid={} addr=0x{:x} ({} pages, {} owned, base=0x{:x})\n",
                proc.pid, addr, p.pages, p.frames.len(), p.start_va);
        }
    }
    drop(pieces);

    let mut to_free = Vec::new();
    proc.with_address_space(|aspace| aspace.unmap_present(addr, end, &mut to_free));
    for frame in to_free {
        akuma_exec::pmm::free_page_at(frame, akuma_pmm::FreeSite::MunmapRegion);
    }
}
