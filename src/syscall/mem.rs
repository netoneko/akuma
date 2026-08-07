use super::*;
use akuma_exec::process::MmapRegion;

/// RAII guard that runs a memory-management syscall **without** the Big Kernel
/// Lock — Phase 5 of docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md. Mirrors
/// [`super::fs::VfsBklGuard`] exactly, including the latching discipline.
///
/// Constructed at the top of `sys_mprotect`/`sys_madvise`/`sys_munmap`/
/// `sys_mremap`/`sys_mmap` (after early-error/arg-validation returns — an
/// early `EINVAL` on a malformed length never touches the state this guard
/// exists to protect, so it shouldn't pay for a drop+reacquire): `new()` DROPS
/// the BKL so this core runs the syscall concurrently with peer cores, and
/// `drop()` RE-ACQUIRES it on every return path. Correctness rests on the state
/// these syscalls mutate already carrying its own fine-grained lock —
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
///
/// Zero-cost no-op unless BOTH `kernel_smp_shared` and `kernel_no_bkl_mm` are set
/// (or the runtime toggle `mm_bkl_drop_enabled()` is off) — the struct is empty
/// and `new`/`drop` compile to nothing.
pub(super) struct MmBklGuard {
    /// Whether `new()` actually dropped the BKL, **latched at construction** —
    /// same reasoning as `VfsBklGuard::dropped_bkl`: `drop()` must not re-read
    /// `mm_bkl_drop_enabled()`, since a toggle flip mid-syscall would otherwise
    /// unbalance the syscall wrapper's single `leave_kernel`.
    #[cfg(all(kernel_smp_shared, kernel_no_bkl_mm))]
    dropped_bkl: bool,
}

impl MmBklGuard {
    #[inline]
    pub(super) fn new() -> Self {
        #[cfg(all(kernel_smp_shared, kernel_no_bkl_mm))]
        let dropped_bkl = crate::smp_shared::mm_bkl_drop_enabled();
        #[cfg(all(kernel_smp_shared, kernel_no_bkl_mm))]
        if dropped_bkl {
            akuma_exec::bkl::dropped_window_open();
        }
        Self {
            #[cfg(all(kernel_smp_shared, kernel_no_bkl_mm))]
            dropped_bkl,
        }
    }
}

impl Drop for MmBklGuard {
    #[inline]
    fn drop(&mut self) {
        // Latched in `new()` — deliberately NOT a fresh `mm_bkl_drop_enabled()` read.
        #[cfg(all(kernel_smp_shared, kernel_no_bkl_mm))]
        if self.dropped_bkl {
            akuma_exec::bkl::dropped_window_close();
        }
    }
}

// ── Linux mmap flag constants ────────────────────────────────────────────────
//
// Lifted from `sys_mmap` to module scope so the same bits are used by both
// `sys_mmap` and the diagnostic helpers below. Values match Linux AArch64.

pub const MAP_SHARED: u32 = 0x01;
pub const MAP_PRIVATE: u32 = 0x02;
pub const MAP_FIXED: u32 = 0x10;
pub const MAP_ANONYMOUS: u32 = 0x20;
pub const MAP_NORESERVE: u32 = 0x4000;
pub const MAP_POPULATE: u32 = 0x8000;
pub const MAP_STACK: u32 = 0x20000; // hint-only on Linux; ignored here
pub const MAP_FIXED_NOREPLACE: u32 = 0x100000;

pub const PROT_NONE: u32 = 0;
pub const PROT_WRITE: u32 = 0x2;

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

/// Returns `true` if a MAP_FIXED / MAP_FIXED_NOREPLACE call with the given
/// `addr` and `flags` would be rejected with `EINVAL` for **page misalignment**.
///
/// Mirrors the alignment guard in `sys_mmap`. Pure function over the syscall
/// inputs so kernel tests can assert that errno-shaped argument values
/// (e.g. crash14: `addr = 0xffffffffffffffea`) genuinely map to EINVAL when
/// MAP_FIXED is set, and *do not* trip this branch when it is not.
pub fn mmap_fixed_addr_unaligned_einval(addr: usize, flags: u32) -> bool {
    let is_fixed = (flags & MAP_FIXED) != 0;
    let is_fixed_noreplace = (flags & MAP_FIXED_NOREPLACE) != 0;
    (is_fixed || is_fixed_noreplace) && addr != 0 && (addr & 0xFFF) != 0
}

