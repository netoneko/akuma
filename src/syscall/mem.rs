use super::*;
use akuma_primitives::{GuardToggle, ToggledGuard};
use akuma_exec::process::MmapRegion;

/// The `no-bkl-mm` carve-out (Phase 5 of
/// docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md), as a [`GuardToggle`] marker.
///
/// Correctness rests on the state `sys_mprotect`/`sys_madvise`/`sys_munmap`/
/// `sys_mremap`/`sys_mmap` mutate already carrying its own fine-grained lock —
/// `Process::as_lock` (page-table PTE edits, via `Process::with_address_space`),
/// `Process::vm_lock` (`mmap_regions` AND the mmap free-list, via
/// `vm_with_regions`/`vm_alloc_mmap`/`vm_free_mmap`), `Process::lazy_regions`, PMM/
/// `FRAME_TRACKER` (never held across a yield), and `SHARED_FILE_MAPPINGS` — so
/// the BKL is redundant for them. Two gaps this phase's audit found (a plain
/// unguarded `ProcessMemory::free_regions`, and `sys_mmap`'s OOM/reclaim sweep
/// mutating page tables with no `as_lock` hold) were closed as a prerequisite;
/// see `Process::vm_alloc_mmap`/`vm_free_mmap` and
/// `reclaim_clean_file_pages`'s per-page `as_lock_hold`.
///
/// Unlike the VFS/net carve-outs, none of `as_lock`/`vm_lock`/`lazy_regions`/
/// PMM need to know a BKL-free window is calling them — their IRQ-masking is
/// already unconditional (not gated on being reachable from a dropped-BKL window
/// the way `PreemptGuard` is, see BKL_VFS_CARVE_OUT.md §19.3), so there is no
/// AB-BA nested-IRQ hazard analogous to the one that gate exists to close.
pub(super) struct MmBkl;

