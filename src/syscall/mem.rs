use super::*;

pub(super) fn sys_brk(new_brk: usize) -> u64 {
    let owner_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    if let Some(proc) = akuma_exec::process::lookup_process(owner_pid) {
        if new_brk == 0 { proc.get_brk() as u64 } else { proc.set_brk(new_brk) as u64 }
    } else { 0 }
}

pub(super) fn sys_mmap(addr: usize, len: usize, prot: u32, flags: u32, fd: i32, offset: usize) -> u64 {
    if len == 0 { return !0u64; }
    let pages = (len + 4095) / 4096;
    let page_flags = akuma_exec::mmu::user_flags::from_prot(prot);

    const MAP_ANONYMOUS: u32 = 0x20;
    const MAP_FIXED: u32 = 0x10;
    const MAP_NORESERVE: u32 = 0x4000;
    const MAP_FIXED_NOREPLACE: u32 = 0x100000;
    const PROT_NONE: u32 = 0;

    let is_lazy = prot == PROT_NONE && (flags & MAP_ANONYMOUS != 0);
    let is_fixed = flags & MAP_FIXED != 0;
    let is_fixed_noreplace = flags & MAP_FIXED_NOREPLACE != 0;

    let owner_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let proc = match akuma_exec::process::lookup_process(owner_pid) {
        Some(p) => p,
        None => return !0u64,
    };

    let mmap_addr = if (is_fixed || is_fixed_noreplace) && addr != 0 {
        if addr & 0xFFF != 0 { return !0u64; }
        // Reject MAP_FIXED mappings that overlap the kernel identity-map range.
        // The Go runtime uses MAP_FIXED to commit its heap arenas; without this
        // guard a process can map user pages at e.g. 0x8000_0000, overlapping the
        // kernel's physical-RAM identity map and causing silent memory corruption.
        {
            use akuma_exec::process::types::ProcessMemory;
            let map_end = addr.saturating_add(pages * 4096);
            if addr < ProcessMemory::KERNEL_VA_END
                && map_end > ProcessMemory::KERNEL_VA_START
            {
                crate::tprint!(128, "[mmap] REJECT MAP_FIXED kernel VA: pid={} addr=0x{:x} len=0x{:x}\n",
                    proc.pid, addr, pages * 4096);
                return EINVAL;
            }
        }
        if is_fixed {
            let as_pid = akuma_exec::process::read_current_pid().unwrap_or(proc.pid);
            let _ = akuma_exec::process::munmap_lazy_regions_in_range(as_pid, addr, pages * 4096);
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
                    proc.pid, pages * 4096, proc.memory.next_mmap, proc.memory.mmap_limit);
                return !0u64;
            }
        }
    };

    let is_file_backed = flags & MAP_ANONYMOUS == 0 && fd >= 0;

    let use_lazy = !is_file_backed && (
        is_lazy ||
        (flags & MAP_NORESERVE != 0) ||
        pages > 256
    );

    if use_lazy {
        let as_pid = akuma_exec::process::read_current_pid().unwrap_or(proc.pid);
        let count = akuma_exec::process::push_lazy_region(as_pid, mmap_addr, pages * 4096, page_flags);
        crate::tprint!(192, "[mmap] pid={} len=0x{:x} prot=0x{:x} flags=0x{:x} = 0x{:x} (lazy, {} regions)\n",
            proc.pid, len, prot, flags, mmap_addr, count);
        return mmap_addr as u64;
    }

    let initial_flags = if is_file_backed {
        akuma_exec::mmu::user_flags::RW_NO_EXEC
    } else {
        page_flags
    };

    let mut frames = alloc::vec::Vec::new();
    for i in 0..pages {
        if let Some(frame) = crate::pmm::alloc_page_zeroed() {
            frames.push(frame);
            let (table_frames, _) = unsafe { akuma_exec::mmu::map_user_page(mmap_addr + i * 4096, frame.addr, initial_flags) };
            proc.address_space.track_user_frame(frame);
            for tf in table_frames {
                proc.address_space.track_page_table_frame(tf);
            }
        } else {
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::tprint!(128, "[mmap] pid={} len=0x{:x} FAIL OOM at page {}/{}\n",
                    proc.pid, len, i, pages);
            }
            return !0u64;
        }
    }
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

    proc.mmap_regions.push((mmap_addr, frames));

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
        let owner_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
        let is_mapped = akuma_exec::mmu::is_current_user_page_mapped(old_addr)
            || akuma_exec::process::lazy_region_lookup_for_pid(owner_pid, old_addr).is_some()
            || akuma_exec::process::lookup_process(owner_pid)
                .map(|p| p.mmap_regions.iter().any(|(start, frames)| {
                    old_addr >= *start && old_addr < *start + frames.len() * 4096
                }))
                .unwrap_or(false);
        return if is_mapped { ENOMEM } else { EFAULT };
    }

    let owner_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
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

        proc.mmap_regions.push((new_addr, new_frames));

        let mut found_eager = false;
        if let Some(idx) = proc.mmap_regions.iter().position(|(va, _)| *va == old_addr) {
            let (_, old_frames) = proc.mmap_regions.remove(idx);
            let freed_size = old_frames.len() * 4096;
            for (i, frame) in old_frames.into_iter().enumerate() {
                let _ = proc.address_space.unmap_page(old_addr + i * 4096);
                proc.address_space.remove_user_frame(frame);
                crate::pmm::free_page(frame);
            }
            proc.memory.free_regions.push((old_addr, freed_size));
            found_eager = true;
        }

        if !found_eager {
            let lazy_results = akuma_exec::process::munmap_lazy_regions_in_range(owner_pid, old_addr, old_pages * 4096);
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
    const MADV_DONTNEED: i32 = 4;
    const MADV_FREE: i32 = 8;

    match advice {
        MADV_DONTNEED => {
            let owner_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
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
    let owner_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    if let Some(proc) = akuma_exec::process::lookup_process(owner_pid) {
        akuma_exec::process::update_lazy_region_flags(owner_pid, addr, pages * 4096, new_flags);

        for i in 0..pages {
            let va = addr + i * 4096;
            if proc.address_space.is_mapped(va) {
                let _ = proc.address_space.update_page_flags(va, new_flags);
            }
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
        EINVAL
    }
}

pub(super) fn sys_munmap(addr: usize, len: usize) -> u64 {
    let owner_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let proc = match akuma_exec::process::lookup_process(owner_pid) {
        Some(p) => p,
        None => return !0u64,
    };

    let unmap_len = if len > 0 { (len + 4095) & !4095 } else { 4096 };
    let unmap_pages = unmap_len / 4096;

    if let Some(idx) = proc.mmap_regions.iter().position(|(start, _)| *start == addr) {
        let region_pages = proc.mmap_regions[idx].1.len();
        if unmap_pages >= region_pages {
            let (_, frames) = proc.mmap_regions.remove(idx);
            let freed_size = frames.len() * 4096;
            crate::tprint!(128, "[munmap] pid={} addr=0x{:x} full ({} pages)\n",
                proc.pid, addr, frames.len());
            for (i, frame) in frames.into_iter().enumerate() {
                let _ = proc.address_space.unmap_page(addr + i * 4096);
                proc.address_space.remove_user_frame(frame);
                crate::pmm::free_page(frame);
            }
            proc.memory.free_regions.push((addr, freed_size));
        } else {
            let (old_start, old_frames) = proc.mmap_regions.remove(idx);
            crate::tprint!(192, "[munmap] pid={} addr=0x{:x} partial prefix {}/{} pages\n",
                proc.pid, addr, unmap_pages, old_frames.len());
            let mut iter = old_frames.into_iter();
            for i in 0..unmap_pages {
                if let Some(frame) = iter.next() {
                    let _ = proc.address_space.unmap_page(old_start + i * 4096);
                    proc.address_space.remove_user_frame(frame);
                    crate::pmm::free_page(frame);
                }
            }
            let remaining: Vec<crate::pmm::PhysFrame> = iter.collect();
            if !remaining.is_empty() {
                let new_start = old_start + unmap_pages * 4096;
                proc.mmap_regions.push((new_start, remaining));
            }
            proc.memory.free_regions.push((addr, unmap_pages * 4096));
        }
        return 0;
    }

    let as_pid = akuma_exec::process::read_current_pid().unwrap_or(proc.pid);
    let results = akuma_exec::process::munmap_lazy_regions_in_range(as_pid, addr, unmap_len);
    if !results.is_empty() {
        for &(freed_start, freed_pages) in &results {
            for i in 0..freed_pages {
                if let Some(frame) = proc.address_space.unmap_and_free_page(freed_start + i * 4096) {
                    crate::pmm::free_page(frame);
                }
            }
            proc.memory.free_regions.push((freed_start, freed_pages * 4096));
        }
        return 0;
    }

    let total_pages = unmap_len / 4096;
    for i in 0..total_pages {
        let va = addr + i * 4096;
        let in_eager = proc.mmap_regions.iter().any(|(start, frames)| {
            va >= *start && va < *start + frames.len() * 4096
        });
        if !in_eager {
            if let Some(frame) = proc.address_space.unmap_and_free_page(va) {
                crate::pmm::free_page(frame);
            }
        }
    }
    0
}