/// Returns `true` if a MAP_FIXED mapping would overlap the kernel
/// identity-map VA range (and thus be rejected with `EINVAL`).
///
/// Same predicate as the in-line guard in `sys_mmap`; kept here so the
/// diagnostic logger can derive a one-token reason hint without re-walking
/// the syscall body.
pub fn mmap_fixed_overlaps_kernel_va(addr: usize, len: usize) -> bool {
    use akuma_exec::process::types::ProcessMemory;
    let pages = len.div_ceil(4096);
    let map_end = addr.saturating_add(pages * 4096);
    // kernel_va_end() scales with detected RAM so this guard catches MAP_FIXED
    // overlaps with the full RAM identity map, not just a fixed 2GB window.
    addr < akuma_exec::mmu::kernel_va_end() && map_end > ProcessMemory::KERNEL_VA_START
}

pub(super) fn sys_brk(new_brk: usize) -> u64 {
    let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process_shared(current_pid).map_or(current_pid, |p| p.tgid);
    if let Some(proc) = akuma_exec::process::lookup_process_shared(owner_pid) {
        if new_brk == 0 { proc.get_brk() as u64 } else { proc.set_brk(new_brk) as u64 }
    } else { 0 }
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
            let inode = {
                let _vfs_window = super::fs::VfsBklGuard::new();
                crate::vfs::resolve_inode(&path).unwrap_or(0)
            };
            let source = akuma_exec::process::LazySource::File {
                path, inode, file_offset: offset, filesz: len, segment_va: mmap_addr,
            };
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

    let is_lazy = prot == PROT_NONE && (flags & MAP_ANONYMOUS != 0);
    let is_fixed = flags & MAP_FIXED != 0;
    let is_fixed_noreplace = flags & MAP_FIXED_NOREPLACE != 0;
    let map_populate = flags & MAP_POPULATE != 0;

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

    let is_file_backed = flags & MAP_ANONYMOUS == 0 && fd >= 0;

    // Writable MAP_SHARED on a file-backed mapping has true shared-page semantics:
    // writes through the mapping must become visible in the file. Akuma has no
    // unified page cache, so we honor this by mapping it EAGERLY (resident,
    // writable, populated from the file) and writing the pages back to the file on
    // munmap/msync/exit (see SHARED_FILE_MAPPINGS). Read-only MAP_SHARED has no
    // writes to flush, so it stays on the cheap lazy MAP_PRIVATE-equivalent path.
    let is_shared_writable = (flags & MAP_SHARED != 0) && is_file_backed && (prot & PROT_WRITE != 0);

    // MAP_POPULATE requests eager pre-faulting; it suppresses lazy allocation.
    // MADV_WILLNEED can also trigger pre-faulting on existing lazy regions.
    // Anonymous private mappings above MMAP_EAGER_MAX_PAGES are demand-paged
    // (zero-fill on first touch) rather than eagerly allocated+zeroed+mapped.
    // This is the "lazy/zero-on-demand population" win from COW_OPTIMIZATIONS.md:
    // pages that are never touched are never allocated, which cuts the physical
    // footprint (the rustc trace ended near OOM from eager over-commit). Small
    // mappings stay eager — see config::MMAP_EAGER_MAX_PAGES for the rationale.
    let use_lazy = !is_file_backed && !map_populate && (
        is_lazy ||
        (flags & MAP_NORESERVE != 0) ||
        pages > crate::config::MMAP_EAGER_MAX_PAGES
    );

    if use_lazy {
        let count = akuma_exec::process::push_lazy_region(proc.tgid, mmap_addr, pages * 4096, page_flags);
        crate::tprint!(192, "[mmap] pid={} len=0x{:x} prot=0x{:x} flags=0x{:x} = 0x{:x} (lazy, {} regions)\n",
            proc.pid, len, prot, flags, mmap_addr, count);
        return mmap_addr as u64;
    }

    // When MMAP_FILE_BACKED_LAZY is set, demand-page file-backed mmaps instead
    // of eagerly allocating all frames. Default on the size profile where PMM
    // is tight (8 MB): eagerly mapping a 600 KB shared library exhausts user
    // pages before the process can start. Pages are faulted in via
    // LazySource::File, same mechanism as demand-paged ELFs.
    // Writable MAP_SHARED is forced eager (see below) so its pages are all
    // resident for writeback; everything else may demand-page lazily.
    if crate::config::MMAP_FILE_BACKED_LAZY && is_file_backed && !is_shared_writable
        && let Some(akuma_exec::process::FileDescriptor::File(ref f)) = proc.get_fd(fd as u32) {
            let path = f.path.clone();
            // Path→inode resolution reads ext2 metadata (real I/O on a cold cache) —
            // take the dropped-BKL window for it like every other on-disk read path
            // (Phase 2e of the no-bkl-vfs carve-out).
            let inode = {
                let _vfs_window = super::fs::VfsBklGuard::new();
                crate::vfs::resolve_inode(&path).unwrap_or(0)
            };
            let source = akuma_exec::process::LazySource::File {
                path: path.clone(),
                inode,
                file_offset: offset,
                filesz: len,
                segment_va: mmap_addr,
            };
            let count = akuma_exec::process::push_lazy_region_with_source(
                proc.tgid, mmap_addr, pages * 4096, page_flags, source);
            crate::tprint!(192, "[mmap] pid={} fd={} file={} off={} len=0x{:x} = 0x{:x} (lazy-file, {} regions)\n",
                proc.pid, fd, &path, offset, len, mmap_addr, count);
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
    let _ = map_populate; // populate is now subsumed by the lazy fallback above

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
    } else {
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
            crate::tprint!(192, "[mmap] pid={} fd={} file={} off={} len=0x{:x} = 0x{:x} (shared-writable, writeback on)\n",
                proc.pid, fd, &f.path, offset, len, mmap_addr);
        }

    proc.vm_with_regions(|r| r.push(MmapRegion::owned_with_flags(mmap_addr, frames, page_flags)));

    mmap_addr as u64
}