impl GuardToggle for MmBkl {
    const COMPILED_IN: bool = cfg!(all(kernel_smp_shared, kernel_no_bkl_mm));
    #[inline]
    fn enabled() -> bool {
        #[cfg(kernel_smp_shared)]
        {
            crate::smp_shared::mm_bkl_drop_enabled()
        }
        #[cfg(not(kernel_smp_shared))]
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
// `crate::syscall::mem::MAP_FIXED` (kernel tests) keeps its spelling.

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

/// `MADV_DONTNEED` calls whose start address was not page-aligned. Linux rejects
/// these with `EINVAL`; rounding the start DOWN pulls the caller's live head page
/// into the zeroed range. Still unfixed — deliberately a separate cycle
/// (`CARGO_HEAP_NULL_RC.md` § "The fix", follow-on 1); it has never read non-zero.
pub static DONTNEED_UNALIGNED: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Pages whose frame was shared with another address space (`cow_ref >= 2` — a
/// post-fork CoW page or a `file_page_cache` page) and which therefore took the
/// share-breaking path instead of being zeroed in place. Before 2026-08-14 every
/// one of these wiped a frame some other address space still maps.
pub static DONTNEED_SHARED_FRAME: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Shared pages left **untouched** because no replacement frame was available:
/// the PMM had none (`MADV_DONTNEED` is advisory, so failing to zero beats
/// wiping a peer), or a fork landed between this handler's classify and apply
/// passes and made a page shared that was private a moment earlier. Expected 0;
/// a climbing value means memory pressure is reaching this path.
pub static DONTNEED_SHARE_BREAK_SKIPPED: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// `MADV_DONTNEED` pages that were **file-backed**, and so had their mapping dropped
/// (Linux's behaviour) instead of being zeroed in place or replaced with a private
/// zero frame. Before this, both of the other actions made an mmap'd file read back as
/// zeros permanently — see `MADV_DONTNEED_SHARED_FRAME.md` "Still open" item 2, and
/// `FPCACHE_ZERO_PAGE_POISONING.md` for the hunt that ended here.
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

/// Writable `MAP_SHARED` file-backed mappings whose dirty pages must be flushed
/// back to the backing file.
///
/// Akuma has no unified page cache, so a file-backed mapping and the on-disk file
/// do NOT share storage: writes through the mapping land in anonymous frames and
/// are invisible to the file unless we copy them back explicitly. We record each
/// such mapping by `(tgid, base_va)` and write its resident pages back to the
/// file on `munmap`, `msync`, and process exit. `msync` is the POSIX flush point
/// linkers like `rust-lld` use right before renaming their output buffer, so
/// without this a freshly-linked binary lands on disk as zero bytes (the in-VM
/// self-host `hello` link, docs/AKUMA_SELF_HOSTING.md §7d).
///
/// These mappings are allocated EAGERLY (all pages resident) so the frame list is
/// always complete and writeback is a straight copy — see `sys_mmap`.
struct SharedFileMapping {
    path: alloc::string::String,
    file_offset: usize,
    len: usize,
}

static SHARED_FILE_MAPPINGS: Spinlock<BTreeMap<(u32, usize), SharedFileMapping>> =
    Spinlock::new(BTreeMap::new());

/// Does `uaddr` fall inside a writable `MAP_SHARED` file mapping owned by `tgid`?
///
/// This is Akuma's *entire* notion of memory genuinely shared between address
/// spaces: `MAP_SHARED|MAP_ANONYMOUS` gets no special handling (fork copies it) and
/// there is no shm, so any other mapping is private to one address space no matter
/// what flags created it. `futex_key_tgid` uses this to decide whether a
/// non-private futex may legitimately share a key namespace with another process —
/// see its doc comment for why keying every non-private op globally was a
/// cross-process lost wakeup.
pub(super) fn is_shared_file_mapping(tgid: u32, uaddr: usize) -> bool {
    let map = SHARED_FILE_MAPPINGS.lock();
    map.range(..=(tgid, uaddr))
        .next_back()
        .is_some_and(|(&(t, base), m)| t == tgid && uaddr < base.saturating_add(m.len))
}

/// Copy `len` bytes from the resident pages at physical addresses `pas` back to
/// `path`, starting at `file_offset`. Returns the number of bytes written.
pub fn writeback_shared_pages(path: &str, file_offset: usize, len: usize, pas: &[usize]) -> usize {
    let mut off = file_offset;
    let mut written = 0usize;
    for (i, &pa) in pas.iter().enumerate() {
        let chunk = core::cmp::min(4096, len.saturating_sub(i * 4096));
        if chunk == 0 { break; }
        let kva = akuma_exec::mmu::phys_to_virt(pa).cast_const();
        let buf = unsafe { core::slice::from_raw_parts(kva, chunk) };
        match crate::fs::write_at(path, off, buf) {
            Ok(n) => { written += n; off += n; }
            Err(_) => break,
        }
    }
    written
}

/// Flush and forget every writable MAP_SHARED file mapping owned by `tgid`.
/// Called from the exit syscalls so a process that drops its mapping by exiting
/// (rather than calling `munmap`) still persists its writes, and so a later
/// process reusing the same `tgid`/`base_va` can't inherit a stale entry.
pub(super) fn flush_and_clear_shared_file_mappings(tgid: u32) {
    // Snapshot the entries for this tgid, then resolve+writeback outside the lock
    // (writeback touches the fs, which takes other locks).
    let entries: Vec<(usize, alloc::string::String, usize, usize)> = {
        let map = SHARED_FILE_MAPPINGS.lock();
        map.iter()
            .filter(|((t, _), _)| *t == tgid)
            .map(|((_, base), m)| (*base, m.path.clone(), m.file_offset, m.len))
            .collect()
    };
    if entries.is_empty() { return; }
    if let Some(proc) = akuma_exec::process::lookup_process_shared(tgid) {
        for (base, path, foff, mlen) in &entries {
            let pas = proc.vm_with_regions(|r| {
                r.iter().find(|reg| reg.start_va == *base)
                    .map(|reg| reg.frames.iter().map(|f| f.addr).collect::<Vec<usize>>())
            });
            if let Some(pas) = pas {
                writeback_shared_pages(path, *foff, *mlen, &pas);
            }
        }
    }
    let mut map = SHARED_FILE_MAPPINGS.lock();
    map.retain(|(t, _), _| *t != tgid);
}

/// `msync(addr, len, flags)` — flush writable MAP_SHARED file mappings that
/// overlap `[addr, addr+len)` back to their backing files. Other mappings are a
/// no-op (return success), matching Linux for clean/private ranges.
pub(super) fn sys_msync(addr: usize, len: usize, _flags: u32) -> u64 {
    let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process_shared(current_pid).map_or(current_pid, |p| p.tgid);
    let proc = match akuma_exec::process::lookup_process_shared(owner_pid) {
        Some(p) => p,
        None => return ESRCH,
    };
    let end = addr.saturating_add(if len > 0 { (len + 4095) & !4095 } else { 4096 });
    let entries: Vec<(usize, alloc::string::String, usize, usize)> = {
        let map = SHARED_FILE_MAPPINGS.lock();
        map.iter()
            .filter(|((t, base), m)| *t == proc.tgid && *base < end && base.saturating_add(m.len) > addr)
            .map(|((_, base), m)| (*base, m.path.clone(), m.file_offset, m.len))
            .collect()
    };
    for (base, path, foff, mlen) in &entries {
        let pas = proc.vm_with_regions(|r| {
            r.iter().find(|reg| reg.start_va == *base)
                .map(|reg| reg.frames.iter().map(|f| f.addr).collect::<Vec<usize>>())
        });
        if let Some(pas) = pas {
            writeback_shared_pages(path, *foff, *mlen, &pas);
        }
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
    use akuma_exec::process::types::ProcessMemory;
    akuma_syscalls_mem::mmap::fixed_overlaps_kernel_va(
        addr,
        len,
        ProcessMemory::KERNEL_VA_START,
        akuma_exec::mmu::kernel_va_end(),
    )
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
fn resolve_file_extent(path: &str, offset: usize, len: usize) -> (u32, u32, usize) {
    match crate::vfs::file_size(path) {
        Ok(file_len) => {
            let (mount_id, inode) = crate::vfs::resolve_file_id(path).unwrap_or((0, 0));
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
                resolve_file_extent(&path, offset, len)
            };
            let source = akuma_exec::process::LazySource::file(
                path, mount_id, inode, offset, filesz, mmap_addr,
            );
            let count = akuma_exec::process::push_lazy_region_with_source(
                proc.tgid, mmap_addr, pages * 4096, page_flags, source);
            crate::tprint!(128, "[mmap] eager OOM -> lazy-file fallback pid={} pages={} ({} regions)\n",
                proc.pid, pages, count);
            return mmap_addr as u64;
        }
        return ENOMEM;
    }
    let count = akuma_exec::process::push_lazy_region(proc.tgid, mmap_addr, pages * 4096, page_flags);
    crate::tprint!(128, "[mmap] eager OOM -> lazy fallback pid={} pages={} ({} regions)\n",
        proc.pid, pages, count);
    mmap_addr as u64
}

pub(super) fn sys_mmap(addr: usize, len: usize, prot: u32, flags: u32, fd: i32, offset: usize) -> u64 {
    if len == 0 { return EINVAL; }
    let pages = len.div_ceil(4096);
    let page_flags = akuma_exec::mmu::user_flags::from_prot(prot);

    let _ = MAP_STACK; // silence unused-import lint; flag accepted but ignored

    // The mapping-kind decision — lazy vs eager, file-backed vs anonymous,
    // shared-writable, shared-anon — is a function of the argument bits and the page
    // count, so it lives in `akuma-syscalls-mem` where it is host-tested against the
    // threshold, `MAP_POPULATE`'s precedence and the `MAP_SHARED` matrix. Everything
    // below acts on it.
    let plan = akuma_syscalls_mem::mmap::plan(
        prot, flags, fd, pages, crate::config::MMAP_EAGER_MAX_PAGES,
    );
    let is_fixed = flags & MAP_FIXED != 0;
    let is_fixed_noreplace = flags & MAP_FIXED_NOREPLACE != 0;
    let _ = MAP_POPULATE; // consumed by `plan`; subsumed by the lazy fallback below

    // Like `len == 0`, an unaligned MAP_FIXED / MAP_FIXED_NOREPLACE address is
    // EINVAL before any process lookup. Otherwise `handle_syscall` from kernel
    // tests (no current user task) returns ESRCH instead of EINVAL.
    if mmap_fixed_addr_unaligned_einval(addr, flags) {
        return EINVAL;
    }

    let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process_shared(current_pid).map_or(current_pid, |p| p.tgid);
    let proc = match akuma_exec::process::lookup_process_shared(owner_pid) {
        Some(p) => p,
        None => return ESRCH,
    };
    let _mm_window = MmBklGuard::new();

    let mmap_addr = if (is_fixed || is_fixed_noreplace) && addr != 0 {
        // Reject MAP_FIXED mappings that overlap the kernel identity-map range.
        // The Go runtime uses MAP_FIXED to commit its heap arenas; without this
        // guard a process can map user pages at e.g. 0x8000_0000, overlapping the
        // kernel's physical-RAM identity map and causing silent memory corruption.
        if mmap_fixed_overlaps_kernel_va(addr, pages * 4096) {
            crate::tprint!(128, "[mmap] REJECT MAP_FIXED kernel VA: pid={} addr=0x{:x} len=0x{:x}\n",
                proc.pid, addr, pages * 4096);
            return EINVAL;
        }
        if is_fixed {
            let _ = akuma_exec::process::munmap_lazy_regions_in_range(proc.tgid, addr, pages * 4096);
            // Page-table edits under `as_lock` (shared-kernel SMP): excludes a
            // concurrent BKL-free fault on this address space. No alloc/IO here.
            proc.with_address_space(|aspace| {
                for i in 0..pages {
                    let va = addr + i * 4096;
                    let _ = aspace.unmap_page(va);
                }
            });
        }
        addr
    } else if let Some(a) = proc.vm_alloc_mmap(pages * 4096) { a } else {
        crate::safe_print!(192, "[mmap] REJECT: pid={} size=0x{:x} next=0x{:x} limit=0x{:x}\n",
            proc.pid, pages * 4096,
            proc.memory.next_mmap.load(core::sync::atomic::Ordering::Relaxed),
            proc.memory.mmap_limit);
        return ENOMEM;
    };

    let is_file_backed = plan.is_file_backed;

    // Writable MAP_SHARED on a file-backed mapping has true shared-page semantics:
    // writes through the mapping must become visible in the file. Akuma has no
    // unified page cache, so we honor this by mapping it EAGERLY (resident,
    // writable, populated from the file) and writing the pages back to the file on
    // munmap/msync/exit (see SHARED_FILE_MAPPINGS). Read-only MAP_SHARED has no
    // writes to flush, so it stays on the cheap lazy MAP_PRIVATE-equivalent path.
    let is_shared_writable = plan.is_shared_writable;

    // MAP_POPULATE requests eager pre-faulting; it suppresses lazy allocation.
    // MADV_WILLNEED can also trigger pre-faulting on existing lazy regions.
    // Anonymous private mappings above MMAP_EAGER_MAX_PAGES are demand-paged
    // (zero-fill on first touch) rather than eagerly allocated+zeroed+mapped.
    // This is the "lazy/zero-on-demand population" win from COW_OPTIMIZATIONS.md:
    // pages that are never touched are never allocated, which cuts the physical
    // footprint (the rustc trace ended near OOM from eager over-commit). Small
    // mappings stay eager — see config::MMAP_EAGER_MAX_PAGES for the rationale.
    let use_lazy = plan.use_lazy;

    if use_lazy {
        let count = akuma_exec::process::push_lazy_region(proc.tgid, mmap_addr, pages * 4096, page_flags);
        if crate::config::MEM_SYSCALL_TRACE_ENABLED {
            crate::tprint!(192, "[mmap] pid={} len=0x{:x} prot=0x{:x} flags=0x{:x} = 0x{:x} (lazy, {} regions)\n",
                proc.pid, len, prot, flags, mmap_addr, count);
        }
        return mmap_addr as u64;
    }

    // When MMAP_FILE_BACKED_LAZY is set, demand-page file-backed mmaps instead
    // of eagerly allocating all frames. Default on the size profile where PMM
    // is tight (8 MB): eagerly mapping a 600 KB shared library exhausts user
    // pages before the process can start. Pages are faulted in via
    // LazySource::File, same mechanism as demand-paged ELFs.
    // Writable MAP_SHARED is forced eager (see below) so its pages are all
    // resident for writeback; everything else may demand-page lazily.
    if crate::config::MMAP_FILE_BACKED_LAZY && plan.file_lazy_eligible
        && let Some(akuma_exec::process::FileDescriptor::File(ref f)) = proc.get_fd(fd as u32) {
            let path = f.path.clone();
            // Path→inode resolution reads ext2 metadata (real I/O on a cold cache) —
            // take the dropped-BKL window for it like every other on-disk read path
            // (Phase 2e of the no-bkl-vfs carve-out).
            let (mount_id, inode, filesz) = {
                let _vfs_window = super::fs::VfsBklGuard::new();
                resolve_file_extent(&path, offset, len)
            };
            let source = akuma_exec::process::LazySource::file(
                path.clone(),
                mount_id,
                inode,
                offset,
                filesz,
                mmap_addr,
            );
            let count = akuma_exec::process::push_lazy_region_with_source(
                proc.tgid, mmap_addr, pages * 4096, page_flags, source);
            if crate::config::MEM_SYSCALL_TRACE_ENABLED {
                crate::tprint!(192, "[mmap] pid={} fd={} file={} off={} len=0x{:x} = 0x{:x} (lazy-file, {} regions)\n",
                    proc.pid, fd, &path, offset, len, mmap_addr, count);
            }
            return mmap_addr as u64;
        }

    // Batch-allocate all pages in a single PMM lock acquisition, then map
    // them with no_flush and issue a single TLB flush after the loop.
    let frame_batch = if let Some(b) = crate::pmm::alloc_pages_zeroed(pages) { b } else {
        // The eager batch uses the *critical* allocator, which (unlike the
        // demand-paging fault path) does not evict. Under memory pressure that
        // makes a small eager mmap fail outright — userspace `new`/`malloc`
        // then gets ENOMEM and aborts with std::bad_alloc. So mirror the fault
        // path: evict clean file-backed pages (e.g. model weights mmap'd larger
        // than RAM) and retry once.
        let reclaimed = akuma_exec::process::reclaim_clean_file_pages(pages + crate::pmm::USER_PAGE_RESERVE);
        if reclaimed > 0 {
            if let Some(b) = crate::pmm::alloc_pages_zeroed(pages) {
                b
            } else if is_shared_writable {
                // A writable MAP_SHARED mapping must stay eager so its pages are
                // tracked for writeback; the lazy fallback can't do that, so fail
                // rather than silently drop writes.
                return ENOMEM;
            } else {
                // Still short of a contiguous eager batch: fall back to a lazy
                // (demand-paged) region, which always succeeds as a VA
                // reservation and faults in via the reclaim-aware path. Safe
                // for both anonymous and file-backed mappings.
                return mmap_eager_to_lazy_fallback(proc, is_file_backed, fd, offset, len, mmap_addr, pages, page_flags);
            }
        } else if is_shared_writable {
            return ENOMEM;
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
        if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(fd as u32) {
            let path = f.path;
            let _vfs_window = super::fs::VfsBklGuard::new();
            let mut file_off = offset;
            let mut bytes_read = 0usize;
            for (i, frame) in frame_batch.iter().enumerate() {
                let chunk = core::cmp::min(4096, len.saturating_sub(i * 4096));
                if chunk == 0 { break; }
                let page_kva = akuma_exec::mmu::phys_to_virt(frame.addr);
                let page_buf = unsafe { core::slice::from_raw_parts_mut(page_kva, chunk) };
                match crate::fs::read_at(&path, file_off, page_buf) {
                    Ok(n) => {
                        bytes_read += n;
                        file_off += n;
                        if n < chunk { break; }
                    }
                    Err(_) => break,
                }
            }
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(256, "[mmap] pid={} fd={} file={} off={} len={} = 0x{:x} (read {} bytes)\n",
                    proc.pid, fd, &path, offset, len, mmap_addr, bytes_read);
            }
        }
    } else if crate::config::MEM_SYSCALL_TRACE_ENABLED {
        crate::tprint!(128, "[mmap] pid={} len=0x{:x} prot=0x{:x} flags=0x{:x} = 0x{:x} (eager)\n",
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
            let (table_frames, installed) = unsafe {
                akuma_exec::mmu::map_user_page_no_flush(va, frame.addr, page_flags)
            };
            if !installed {
                if declined == 0 {
                    first_va = va;
                }
                declined += 1;
            }
            aspace.track_user_frame(*frame);
            for tf in table_frames {
                aspace.track_page_table_frame(tf);
            }
        }
        // Single TLB flush for the entire mmap range (still under the hold).
        akuma_exec::mmu::flush_tlb_range(mmap_addr, pages);
        if declined > 0 {
            // Resolve the surviving (stale) PA for the first refusal — the walk is
            // only paid on the broken path.
            let ttbr0: u64;
            #[cfg(target_os = "none")]
            unsafe { core::arch::asm!("mrs {}, TTBR0_EL1", out(reg) ttbr0); }
            #[cfg(not(target_os = "none"))]
            { ttbr0 = 0; }
            let l0 = akuma_exec::mmu::phys_to_virt((ttbr0 & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
            let stale_pa = akuma_exec::mmu::translate_user_va(l0, first_va).unwrap_or(0) & !0xFFF;
            crate::safe_print!(255,
                "[MMAP-STALE-PTE] pid={} tgid={} base={:#x} pages={} declined={} first_va={:#x} \
                 stale_pa={:#x} prot={:#x} flags={:#x}\n",
                proc.pid, proc.tgid, mmap_addr, pages, declined, first_va, stale_pa, prot, flags);
        }
    });
    crate::pmm::dp_count(&crate::pmm::EAGER_MMAP_PAGES, pages);

    // Record writable MAP_SHARED file mappings so their pages get written back to
    // the file on munmap/msync/exit (Akuma has no shared page cache).
    if is_shared_writable
        && let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(fd as u32) {
            SHARED_FILE_MAPPINGS.lock().insert(
                (proc.tgid, mmap_addr),
                SharedFileMapping { path: f.path.clone(), file_offset: offset, len },
            );
            if crate::config::MEM_SYSCALL_TRACE_ENABLED {
                crate::tprint!(192, "[mmap] pid={} fd={} file={} off={} len=0x{:x} = 0x{:x} (shared-writable, writeback on)\n",
                    proc.pid, fd, &f.path, offset, len, mmap_addr);
            }
        }

    // `MAP_SHARED|MAP_ANONYMOUS` must survive fork as one object rather than being
    // CoW-copied — see `MmapRegion::shared_anon` and `process::share_rw_range`. File
    // -backed `MAP_SHARED` is a different mechanism entirely (SHARED_FILE_MAPPINGS
    // writeback), so this is the anonymous case only.
    let region = MmapRegion::owned_with_flags(mmap_addr, frames, page_flags);
    let region = if plan.shared_anon { region.shared_anon() } else { region };
    proc.vm_with_regions(|r| r.push(region));

    mmap_addr as u64
}

pub(super) fn sys_mremap(old_addr: usize, old_size: usize, new_size: usize, flags: u32) -> u64 {
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
    // The old extent, for tearing the source mapping down after the copy. The plan
    // used the same value to decide, but that was a decision and this is an effect —
    // the crate does not hand back extents it only needed internally.
    let old_pages = old_size.div_ceil(4096);

    // Resolve the address-space owner ONCE, while the BKL is still held, and reuse
    // this reference for the rest of the call — the process table itself has no
    // inner lock (BKL_PROCESS_CARVE_OUT.md's audit), so every syscall carved out of
    // the BKL so far resolves its process reference before opening the drop window
    // rather than re-walking the table inside it.
    let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process_shared(current_pid).map_or(current_pid, |p| p.tgid);
    let proc = akuma_exec::process::lookup_process_shared(owner_pid);
    let _mm_window = MmBklGuard::new();

    // Gated on `!may_move`, exactly as before: this probe is three lookups and a
    // `vm_lock` acquisition, so running it on every growing mremap would be a lock
    // per call. The crate reports `may_move` rather than deciding, for that reason.
    if !may_move {
        let is_mapped = akuma_exec::mmu::is_current_user_page_mapped(old_addr)
            || akuma_exec::process::lazy_region_lookup_for_pid(owner_pid, old_addr).is_some()
            || proc.is_some_and(|p| p.vm_with_regions(|r| r.iter().any(|reg| reg.contains(old_addr))));
        return akuma_syscalls_mem::mremap::no_move_errno(is_mapped);
    }

    let proc = match proc {
        Some(p) => p,
        None => return ENOMEM,
    };
    let new_addr = match proc.vm_alloc_mmap(new_pages * 4096) {
        Some(a) => a,
        None => return ENOMEM,
    };

    {
        // Pre-allocate all frames BEFORE taking `as_lock` (alloc must stay outside the
        // hold — the PMM OOM/reclaim path can re-enter `as_lock`). Roll back on OOM.
        let mut new_frames = alloc::vec::Vec::with_capacity(new_pages);
        for _ in 0..new_pages {
            if let Some(frame) = crate::pmm::alloc_page_zeroed() {
                new_frames.push(frame);
            } else {
                for f in new_frames { crate::pmm::free_page_at(f, akuma_pmm::FreeSite::Mremap); }
                return ENOMEM;
            }
        }
        // Page-table install under `as_lock` (shared-kernel SMP). PTE edits only.
        proc.with_address_space(|aspace| {
            for (i, frame) in new_frames.iter().enumerate() {
                let (table_frames, _) = unsafe { akuma_exec::mmu::map_user_page(new_addr + i * 4096, frame.addr, akuma_exec::mmu::user_flags::RW_NO_EXEC) };
                aspace.track_user_frame(*frame);
                for tf in table_frames {
                    aspace.track_page_table_frame(tf);
                }
            }
        });

        let copy_len = old_size.min(new_size);
        if validate_user_ptr(old_addr as u64, copy_len) {
            let mut kernel_buf = alloc::vec![0u8; copy_len.min(1024 * 1024)];
            let mut total_copied = 0;
            while total_copied < copy_len {
                let chunk = (copy_len - total_copied).min(kernel_buf.len());
                if copy_from_user(&mut kernel_buf[..chunk], (old_addr + total_copied) as u64).is_err() {
                    break;
                }
                if copy_to_user((new_addr + total_copied) as u64, &kernel_buf[..chunk]).is_err() {
                    break;
                }
                total_copied += chunk;
            }
        }

        // A remap moves and resizes a mapping; it does not reprotect it, so the new
        // region carries the old one's recorded protection.
        let old_flags = proc.vm_with_regions(|r| {
            r.iter().find(|reg| reg.start_va == old_addr).map(|reg| reg.flags)
        });
        proc.vm_with_regions(|r| r.push(MmapRegion::owned_with_flags(
            new_addr, new_frames, old_flags.unwrap_or(akuma_exec::mmu::user_flags::NONE))));

        let mut found_eager = false;
        // Remove the old region under the lock, then unmap/free its frames after
        // releasing it (unmap/free must not run while vm_lock is held).
        let old_region_opt = proc.vm_with_regions(|r| {
            r.iter().position(|reg| reg.start_va == old_addr).map(|idx| r.remove(idx))
        });
        if let Some(old_region) = old_region_opt {
            // Size from `pages`, not the owned frame count: a CoW-inherited region
            // owns no frames but has every page mapped, and using `frames.len()`
            // would leave those pages mapped while still recycling the VA range.
            let old_region_pages = old_region.pages;
            let freed_size = old_region.len_bytes();
            let old_frames = old_region.frames;
            // Unmap + user-frame bookkeeping under `as_lock`; collect frames to free
            // and free them AFTER releasing the hold (free stays outside).
            let mut to_free = alloc::vec::Vec::new();
            proc.with_address_space(|aspace| {
                for i in 0..old_region_pages {
                    let va = old_addr + i * 4096;
                    match old_frames.get(i) {
                        // Frame this process owns. Free only when this drops its
                        // last reference; an aliased/shared PA is freed by its
                        // surviving owner instead.
                        Some(&frame) => {
                            let _ = aspace.unmap_page(va);
                            if aspace.remove_user_frame(frame) {
                                to_free.push(frame);
                            }
                        }
                        // CoW-inherited page: take the PA from the live PTE.
                        None => {
                            if let Some(frame) = aspace.unmap_and_free_page(va) {
                                to_free.push(frame);
                            }
                        }
                    }
                }
            });
            for frame in to_free { crate::pmm::free_page_at(frame, akuma_pmm::FreeSite::Mremap); }
            proc.vm_free_mmap(old_addr, freed_size);
            found_eager = true;
        }

        if !found_eager {
            let lazy_results = akuma_exec::process::munmap_lazy_regions_in_range(proc.tgid, old_addr, old_pages * 4096);
            // Unmap under `as_lock`; free the returned frames after the hold.
            let mut to_free = alloc::vec::Vec::new();
            proc.with_address_space(|aspace| {
                for &(freed_start, freed_pages) in &lazy_results {
                    for i in 0..freed_pages {
                        if let Some(frame) = aspace.unmap_and_free_page(freed_start + i * 4096) {
                            to_free.push(frame);
                        }
                    }
                }
                for i in 0..old_pages {
                    let va = old_addr + i * 4096;
                    if let Some(frame) = aspace.unmap_and_free_page(va) {
                        to_free.push(frame);
                    }
                }
            });
            for frame in to_free { crate::pmm::free_page_at(frame, akuma_pmm::FreeSite::Mremap); }
            proc.vm_free_mmap(old_addr, old_pages * 4096);
        }

        new_addr as u64
    }
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
        let pa = aspace.translate(aligned_addr + i * 4096).map(|pa| pa & !0xFFF);
        let cow = pa.map_or(0, crate::pmm::cow_ref_get);
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
    spares: &[crate::pmm::PhysFrame],
    to_free: &mut alloc::vec::Vec<crate::pmm::PhysFrame>,
) -> DontneedOutcome {
    let mut out = DontneedOutcome::default();
    for i in 0..pages {
        let va = aligned_addr + i * 4096;
        let pa = aspace.translate(va).map(|pa| pa & !0xFFF);
        let cow = pa.map_or(0, crate::pmm::cow_ref_get);
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
                // RW_NO_EXEC, matching `complete_cow_break`: a CoW-shared page is
                // mapped RO regardless of the region's real protection, so the old
                // PTE's bits cannot be copied forward — an RO replacement would
                // take a write fault with `cow_ref=0` and no lazy region, the
                // unrecoverable `[WPF] … lazy_self=NONE` shape.
                if aspace
                    .map_page(va, new_frame.addr, akuma_exec::mmu::user_flags::RW_NO_EXEC)
                    .is_ok()
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

/// `MADV_DONTNEED` over `[aligned_addr, aligned_addr + pages*4096)`: every mapped
/// page in the range reads back as zeroes afterwards, **and no page any other
/// address space maps is written to**.
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
fn madvise_dontneed_range(
    proc: &akuma_exec::process::Process,
    aligned_addr: usize,
    pages: usize,
) {
    use core::sync::atomic::Ordering;

    // Pass 0 — FILE-BACKED pages. Neither of the two actions below is correct for
    // one: `ZeroInPlace` overwrites the file's bytes, and `BreakSharing` installs a
    // private zero frame. Both leave the mapping reading as zeros for the rest of
    // its life, with no short read, no cache miss and no error anywhere — the
    // residue left after `DP_FILE_FILL_SHORT`, `DP_FILE_CACHE_MISMATCH` and
    // `DP_PROTNONE_FILE_REGION` all came back clean across a reproducing build.
    //
    // Linux drops the mapping and lets the next touch re-read the file. That is
    // safe to do *here*, unlike the anonymous case the comment above worries
    // about, precisely because being file-backed means a `LazySource::File` lazy
    // region exists to demand-page from — the missing-region hazard that forced
    // the zero-frame design does not apply.
    //
    // MADV_DONTNEED_SHARED_FRAME.md "Still open" item 2.
    let file_vas: alloc::vec::Vec<usize> = (0..pages)
        .map(|i| aligned_addr + i * 4096)
        .filter(|va| {
            matches!(
                akuma_exec::process::lazy_region_lookup_for_pid(proc.tgid, *va),
                Some((_, akuma_exec::process::LazySource::File { .. }, _, _))
            )
        })
        .collect();
    if !file_vas.is_empty() {
        let mut dropped: alloc::vec::Vec<crate::pmm::PhysFrame> =
            alloc::vec::Vec::with_capacity(file_vas.len());
        proc.with_address_space(|aspace| {
            for va in &file_vas {
                if let Some(old) = aspace.unmap_and_free_page(*va) {
                    dropped.push(old);
                }
            }
        });
        for frame in dropped {
            crate::pmm::free_page(frame);
        }
        DONTNEED_FILE_BACKED.fetch_add(file_vas.len(), Ordering::Relaxed);
        crate::tprint!(192,
            "[DONTNEED-FILE] pid={} va={:#x} pages={} — dropped file-backed mapping instead of zeroing it\n",
            proc.pid, file_vas[0], file_vas.len());
    }

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
    let mut spares: alloc::vec::Vec<crate::pmm::PhysFrame> = if shared_pages == 0 {
        alloc::vec::Vec::new()
    } else if let Some(v) = crate::pmm::alloc_pages_zeroed(shared_pages) {
        v
    } else {
        let mut v = alloc::vec::Vec::with_capacity(shared_pages);
        for _ in 0..shared_pages {
            match crate::pmm::alloc_page_zeroed_user() {
                Some(f) => v.push(f),
                None => break,
            }
        }
        v
    };
    for frame in &spares {
        crate::pmm::track_frame(*frame, akuma_exec::runtime::FrameSource::UserData);
    }

    // Old frames whose last VA in this address space went away, and any
    // replacement that could not be installed. Freed after the hold; the `Vec` is
    // pre-reserved so the pushes inside it never hit the allocator.
    let mut to_free: alloc::vec::Vec<crate::pmm::PhysFrame> =
        alloc::vec::Vec::with_capacity(spares.len() * 2);

    // Pass 2 — apply. `translate` / `zero_mapped_page` / `unmap_and_free_page`
    // all read page-table state, so the hold is what keeps a concurrent BKL-free
    // fault from editing the tables underneath them.
    let DontneedOutcome { used, broke, skipped } = proc.with_address_space(|aspace| {
        dontneed_apply(aspace, aligned_addr, pages, &spares, &mut to_free)
    });

    for frame in spares.drain(used..) {
        crate::pmm::free_page_at(frame, akuma_pmm::FreeSite::MadviseDontneed);
    }
    for frame in to_free {
        crate::pmm::free_page_at(frame, akuma_pmm::FreeSite::MadviseDontneed);
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
            let frames = match crate::pmm::alloc_pages_zeroed(prefault.len()) {
                Some(v) => v,
                None => return 0, // advisory — ignore OOM
            };
            // Prefault install under `as_lock` (shared-kernel SMP); frames were
            // allocated above (alloc stays outside the hold).
            proc.with_address_space(|aspace| {
                for (idx, (page_va, flags)) in prefault.into_iter().enumerate() {
                    let frame = frames[idx];
                    let (table_frames, _) = unsafe {
                        akuma_exec::mmu::map_user_page_no_flush(page_va, frame.addr, flags)
                    };
                    aspace.track_user_frame(frame);
                    for tf in table_frames {
                        aspace.track_page_table_frame(tf);
                    }
                }
                // Flush the entire requested range (covers all newly mapped pages).
                akuma_exec::mmu::flush_tlb_range(aligned_addr, (end - aligned_addr) / 4096);
            });
            0
        }
        Action::Dontneed => {
            let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process_shared(current_pid).map_or(current_pid, |p| p.tgid);
            let proc = match akuma_exec::process::lookup_process_shared(owner_pid) {
                Some(p) => p,
                None => return 0,
            };
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
            unsafe {
                core::arch::asm!("dsb ish");
                core::arch::asm!("isb");
            }
            0
        }
        // Query / Register / Invalid all answer without the kernel acting.
        other => other.immediate_result().unwrap_or(0),
    }
}

pub(super) fn sys_mprotect(addr: usize, len: usize, prot: u32) -> u64 {
    if len == 0 { return 0; }
    if addr & 0xFFF != 0 { return EINVAL; }
    let pages = len.div_ceil(4096);
    let new_flags = akuma_exec::mmu::user_flags::from_prot(prot);
    let adding_exec = prot & 0x4 != 0;
    let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process_shared(current_pid).map_or(current_pid, |p| p.tgid);
    if crate::config::MEM_SYSCALL_TRACE_ENABLED {
        crate::tprint!(128, "[mprotect] pid={} owner={} addr=0x{:x} len=0x{:x} prot={:#x}\n",
            current_pid, owner_pid, addr, pages * 4096, prot);
    }
    if let Some(proc) = akuma_exec::process::lookup_process_shared(owner_pid) {
        let _mm_window = MmBklGuard::new();
        akuma_exec::process::update_lazy_region_flags(proc.tgid, addr, pages * 4096, new_flags);
        // Eager regions need the same bookkeeping: they are the ones whose PTEs the
        // fault handler can only repair if it knows the intended protection.
        akuma_exec::process::update_eager_region_flags(proc.tgid, addr, pages * 4096, new_flags);

        // Update all page table entries with no_flush, then issue a single
        // TLB range flush. Previously each update_page_flags call issued its
        // own dsb+tlbi+dsb+isb, causing O(pages) expensive barrier sequences.
        // Permission edits under `as_lock` (shared-kernel SMP). PTE edits only.
        proc.with_address_space(|aspace| {
            let mut any_updated = false;
            for i in 0..pages {
                let va = addr + i * 4096;
                if aspace.is_mapped(va) {
                    let _ = aspace.update_page_flags_no_flush(va, new_flags);
                    any_updated = true;
                }
            }
            if any_updated {
                akuma_exec::mmu::flush_tlb_range(addr, pages);
            }
        });
        if adding_exec {
            for i in 0..pages {
                let va = addr + i * 4096;
                unsafe {
                    let mut off = 0usize;
                    while off < 4096 {
                        core::arch::asm!("dc cvau, {}", in(reg) (va + off) as u64);
                        off += 64;
                    }
                }
            }
            unsafe {
                core::arch::asm!("dsb ish");
                core::arch::asm!("ic iallu");
                core::arch::asm!("dsb ish");
                core::arch::asm!("isb");
            }
        }
        0
    } else {
        crate::tprint!(128, "[mprotect] EINVAL: owner={} not found\n", owner_pid);
        EINVAL
    }
}

pub(super) fn sys_munmap(addr: usize, len: usize) -> u64 {
    let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process_shared(current_pid).map_or(current_pid, |p| p.tgid);
    let proc = match akuma_exec::process::lookup_process_shared(owner_pid) {
        Some(p) => p,
        None => return ESRCH,
    };
    let _mm_window = MmBklGuard::new();

    // `munmap(addr, 0)` unmaps ONE page here where Linux returns EINVAL — a
    // preserved divergence, pinned by the crate's
    // `diverge_munmap_zero_length_unmaps_one_page`.
    let unmap_len = akuma_syscalls_mem::mmap::munmap_len(len);

    // Detach EVERY eager region overlapping the range, clipping partial ones at
    // both ends, under vm_lock (pure Vec ops only). The actual page unmap + frame
    // free happens AFTER the lock is released (it takes other locks and must not
    // run while vm_lock is held). Yields one `(base_va, pages, owned_frames)` piece
    // per region touched.
    //
    // This used to match a single region by **exact `start_va`** and return. Any
    // unmap that began mid-region, or spanned more than one, therefore unmapped only
    // the first region's worth of pages, reported success, and left the rest mapped
    // with their VA never recycled. Nothing created split regions often enough for
    // that to show — but every mechanism that does (a partial `mprotect` that splits
    // for flags, a stale region left by an earlier clipped unmap) turns it into a
    // silent leak, and a leftover region also lets an obsolete protection record
    // shadow the live one in `eager_region_flags_for_page_fault`. See
    // docs/archive/CARGO_HEAP_NULL_RC.md (D8/D9).
    //
    // Page counts come from `MmapRegion::pages`, not the frame count: a CoW-inherited
    // region has every page mapped but owns no frames, and sizing the unmap by
    // `frames.len()` would silently unmap nothing while recycling the VA range.
    let unmap_end = addr.saturating_add(unmap_len);
    let detached = proc.vm_with_regions(|r| {
        akuma_exec::process::detach_eager_regions_in_range(r, addr, unmap_end)
    });
    let unmapped_any_eager = !detached.is_empty();
    for (base, n, frames) in detached {
        if crate::config::TRACE_MUNMAP {
            crate::tprint!(128, "[munmap] pid={} addr=0x{:x} ({} pages, {} owned, base=0x{:x})\n",
                proc.pid, addr, n, frames.len(), base);
        }
        // Writable MAP_SHARED file mapping: flush its (still-resident) pages back
        // to the backing file BEFORE the frames are freed below.
        let wb = SHARED_FILE_MAPPINGS.lock().remove(&(proc.tgid, base));
        if let Some(m) = wb {
            let pas: Vec<usize> = frames.iter().map(|f| f.addr).collect();
            let flush_len = m.len.min(n * 4096);
            let written = writeback_shared_pages(&m.path, m.file_offset, flush_len, &pas);
            if crate::config::TRACE_MUNMAP {
                crate::tprint!(192, "[munmap] pid={} shared-writeback file={} off={} {} bytes\n",
                    proc.pid, &m.path, m.file_offset, written);
            }
        }
        // Defer the TLB flush: clear each PTE without a per-page barrier,
        // then flush the whole region once (cheap-win E, COW_OPTIMIZATIONS.md).
        // Unmap + user-frame bookkeeping under `as_lock` (shared-kernel SMP); free the
        // dropped frames AFTER releasing the hold (free stays outside).
        let mut to_free = alloc::vec::Vec::new();
        proc.with_address_space(|aspace| {
            for i in 0..n {
                let va = base + i * 4096;
                // **Take the PA from the live PTE, never from the region's record.**
                // That record goes stale whenever something replaces a mapping without
                // rewriting the region's frame list — `complete_cow_break` installing a
                // private copy, `MADV_DONTNEED`'s share-break, a CoW write fault.
                // Freeing the *recorded* frame released a page this process no longer
                // maps (a peer may still hold it — the premature free every
                // `site=munmap-region` poison report named) while leaking the one that
                // actually was mapped. This used to be two arms: an owned-frame arm
                // that trusted `frames[i]`, and a CoW-inherited arm that read the PTE.
                // Only the second was right, so both collapse into it —
                // `unmap_and_free_page_no_flush` walks the tables **once**, returns what
                // was really there and drops it from `user_frames`. The record is now
                // consulted for one purpose: reporting that it disagreed.
                let dropped = aspace.unmap_and_free_page_no_flush(va);
                if let (Some(rec), Some(live)) = (frames.get(i).copied(), dropped)
                    && rec.addr != live.addr
                {
                    let seen = crate::pmm::DP_MUNMAP_STALE_REGION_FRAME
                        .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    // Rate-limited: this fires ~11k times per self-host build, and
                    // printing every one doubles build wall time — enough to move the
                    // very races being measured. `munmap_stale=` carries the volume.
                    if seen < 32 {
                        crate::tprint!(192,
                            "[MUNMAP-STALE] pid={} va={:#x} recorded={:#x} live={:#x} — freed the live frame\n",
                            proc.pid, va, rec.addr, live.addr);
                    }
                }
                if let Some(frame) = dropped {
                    to_free.push(frame);
                }
            }
            akuma_exec::mmu::flush_tlb_range_all_asid(base, n);
        });
        for frame in to_free { crate::pmm::free_page_at(frame, akuma_pmm::FreeSite::MunmapRegion); }
        proc.vm_free_mmap(base, n * 4096);
    }

    // Lazy regions in the same range. Previously unreachable whenever an eager
    // region matched, because that path returned — so a range covering both kinds
    // left the lazy half mapped.
    let results = akuma_exec::process::munmap_lazy_regions_in_range(proc.tgid, addr, unmap_len);
    if !results.is_empty() {
        for &(freed_start, freed_pages) in &results {
            let mut to_free = alloc::vec::Vec::new();
            proc.with_address_space(|aspace| {
                for i in 0..freed_pages {
                    if let Some(frame) = aspace.unmap_and_free_page_no_flush(freed_start + i * 4096) {
                        to_free.push(frame);
                    }
                }
                akuma_exec::mmu::flush_tlb_range_all_asid(freed_start, freed_pages);
            });
            let had_physical = !to_free.is_empty();
            for frame in to_free { crate::pmm::free_page_at(frame, akuma_pmm::FreeSite::MunmapPartial); }
            // Only recycle the VA range when physical pages were actually freed.
            // Pure lazy (PROT_NONE, never demand-paged) regions must NOT be put
            // back in free_regions: alloc_mmap prefers free_regions over
            // next_mmap, which causes an infinite mmap→reject→munmap→same-addr
            // loop (observed with Go's heap prober returning 0x100000000 60+
            // times in succession).
            if had_physical {
                proc.vm_free_mmap(freed_start, freed_pages * 4096);
            }
        }
        return 0;
    }
    if unmapped_any_eager {
        return 0;
    }

    let total_pages = unmap_len / 4096;
    // Compute the eager-membership mask first (vm_lock), so the `as_lock` window below
    // is pure page-table work with no nested lock ordering to reason about.
    let mut skip = alloc::vec::Vec::with_capacity(total_pages);
    for i in 0..total_pages {
        let va = addr + i * 4096;
        skip.push(proc.vm_with_regions(|r| r.iter().any(|reg| reg.contains(va))));
    }
    let mut to_free = alloc::vec::Vec::new();
    proc.with_address_space(|aspace| {
        for (i, &skipped) in skip.iter().enumerate() {
            if skipped { continue; }
            let va = addr + i * 4096;
            if let Some(frame) = aspace.unmap_and_free_page_no_flush(va) {
                to_free.push(frame);
            }
        }
        // Some VAs in [addr, addr+unmap_len) may have been skipped (in_eager) or
        // never mapped, but flushing the whole span once is correct and cheaper
        // than tracking which pages we actually cleared.
        if total_pages > 0 {
            akuma_exec::mmu::flush_tlb_range_all_asid(addr, total_pages);
        }
    });
    for frame in to_free { crate::pmm::free_page_at(frame, akuma_pmm::FreeSite::MunmapSpan); }
    0
}
