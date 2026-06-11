use super::*;

// ── Linux mmap flag constants ────────────────────────────────────────────────
//
// Lifted from `sys_mmap` to module scope so the same bits are used by both
// `sys_mmap` and the diagnostic helpers below. Values match Linux AArch64.

pub(crate) const MAP_SHARED: u32 = 0x01;
pub(crate) const MAP_PRIVATE: u32 = 0x02;
pub(crate) const MAP_FIXED: u32 = 0x10;
pub(crate) const MAP_ANONYMOUS: u32 = 0x20;
pub(crate) const MAP_NORESERVE: u32 = 0x4000;
pub(crate) const MAP_POPULATE: u32 = 0x8000;
pub(crate) const MAP_STACK: u32 = 0x20000; // hint-only on Linux; ignored here
pub(crate) const MAP_FIXED_NOREPLACE: u32 = 0x100000;

pub(crate) const PROT_NONE: u32 = 0;

/// Returns `true` if a MAP_FIXED / MAP_FIXED_NOREPLACE call with the given
/// `addr` and `flags` would be rejected with `EINVAL` for **page misalignment**.
///
/// Mirrors the alignment guard in `sys_mmap`. Pure function over the syscall
/// inputs so kernel tests can assert that errno-shaped argument values
/// (e.g. crash14: `addr = 0xffffffffffffffea`) genuinely map to EINVAL when
/// MAP_FIXED is set, and *do not* trip this branch when it is not.
pub(crate) fn mmap_fixed_addr_unaligned_einval(addr: usize, flags: u32) -> bool {
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
pub(crate) fn mmap_fixed_overlaps_kernel_va(addr: usize, len: usize) -> bool {
    use akuma_exec::process::types::ProcessMemory;
    let pages = (len + 4095) / 4096;
    let map_end = addr.saturating_add(pages * 4096);
    // kernel_va_end() scales with detected RAM so this guard catches MAP_FIXED
    // overlaps with the full RAM identity map, not just a fixed 2GB window.
    addr < akuma_exec::mmu::kernel_va_end() && map_end > ProcessMemory::KERNEL_VA_START
}

pub(super) fn sys_brk(new_brk: usize) -> u64 {
    let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process(current_pid).map(|p| p.tgid).unwrap_or(current_pid);
    if let Some(proc) = akuma_exec::process::lookup_process(owner_pid) {
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
            let inode = crate::vfs::resolve_inode(&path).unwrap_or(0);
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
    let pages = (len + 4095) / 4096;
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
    let owner_pid = akuma_exec::process::lookup_process(current_pid).map(|p| p.tgid).unwrap_or(current_pid);
    let proc = match akuma_exec::process::lookup_process(owner_pid) {
        Some(p) => p,
        None => return ESRCH,
    };

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
            for i in 0..pages {
                let va = addr + i * 4096;
                let _ = proc.address_space.unmap_page(va);
            }
        }
        addr
    } else {
        match proc.memory.alloc_mmap(pages * 4096) {
            Some(a) => a,
            None => {
                crate::safe_print!(192, "[mmap] REJECT: pid={} size=0x{:x} next=0x{:x} limit=0x{:x}\n",
                    proc.pid, pages * 4096,
                    proc.memory.next_mmap.load(core::sync::atomic::Ordering::Relaxed),
                    proc.memory.mmap_limit);
                return ENOMEM;
            }
        }
    };

    let is_file_backed = flags & MAP_ANONYMOUS == 0 && fd >= 0;

    // MAP_SHARED on file-backed mappings: read-only MAP_SHARED is semantically identical
    // to MAP_PRIVATE (no writes → no CoW divergence), so we handle it silently.
    // Only warn for writable MAP_SHARED, which would require true shared-page semantics.
    if flags & MAP_SHARED != 0 && is_file_backed && (prot & 0x2) != 0 {
        crate::tprint!(192, "[mmap] MAP_SHARED file-backed writable unsupported (MAP_PRIVATE semantics): pid={} fd={}\n",
            proc.pid, fd);
    }

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
    if crate::config::MMAP_FILE_BACKED_LAZY && is_file_backed {
        if let Some(akuma_exec::process::FileDescriptor::File(ref f)) = proc.get_fd(fd as u32) {
            let path = f.path.clone();
            let inode = crate::vfs::resolve_inode(&path).unwrap_or(0);
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
    }

    let initial_flags = if is_file_backed {
        akuma_exec::mmu::user_flags::RW_NO_EXEC
    } else {
        page_flags
    };

    // Batch-allocate all pages in a single PMM lock acquisition, then map
    // them with no_flush and issue a single TLB flush after the loop.
    let frame_batch = match crate::pmm::alloc_pages_zeroed(pages) {
        Some(b) => b,
        None => {
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
        }
    };
    let _ = map_populate; // populate is now subsumed by the lazy fallback above
    let mut frames = alloc::vec::Vec::with_capacity(pages);
    for (i, frame) in frame_batch.into_iter().enumerate() {
        let (table_frames, _) = unsafe {
            akuma_exec::mmu::map_user_page_no_flush(mmap_addr + i * 4096, frame.addr, initial_flags)
        };
        proc.address_space.track_user_frame(frame);
        for tf in table_frames {
            proc.address_space.track_page_table_frame(tf);
        }
        frames.push(frame);
    }
    crate::pmm::dp_count(&crate::pmm::EAGER_MMAP_PAGES, pages);
    // Single TLB flush for the entire mmap range.
    akuma_exec::mmu::flush_tlb_range(mmap_addr, pages);
    if is_file_backed {
        if let Some(akuma_exec::process::FileDescriptor::File(f)) = proc.get_fd(fd as u32) {
            let path = f.path.clone();
            let mut file_off = offset;
            let mut bytes_read = 0usize;
            for i in 0..pages {
                let chunk = core::cmp::min(4096, len.saturating_sub(i * 4096));
                if chunk == 0 { break; }
                let page_kva = akuma_exec::mmu::phys_to_virt(frames[i].addr);
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
        if page_flags != initial_flags {
            for i in 0..pages {
                let _ = proc.address_space.update_page_flags(mmap_addr + i * 4096, page_flags);
            }
        }
    } else {
        crate::tprint!(128, "[mmap] pid={} len=0x{:x} prot=0x{:x} flags=0x{:x} = 0x{:x} (eager)\n",
            proc.pid, len, prot, flags, mmap_addr);
    }

    proc.vm_with_regions(|r| r.push((mmap_addr, frames)));

    mmap_addr as u64
}

pub(super) fn sys_mremap(old_addr: usize, old_size: usize, new_size: usize, flags: u32) -> u64 {
    if new_size == 0 { return EINVAL; }
    if old_addr & 0xFFF != 0 { return EINVAL; }
    const MREMAP_MAYMOVE: u32 = 1;

    let va_limit = user_va_limit() as usize;
    if old_addr >= va_limit { return EFAULT; }

    let old_pages = (old_size + 4095) / 4096;
    let new_pages = (new_size + 4095) / 4096;

    if new_pages <= old_pages {
        return old_addr as u64;
    }

    if flags & MREMAP_MAYMOVE == 0 {
        let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process(current_pid).map(|p| p.tgid).unwrap_or(current_pid);
        let lazy_key = akuma_exec::process::lookup_process(owner_pid).map(|p| p.tgid).unwrap_or(owner_pid);
        let is_mapped = akuma_exec::mmu::is_current_user_page_mapped(old_addr)
            || akuma_exec::process::lazy_region_lookup_for_pid(lazy_key, old_addr).is_some()
            || akuma_exec::process::lookup_process(owner_pid)
                .map(|p| p.vm_with_regions(|r| r.iter().any(|(start, frames)| {
                    old_addr >= *start && old_addr < *start + frames.len() * 4096
                })))
                .unwrap_or(false);
        return if is_mapped { ENOMEM } else { EFAULT };
    }

    let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process(current_pid).map(|p| p.tgid).unwrap_or(current_pid);
    let new_addr = match akuma_exec::process::lookup_process(owner_pid)
        .and_then(|p| p.memory.alloc_mmap(new_pages * 4096)) {
        Some(a) => a,
        None => return ENOMEM,
    };

    if let Some(proc) = akuma_exec::process::lookup_process(owner_pid) {
        let mut new_frames = alloc::vec::Vec::new();
        for i in 0..new_pages {
            if let Some(frame) = crate::pmm::alloc_page_zeroed() {
                new_frames.push(frame);
                let (table_frames, _) = unsafe { akuma_exec::mmu::map_user_page(new_addr + i * 4096, frame.addr, akuma_exec::mmu::user_flags::RW_NO_EXEC) };
                proc.address_space.track_user_frame(frame);
                for tf in table_frames {
                    proc.address_space.track_page_table_frame(tf);
                }
            } else { return ENOMEM; }
        }

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

        proc.vm_with_regions(|r| r.push((new_addr, new_frames)));

        let mut found_eager = false;
        // Remove the old region under the lock, then unmap/free its frames after
        // releasing it (unmap/free must not run while vm_lock is held).
        let old_frames_opt = proc.vm_with_regions(|r| {
            r.iter().position(|(va, _)| *va == old_addr).map(|idx| r.remove(idx).1)
        });
        if let Some(old_frames) = old_frames_opt {
            let freed_size = old_frames.len() * 4096;
            for (i, frame) in old_frames.into_iter().enumerate() {
                let _ = proc.address_space.unmap_page(old_addr + i * 4096);
                // Free only when this drops the frame's last reference; an
                // aliased/shared PA is freed by its surviving owner instead.
                if proc.address_space.remove_user_frame(frame) {
                    crate::pmm::free_page(frame);
                }
            }
            proc.memory.free_regions.push((old_addr, freed_size));
            found_eager = true;
        }

        if !found_eager {
            let lazy_results = akuma_exec::process::munmap_lazy_regions_in_range(proc.tgid, old_addr, old_pages * 4096);
            for &(freed_start, freed_pages) in &lazy_results {
                for i in 0..freed_pages {
                    if let Some(frame) = proc.address_space.unmap_and_free_page(freed_start + i * 4096) {
                        crate::pmm::free_page(frame);
                    }
                }
            }
            for i in 0..old_pages {
                let va = old_addr + i * 4096;
                if let Some(frame) = proc.address_space.unmap_and_free_page(va) {
                    crate::pmm::free_page(frame);
                }
            }
            proc.memory.free_regions.push((old_addr, old_pages * 4096));
        }

        new_addr as u64
    } else { ENOMEM }
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
    let owner_pid = akuma_exec::process::lookup_process(current_pid).map(|p| p.tgid).unwrap_or(current_pid);
            let proc = match akuma_exec::process::lookup_process(owner_pid) {
                Some(p) => p,
                None => return 0,
            };
            let aligned_addr = addr & !0xFFF;
            let end = (addr.saturating_add(len) + 0xFFF) & !0xFFF;

            // Collect (va, flags) pairs for pages in lazy regions not yet mapped.
            let mut prefault: alloc::vec::Vec<(usize, u64)> = alloc::vec::Vec::new();
            let mut va = aligned_addr;
            while va < end {
                if !akuma_exec::mmu::is_current_user_page_mapped(va) {
                    if let Some((flags, _, _, _)) =
                        akuma_exec::process::lazy_region_lookup_for_pid(proc.tgid, va)
                    {
                        prefault.push((va, flags));
                    }
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
            for (idx, (page_va, flags)) in prefault.into_iter().enumerate() {
                let frame = frames[idx];
                let (table_frames, _) = unsafe {
                    akuma_exec::mmu::map_user_page_no_flush(page_va, frame.addr, flags)
                };
                proc.address_space.track_user_frame(frame);
                for tf in table_frames {
                    proc.address_space.track_page_table_frame(tf);
                }
            }
            // Flush the entire requested range (covers all newly mapped pages).
            akuma_exec::mmu::flush_tlb_range(aligned_addr, (end - aligned_addr) / 4096);
            0
        }
        MADV_DONTNEED => {
            let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process(current_pid).map(|p| p.tgid).unwrap_or(current_pid);
            let proc = match akuma_exec::process::lookup_process(owner_pid) {
                Some(p) => p,
                None => return 0,
            };
            let aligned_addr = addr & !0xFFF;
            let aligned_len = ((addr + len + 0xFFF) & !0xFFF) - aligned_addr;
            let pages = aligned_len / 4096;
            for i in 0..pages {
                proc.address_space.zero_mapped_page(aligned_addr + i * 4096);
            }
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
    let pages = (len + 4095) / 4096;
    let new_flags = akuma_exec::mmu::user_flags::from_prot(prot);
    let adding_exec = prot & 0x4 != 0;
    let current_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let owner_pid = akuma_exec::process::lookup_process(current_pid).map(|p| p.tgid).unwrap_or(current_pid);
    crate::tprint!(128, "[mprotect] pid={} owner={} addr=0x{:x} len=0x{:x} prot={:#x}\n",
        current_pid, owner_pid, addr, pages * 4096, prot);
    if let Some(proc) = akuma_exec::process::lookup_process(owner_pid) {
        akuma_exec::process::update_lazy_region_flags(proc.tgid, addr, pages * 4096, new_flags);

        // Update all page table entries with no_flush, then issue a single
        // TLB range flush. Previously each update_page_flags call issued its
        // own dsb+tlbi+dsb+isb, causing O(pages) expensive barrier sequences.
        let mut any_updated = false;
        for i in 0..pages {
            let va = addr + i * 4096;
            if proc.address_space.is_mapped(va) {
                let _ = proc.address_space.update_page_flags_no_flush(va, new_flags);
                any_updated = true;
            }
        }
        if any_updated {
            akuma_exec::mmu::flush_tlb_range(addr, pages);
        }
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
    let owner_pid = akuma_exec::process::lookup_process(current_pid).map(|p| p.tgid).unwrap_or(current_pid);
    let proc = match akuma_exec::process::lookup_process(owner_pid) {
        Some(p) => p,
        None => return ESRCH,
    };

    let unmap_len = if len > 0 { (len + 4095) & !4095 } else { 4096 };
    let unmap_pages = unmap_len / 4096;

    // Locate & detach the eager region under vm_lock (pure Vec ops only): for a
    // full unmap, remove it; for a partial prefix, remove it, split off the prefix
    // frames, and re-push the remaining suffix. The actual page unmap + frame free
    // happens AFTER the lock is released (it takes other locks and must not run
    // while vm_lock is held). Returns (base_va, frames_to_unmap) or None.
    let detached = proc.vm_with_regions(|r| {
        let idx = r.iter().position(|(start, _)| *start == addr)?;
        let region_pages = r[idx].1.len();
        if unmap_pages >= region_pages {
            let (_, frames) = r.remove(idx);
            Some((addr, frames))
        } else {
            let (old_start, old_frames) = r.remove(idx);
            let mut iter = old_frames.into_iter();
            let prefix: Vec<crate::pmm::PhysFrame> = (0..unmap_pages).filter_map(|_| iter.next()).collect();
            let remaining: Vec<crate::pmm::PhysFrame> = iter.collect();
            if !remaining.is_empty() {
                r.push((old_start + unmap_pages * 4096, remaining));
            }
            Some((old_start, prefix))
        }
    });
    if let Some((base, frames)) = detached {
        let n = frames.len();
        crate::tprint!(128, "[munmap] pid={} addr=0x{:x} ({} pages, base=0x{:x})\n",
            proc.pid, addr, n, base);
        // Defer the TLB flush: clear each PTE without a per-page barrier,
        // then flush the whole region once (cheap-win E, COW_OPTIMIZATIONS.md).
        for (i, frame) in frames.into_iter().enumerate() {
            let _ = proc.address_space.unmap_page_no_flush(base + i * 4096);
            // Free only when this drops the frame's last reference; an
            // aliased/shared PA is freed by its surviving owner instead.
            if proc.address_space.remove_user_frame(frame) {
                crate::pmm::free_page(frame);
            }
        }
        akuma_exec::mmu::flush_tlb_range_all_asid(base, n);
        proc.memory.free_regions.push((addr, n * 4096));
        return 0;
    }

    let results = akuma_exec::process::munmap_lazy_regions_in_range(proc.tgid, addr, unmap_len);
    if !results.is_empty() {
        for &(freed_start, freed_pages) in &results {
            let mut had_physical = false;
            for i in 0..freed_pages {
                if let Some(frame) = proc.address_space.unmap_and_free_page_no_flush(freed_start + i * 4096) {
                    crate::pmm::free_page(frame);
                    had_physical = true;
                }
            }
            akuma_exec::mmu::flush_tlb_range_all_asid(freed_start, freed_pages);
            // Only recycle the VA range when physical pages were actually freed.
            // Pure lazy (PROT_NONE, never demand-paged) regions must NOT be put
            // back in free_regions: alloc_mmap prefers free_regions over
            // next_mmap, which causes an infinite mmap→reject→munmap→same-addr
            // loop (observed with Go's heap prober returning 0x100000000 60+
            // times in succession).
            if had_physical {
                proc.memory.free_regions.push((freed_start, freed_pages * 4096));
            }
        }
        return 0;
    }

    let total_pages = unmap_len / 4096;
    for i in 0..total_pages {
        let va = addr + i * 4096;
        let in_eager = proc.vm_with_regions(|r| r.iter().any(|(start, frames)| {
            va >= *start && va < *start + frames.len() * 4096
        }));
        if !in_eager {
            if let Some(frame) = proc.address_space.unmap_and_free_page_no_flush(va) {
                crate::pmm::free_page(frame);
            }
        }
    }
    // Some VAs in [addr, addr+unmap_len) may have been skipped (in_eager) or
    // never mapped, but flushing the whole span once is correct and cheaper
    // than tracking which pages we actually cleared.
    if total_pages > 0 {
        akuma_exec::mmu::flush_tlb_range_all_asid(addr, total_pages);
    }
    0
}