pub(super) fn sys_mremap(old_addr: usize, old_size: usize, new_size: usize, flags: u32) -> u64 {
    if new_size == 0 { return EINVAL; }
    if old_addr & 0xFFF != 0 { return EINVAL; }
    const MREMAP_MAYMOVE: u32 = 1;

    let va_limit = user_va_limit() as usize;
    if old_addr >= va_limit { return EFAULT; }

    let old_pages = old_size.div_ceil(4096);
    let new_pages = new_size.div_ceil(4096);

    if new_pages <= old_pages {
        return old_addr as u64;
    }

    // Resolve the address-space owner ONCE, while the BKL is still held, and reuse
    // this reference for the rest of the call — the process table itself has no
    // inner lock (BKL_PROCESS_CARVE_OUT.md's audit), so every syscall carved out of
    // the BKL so far resolves its process reference before opening the drop window
    // rather than re-walking the table inside it.
    let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process_shared(current_pid).map_or(current_pid, |p| p.tgid);
    let proc = akuma_exec::process::lookup_process_shared(owner_pid);
    let _mm_window = MmBklGuard::new();

    if flags & MREMAP_MAYMOVE == 0 {
        let is_mapped = akuma_exec::mmu::is_current_user_page_mapped(old_addr)
            || akuma_exec::process::lazy_region_lookup_for_pid(owner_pid, old_addr).is_some()
            || proc.is_some_and(|p| p.vm_with_regions(|r| r.iter().any(|reg| reg.contains(old_addr))));
        return if is_mapped { ENOMEM } else { EFAULT };
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
                for f in new_frames { crate::pmm::free_page(f); }
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
                if unsafe { copy_from_user_safe(kernel_buf.as_mut_ptr(), (old_addr + total_copied) as *const u8, chunk).is_err() } {
                    break; 
                }
                if unsafe { copy_to_user_safe((new_addr + total_copied) as *mut u8, kernel_buf.as_ptr(), chunk).is_err() } {
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
            for frame in to_free { crate::pmm::free_page(frame); }
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
            for frame in to_free { crate::pmm::free_page(frame); }
            proc.vm_free_mmap(old_addr, old_pages * 4096);
        }

        new_addr as u64
    }
}

pub(super) fn sys_madvise(addr: usize, len: usize, advice: i32) -> u64 {
    const MADV_WILLNEED: i32 = 3;
    const MADV_DONTNEED: i32 = 4;
    const MADV_FREE: i32 = 8;

    match advice {
        MADV_WILLNEED => {
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
        MADV_DONTNEED => {
            let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process_shared(current_pid).map_or(current_pid, |p| p.tgid);
            let proc = match akuma_exec::process::lookup_process_shared(owner_pid) {
                Some(p) => p,
                None => return 0,
            };
            let _mm_window = MmBklGuard::new();
            let aligned_addr = addr & !0xFFF;
            let aligned_len = ((addr + len + 0xFFF) & !0xFFF) - aligned_addr;
            let pages = aligned_len / 4096;
            // `zero_mapped_page` reads page-table state to find each frame; hold
            // `as_lock` so a concurrent BKL-free fault can't edit the tables under it.
            proc.with_address_space(|aspace| {
                for i in 0..pages {
                    aspace.zero_mapped_page(aligned_addr + i * 4096);
                }
            });
            0
        }
        MADV_FREE => 0,
        _ => 0,
    }
}

pub fn membarrier_cmd(cmd: u32) -> u64 {
    const CMD_QUERY: u32 = 0;
    const CMD_PRIVATE_EXPEDITED: u32 = 8;
    const CMD_REGISTER_PRIVATE_EXPEDITED: u32 = 16;
    const SUPPORTED: u64 = 0x18;

    match cmd {
        CMD_QUERY => SUPPORTED,
        CMD_REGISTER_PRIVATE_EXPEDITED => 0,
        CMD_PRIVATE_EXPEDITED => {
            unsafe {
                core::arch::asm!("dsb ish");
                core::arch::asm!("isb");
            }
            0
        }
        _ => EINVAL,
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
    crate::tprint!(128, "[mprotect] pid={} owner={} addr=0x{:x} len=0x{:x} prot={:#x}\n",
        current_pid, owner_pid, addr, pages * 4096, prot);
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

    let unmap_len = if len > 0 { (len + 4095) & !4095 } else { 4096 };
    let unmap_pages = unmap_len / 4096;

    // Locate & detach the eager region under vm_lock (pure Vec ops only): for a
    // full unmap, remove it; for a partial prefix, remove it, split off the prefix
    // frames, and re-push the remaining suffix. The actual page unmap + frame free
    // happens AFTER the lock is released (it takes other locks and must not run
    // while vm_lock is held). Returns (base_va, pages_to_unmap, owned_frames) or None.
    //
    // `pages_to_unmap` comes from `MmapRegion::pages`, not the frame count: a
    // CoW-inherited region has every page mapped but owns no frames, and sizing
    // the unmap by `frames.len()` would silently unmap nothing and leak the VA
    // range back to `alloc_mmap` while the pages stayed live.
    let detached = proc.vm_with_regions(|r| {
        let idx = r.iter().position(|reg| reg.start_va == addr)?;
        let region_pages = r[idx].pages;
        if unmap_pages >= region_pages {
            let reg = r.remove(idx);
            Some((addr, region_pages, reg.frames))
        } else {
            let reg = r.remove(idx);
            let mut iter = reg.frames.into_iter();
            let prefix: Vec<crate::pmm::PhysFrame> = (0..unmap_pages).filter_map(|_| iter.next()).collect();
            let remaining: Vec<crate::pmm::PhysFrame> = iter.collect();
            r.push(MmapRegion {
                start_va: reg.start_va + unmap_pages * 4096,
                pages: region_pages - unmap_pages,
                frames: remaining,
                // The surviving suffix keeps the protection of the region it was
                // split out of; a partial unmap changes extent, not permission.
                flags: reg.flags,
            });
            Some((reg.start_va, unmap_pages, prefix))
        }
    });
    if let Some((base, n, frames)) = detached {
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
                match frames.get(i) {
                    // Frame this process owns: unmap the PTE, then drop our
                    // reference. Free only when this drops the frame's last
                    // reference; an aliased/shared PA is freed by its surviving
                    // owner instead.
                    Some(&frame) => {
                        let _ = aspace.unmap_page_no_flush(va);
                        if aspace.remove_user_frame(frame) {
                            to_free.push(frame);
                        }
                    }
                    // CoW-inherited page (no owned frame recorded): take the PA
                    // from the live PTE instead, which also covers the case where
                    // a CoW write fault already swapped in a private frame.
                    None => {
                        if let Some(frame) = aspace.unmap_and_free_page_no_flush(va) {
                            to_free.push(frame);
                        }
                    }
                }
            }
            akuma_exec::mmu::flush_tlb_range_all_asid(base, n);
        });
        for frame in to_free { crate::pmm::free_page(frame); }
        proc.vm_free_mmap(addr, n * 4096);
        return 0;
    }

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
            for frame in to_free { crate::pmm::free_page(frame); }
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
    for frame in to_free { crate::pmm::free_page(frame); }
    0
}
