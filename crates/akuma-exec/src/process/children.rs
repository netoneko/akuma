use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::process::Process;
use crate::process::types::{Pid, ProcessInfo, PROCESS_INFO_ADDR, LazyRegion, LazySource, ProcessInfo2, ProcessState};
use crate::process::channel::{ProcessChannel, get_channel};
use crate::process::table::{LAZY_REGION_TABLE, THREAD_PID_MAP};
use crate::runtime::{with_irqs_disabled, runtime, PhysFrame};
use akuma_terminal as terminal;

/// Registry mapping child PIDs to (ProcessChannel, parent_pid)
/// Used by parent processes to read child stdout via ChildStdout FD
/// and by wait4(-1) to find children of a specific parent.
static CHILD_CHANNELS: Spinlock<BTreeMap<Pid, (Arc<ProcessChannel>, Pid)>> =
    Spinlock::new(BTreeMap::new());

/// Register a child process channel (called when spawning via syscall)
pub fn register_child_channel(child_pid: Pid, channel: Arc<ProcessChannel>, parent_pid: Pid) {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().insert(child_pid, (channel, parent_pid));
    })
}

/// Get a child process channel by PID
pub fn get_child_channel(child_pid: Pid) -> Option<Arc<ProcessChannel>> {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().get(&child_pid).map(|(ch, _)| ch.clone())
    })
}

/// Remove a child process channel (called when child exits or parent closes FD)
pub fn remove_child_channel(child_pid: Pid) -> Option<Arc<ProcessChannel>> {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().remove(&child_pid).map(|(ch, _)| ch)
    })
}

/// Find any exited child of the given parent. Returns (child_pid, channel).
pub fn find_exited_child(parent_pid: Pid) -> Option<(Pid, Arc<ProcessChannel>)> {
    with_irqs_disabled(|| {
        let channels = CHILD_CHANNELS.lock();
        for (&child_pid, (ch, ppid)) in channels.iter() {
            if *ppid == parent_pid && ch.has_exited() {
                return Some((child_pid, ch.clone()));
            }
        }
        None
    })
}

/// Register `poller_tid` as a poller on every child channel of `parent_pid`.
/// When any child exits, `set_exited()` wakes the poller.
pub fn add_poller_to_all_children(parent_pid: Pid, poller_tid: usize) {
    with_irqs_disabled(|| {
        let channels = CHILD_CHANNELS.lock();
        for (ch, ppid) in channels.values() {
            if *ppid == parent_pid {
                ch.add_poller(poller_tid);
            }
        }
    })
}

/// Check if the given parent has any children registered.
pub fn has_children(parent_pid: Pid) -> bool {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().values().any(|(_, ppid)| *ppid == parent_pid)
    })
}

/// Get channel for the current thread (used by syscall handlers)
pub fn current_channel() -> Option<Arc<ProcessChannel>> {
    if let Some(proc) = current_process() {
        if let Some(ref ch) = proc.channel {
            return Some(ch.clone());
        }
    }
    
    // Fallback to thread-ID based lookup for legacy system threads
    let thread_id = crate::threading::current_thread_id();
    get_channel(thread_id)
}

/// Check if the current process has been interrupted (Ctrl+C)
///
/// Called by syscall handlers to detect interrupt signal.
/// Returns true if the process should terminate.
pub fn is_current_interrupted() -> bool {
    current_channel()
        .map(|ch| ch.is_interrupted())
        .unwrap_or(false)
}

/// Interrupt a process by thread ID
///
/// Used by the SSH shell to send Ctrl+C signal to a running process.
pub fn interrupt_thread(thread_id: usize) {
    if let Some(channel) = get_channel(thread_id) {
        channel.set_interrupted();
    }
}

/// Read the current process PID from the process info page
///
/// During a syscall, TTBR0 is still set to the user's page tables,
/// so reading from PROCESS_INFO_ADDR gives us the calling process's PID.
/// This prevents PID spoofing since the page is read-only for userspace.
///
/// Returns None if TTBR0 points to boot page tables (no user process context).
pub fn read_current_pid() -> Option<Pid> {
    // CRITICAL: Check TTBR0 before reading from user address space!
    //
    // PROCESS_INFO_ADDR (0x1000) is only mapped in USER page tables.
    // With boot TTBR0, address 0x1000 is in the device memory region (0x0-0x40000000)
    // and reading from it returns garbage, causing FAR=0x5 crashes.
    let ttbr0: u64;
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0);
    }
    #[cfg(not(target_os = "none"))]
    { ttbr0 = 0; }
    
    // Compare against actual boot TTBR0, not a range check.
    // User page tables are allocated from the same physical memory pool,
    // so they can have addresses in the same range as boot tables.
    let boot_ttbr0 = crate::mmu::get_boot_ttbr0();
    let ttbr0_addr = ttbr0 & 0x0000_FFFF_FFFF_FFFF; // Mask off ASID bits
    if ttbr0_addr == boot_ttbr0 {
        return None; // Boot TTBR0 - no user process context
    }
    
    // Read from the fixed address in the current address space
    // SAFETY: TTBR0 is user page tables, so PROCESS_INFO_ADDR is mapped
    let pid = unsafe { (*(PROCESS_INFO_ADDR as *const ProcessInfo)).pid };
    if pid == 0 { None } else { Some(pid) }
}

/// Look up a process by PID.
///
/// # Safety warning
/// Returns `&'static mut Process` that is ONLY valid while the process stays
/// registered. If another thread calls `unregister_process` between this call
/// and your use of the reference, you get use-after-free.
///
/// **Prefer `crate::process::table::with_process(pid, |p| ...)` for safe access.**
///
/// This function exists for the 218+ legacy call sites in syscall handlers.
/// Most are safe in practice because syscall handlers run in a single thread
/// context and the process can't be freed during a syscall by its own thread.
pub fn lookup_process(pid: Pid) -> Option<&'static mut Process> {
    let ptr = crate::process::table::get_process_ptr(pid)?;
    crate::process::diag::borrow_inc(pid);
    Some(unsafe { &mut *ptr })
}

/// Get the current process (for syscall handlers).
///
/// For CLONE_THREAD children, uses the thread-to-PID map since they share
/// the parent's ProcessInfo page. Otherwise reads PID from the process info page.
///
/// Same safety caveats as `lookup_process`. Prefer `with_process` for new code.
pub fn current_process() -> Option<&'static mut Process> {
    let tid = crate::threading::current_thread_id();
    let thread_pid = with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().get(&tid).copied()
    });
    if let Some(pid) = thread_pid {
        return lookup_process(pid);
    }
    let pid = read_current_pid()?;
    lookup_process(pid)

}

/// Resolve the current process PID (checking THREAD_PID_MAP first, then ProcessInfo page).
pub fn current_pid() -> Option<Pid> {
    let tid = crate::threading::current_thread_id();
    let thread_pid = with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().get(&tid).copied()
    });
    if thread_pid.is_some() { return thread_pid; }
    read_current_pid()
}

/// Get the current process's TerminalState (for syscall handlers)
///
/// Returns a mutable reference to the TerminalState if found.
pub fn current_terminal_state() -> Option<Arc<Spinlock<terminal::TerminalState>>> {
    // 1. Try thread-ID based lookup (for system threads or overridden processes)
    let tid = crate::threading::current_thread_id();
    if let Some(state) = crate::process::channel::get_terminal_state(tid) {
        return Some(state);
    }

    // 2. Fallback to process table
    current_process().map(|p| p.terminal_state.clone())
}

/// Allocate mmap region for current process
/// Returns the address or 0 on failure
pub fn alloc_mmap(size: usize) -> usize {
    // Use address-space owner so CLONE_VM threads share allocation state.
    let pid = read_current_pid().unwrap_or(0);
    let proc = match lookup_process(pid) {
        Some(p) => p,
        None => {
            (runtime().print_str)("[mmap] ERROR: No current process\n");
            return 0;
        }
    };

    // Use per-process memory tracking
    match proc.memory.alloc_mmap(size) {
        Some(addr) => addr,
        None => {
            log::debug!("[mmap] REJECT: pid={} size=0x{:x} next=0x{:x} limit=0x{:x}",
                proc.pid, size, proc.memory.next_mmap.load(core::sync::atomic::Ordering::Relaxed), proc.memory.mmap_limit);
            0
        }
    }
}

/// Record a new mmap region for the current process
///
/// Called by sys_mmap after allocating frames.
/// The frames Vec should contain all physical frames for this region.
pub fn record_mmap_region(start_va: usize, frames: Vec<PhysFrame>) {
    let pid = read_current_pid().unwrap_or(0);
    if let Some(proc) = lookup_process(pid) {
        proc.mmap_regions.push((start_va, frames));
    }
}

/// Record a lazy mmap region — VA reserved, no physical pages.
/// `page_flags` = 0 for PROT_NONE (needs mprotect), non-zero for demand-paged.
pub fn record_lazy_region(start_va: usize, size: usize, page_flags: u64) {
    let pid = read_current_pid().unwrap_or(0);
    if let Some(proc) = lookup_process(pid) {
        proc.lazy_regions.push(LazyRegion { start_va, size, flags: page_flags, source: LazySource::Zero });
    }
}

/// Check if a virtual address falls within any lazy region of the current process.
/// Returns `(flags, source, region_start, region_size)` if found.
/// The source is cloned so the caller can release the table lock before performing I/O.
pub fn lazy_region_lookup(va: usize) -> Option<(u64, LazySource, usize, usize)> {
    let pid = read_current_pid()?;
    with_irqs_disabled(|| {
        let table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get(&pid) {
            // O(log n): last region whose start_va <= va, then range-check.
            if let Some((_key, r)) = regions.range(..=va).next_back() {
                if va < r.start_va + r.size {
                    return Some((r.flags, r.source.clone(), r.start_va, r.size));
                }
            }
        }
        None
    })
}

/// Like lazy_region_lookup but takes an explicit PID (for tests and non-current-process use).
pub fn lazy_region_count_for_pid(pid: Pid) -> usize {
    with_irqs_disabled(|| {
        let table = LAZY_REGION_TABLE.lock();
        table.get(&pid).map_or(0, |r| r.len())
    })
}

pub fn lazy_region_lookup_for_pid(pid: Pid, va: usize) -> Option<(u64, LazySource, usize, usize)> {
    with_irqs_disabled(|| {
        let table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get(&pid) {
            // O(log n): find the last region whose start_va <= va, then range-check.
            if let Some((_key, r)) = regions.range(..=va).next_back() {
                if va < r.start_va + r.size {
                    return Some((r.flags, r.source.clone(), r.start_va, r.size));
                }
            }
        }
        None
    })
}

/// Thread group leader PID for page-fault / CoW paths: all `CLONE_VM` threads in a group must
/// share one [`Process::fault_mutex`] and match [`LAZY_REGION_TABLE`] (see `clone_lazy_regions`,
/// forktest / GO_FORKTEST_DEBUG). Prefer `current_process().tgid`, fall back to ProcessInfo PID.
#[inline]
pub fn address_space_owner_pid_for_fault() -> Option<Pid> {
    current_process().map(|p| p.tgid).or_else(read_current_pid)
}

/// Like [`lazy_region_lookup_for_pid`], but if `pid` misses, retries with the thread-group
/// leader from [`address_space_owner_pid_for_fault`] when it differs (demand-paging / EL0 faults).
pub fn lazy_region_lookup_for_page_fault(pid: Pid, va: usize) -> Option<(u64, LazySource, usize, usize)> {
    if let Some(r) = lazy_region_lookup_for_pid(pid, va) {
        return Some(r);
    }
    if let Some(owner) = address_space_owner_pid_for_fault() {
        if owner != pid {
            return lazy_region_lookup_for_pid(owner, va);
        }
    }
    None
}

/// Stack-local writer for visible kernel output without heap allocation.
struct LazyDebugWriter<const N: usize> {
    buf: [u8; N],
    pos: usize,
}
impl<const N: usize> LazyDebugWriter<N> {
    const fn new() -> Self { Self { buf: [0; N], pos: 0 } }
    fn flush(&mut self) {
        if let Ok(s) = core::str::from_utf8(&self.buf[..self.pos]) {
            (runtime().print_str)(s);
        }
        self.pos = 0;
    }
}
impl<const N: usize> core::fmt::Write for LazyDebugWriter<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = N - self.pos;
        let len = core::cmp::min(bytes.len(), remaining);
        self.buf[self.pos..self.pos + len].copy_from_slice(&bytes[..len]);
        self.pos += len;
        Ok(())
    }
}

pub fn lazy_region_debug(va: usize) {
    let pid = read_current_pid().unwrap_or(0);
    with_irqs_disabled(|| {
        use core::fmt::Write;
        let table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get(&pid) {
            let mut w = LazyDebugWriter::<256>::new();
            let _ = write!(w, "[DP] lazy miss: pid={} va={:#x} regions={} [", pid, va, regions.len());
            for (i, (_, r)) in regions.iter().enumerate().take(8) {
                if i > 0 { let _ = w.write_str(","); }
                let _ = write!(w, "{:#x}+{:#x}", r.start_va, r.size);
            }
            let _ = w.write_str("]\n");
            w.flush();
        } else {
            let mut w = LazyDebugWriter::<128>::new();
            let _ = writeln!(w, "[DP] lazy miss: pid={} va={:#x} no entry in table", pid, va);
            w.flush();
        }
    });
}

pub fn push_lazy_region(pid: Pid, start_va: usize, size: usize, page_flags: u64) -> usize {
    push_lazy_region_with_source(pid, start_va, size, page_flags, LazySource::Zero)
}

pub fn push_lazy_region_with_source(pid: Pid, start_va: usize, size: usize, page_flags: u64, source: LazySource) -> usize {
    let len = with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        let regions = table.entry(pid).or_insert_with(alloc::collections::BTreeMap::new);
        regions.insert(start_va, LazyRegion { start_va, size, flags: page_flags, source });
        regions.len()
    });
    len
}

/// Update flags on all lazy regions that overlap [range_start, range_start+range_size).
pub fn update_lazy_region_flags(pid: Pid, range_start: usize, range_size: usize, new_flags: u64) {
    let range_end = range_start + range_size;
    with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get_mut(&pid) {
            // Collect keys of regions that overlap [range_start, range_end).
            // Any overlapping region must have start_va < range_end AND start_va + size > range_start.
            let keys: alloc::vec::Vec<usize> = regions
                .range(..range_end)
                .filter(|x| *x.0 + x.1.size > range_start)
                .map(|x| *x.0)
                .collect();

            for key in keys {
                let r_start = key;
                let r_size = regions[&key].size;
                let r_end = r_start + r_size;
                let r_flags = regions[&key].flags;
                let r_source = regions[&key].source.clone();

                let clip_start = r_start.max(range_start);
                let clip_end = r_end.min(range_end);

                if clip_start == r_start && clip_end == r_end {
                    // Fully contained: update in place.
                    regions.get_mut(&key).unwrap().flags = new_flags;
                } else {
                    // Partially overlapping: remove and re-insert up to 3 pieces.
                    regions.remove(&key);
                    // "before" tail keeps old flags.
                    if clip_start > r_start {
                        regions.insert(r_start, LazyRegion {
                            start_va: r_start,
                            size: clip_start - r_start,
                            flags: r_flags,
                            source: r_source.clone(),
                        });
                    }
                    // Overlapping slice gets new flags.
                    regions.insert(clip_start, LazyRegion {
                        start_va: clip_start,
                        size: clip_end - clip_start,
                        flags: new_flags,
                        source: r_source.clone(),
                    });
                    // "after" tail keeps old flags.
                    if clip_end < r_end {
                        regions.insert(clip_end, LazyRegion {
                            start_va: clip_end,
                            size: r_end - clip_end,
                            flags: r_flags,
                            source: r_source,
                        });
                    }
                }
            }
        }
    });
}

pub fn remove_lazy_region(pid: Pid, start_va: usize) -> Option<LazyRegion> {
    with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get_mut(&pid) {
            regions.remove(&start_va)
        } else {
            None
        }
    })
}

/// Handle munmap across all lazy regions overlapping [unmap_addr, unmap_addr+unmap_len).
pub fn munmap_lazy_regions_in_range(pid: Pid, unmap_addr: usize, unmap_len: usize) -> Vec<(usize, usize)> {
    let unmap_end = unmap_addr + unmap_len;
    let mut results = Vec::new();

    loop {
        if let Some(result) = munmap_lazy_region_overlapping(pid, unmap_addr, unmap_end) {
            results.push(result);
        } else {
            break;
        }
    }
    results
}

fn munmap_lazy_region_overlapping(pid: Pid, range_start: usize, range_end: usize) -> Option<(usize, usize)> {
    let result = with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        let regions = table.get_mut(&pid)?;

        // Find the first region overlapping [range_start, range_end).
        // A region overlaps if start_va < range_end AND start_va + size > range_start.
        let key = regions
            .range(..range_end)
            .filter(|x| *x.0 + x.1.size > range_start)
            .map(|x| *x.0)
            .next()?;

        let reg_start = key;
        let reg_size = regions[&key].size;
        let reg_end = reg_start + reg_size;
        let reg_flags = regions[&key].flags;
        let reg_source = regions[&key].source.clone();

        let clip_start = range_start.max(reg_start);
        let clip_end = range_end.min(reg_end);

        if clip_start == reg_start && clip_end == reg_end {
            regions.remove(&key);
            Some(('F', reg_start, reg_size / 4096))
        } else if clip_start == reg_start {
            // Trim prefix: remove old entry, insert remainder at new start_va.
            regions.remove(&key);
            regions.insert(clip_end, LazyRegion {
                start_va: clip_end,
                size: reg_end - clip_end,
                flags: reg_flags,
                source: reg_source,
            });
            let freed = (clip_end - clip_start) / 4096;
            Some(('P', clip_start, freed))
        } else if clip_end == reg_end {
            // Trim suffix: shorten the existing entry in place (key unchanged).
            regions.get_mut(&key).unwrap().size = clip_start - reg_start;
            let freed = (reg_end - clip_start) / 4096;
            Some(('S', clip_start, freed))
        } else {
            // Middle split: shorten left piece, insert right piece.
            regions.get_mut(&key).unwrap().size = clip_start - reg_start;
            regions.insert(clip_end, LazyRegion {
                start_va: clip_end,
                size: reg_end - clip_end,
                flags: reg_flags,
                source: reg_source,
            });
            let freed = (clip_end - clip_start) / 4096;
            Some(('M', clip_start, freed))
        }
    });

    if let Some((op, freed_start, freed_pages)) = result {
        log::debug!("[LR{}] pid={} munmap {:#x}+{:#x} ({} pages)",
            op as char, pid, freed_start, freed_pages * 4096, freed_pages);
        Some((freed_start, freed_pages))
    } else {
        None
    }
}

pub fn clear_lazy_regions(pid: Pid) {
    let count = with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        let count = table.get(&pid).map_or(0, |r| r.len());
        table.remove(&pid);
        count
    });
    if count > 0 {
        log::debug!("[LR!] clear pid={} ({} regions)", pid, count);
    }
}

pub fn clone_lazy_regions(from_pid: Pid, to_pid: Pid) {
    with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get(&from_pid) {
            let cloned = regions.clone();
            let len = cloned.len();
            table.insert(to_pid, cloned);
            log::debug!("[LR] clone pid={}->{} ({} regions)", from_pid, to_pid, len);
        }
    });
}

/// Check if a virtual address falls within any lazy region.
pub fn is_in_lazy_region(va: usize) -> bool {
    lazy_region_lookup(va).is_some()
}

/// Remove and return mmap region starting at the given VA
pub fn remove_mmap_region(start_va: usize) -> Option<Vec<PhysFrame>> {
    let pid = read_current_pid().unwrap_or(0);
    let proc = lookup_process(pid)?;
    
    // Find the region
    let idx = proc.mmap_regions.iter().position(|(va, _)| *va == start_va)?;
    
    // Remove and return the frames
    let (va, frames) = proc.mmap_regions.remove(idx);
    
    // RECLAIM: Add the freed range to free_regions
    let size: usize = frames.len() * 4096; // config::PAGE_SIZE
    proc.memory.free_regions.push((va, size));
    
    Some(frames)
}

/// Get stack bounds for current process
pub fn get_stack_bounds() -> (usize, usize) {
    match current_process() {
        Some(p) => (p.memory.stack_bottom, p.memory.stack_top),
        None => (0, 0),
    }
}


/// List all running processes.
///
/// Collects scalar fields with IRQs disabled (safe from use-after-free),
/// then does a second pass to clone Strings per PID.
/// The String clone uses lookup_process which re-validates the pointer.
pub fn list_processes() -> Vec<ProcessInfo2> {
    // Phase 1: collect scalar fields atomically (IRQs disabled, no allocation)
    #[derive(Copy, Clone, Default)]
    struct Info {
        pid: u32,
        ppid: u32,
        box_id: u64,
        state: u8, // 0=ready 1=running 2=blocked 3=zombie
        current_syscall: u64,
        last_syscall: u64,
    }
    let infos = crate::process::table::collect_process_info(|p| {
        let st = match p.state {
            ProcessState::Ready => 0u8,
            ProcessState::Running => 1,
            ProcessState::Blocked => 2,
            ProcessState::Zombie(_) => 3,
        };
        Some(Info {
            pid: p.pid,
            ppid: p.parent_pid,
            box_id: p.box_id,
            state: st,
            current_syscall: p.current_syscall.load(core::sync::atomic::Ordering::Relaxed),
            last_syscall: p.last_syscall.load(core::sync::atomic::Ordering::Relaxed),
        })
    });

    // Phase 2: clone Strings per PID (IRQs enabled, safe to allocate).
    // lookup_process re-validates the pointer; if the process was freed
    // between phase 1 and 2, lookup returns None and we use fallback values.
    let mut result = Vec::with_capacity(infos.len());
    for info in &infos {
        let state_str = match info.state {
            0 => "ready", 1 => "running", 2 => "blocked", _ => "zombie",
        };
        let (name, args) = if let Some(proc) = lookup_process(info.pid) {
            if proc.name.len() <= 4096 && proc.args.len() <= 256 {
                (proc.name.clone(), proc.args.clone())
            } else {
                (alloc::string::String::from("?"), Vec::new())
            }
        } else {
            (alloc::string::String::from("?"), Vec::new())
        };
        result.push(ProcessInfo2 {
            pid: info.pid,
            ppid: info.ppid,
            box_id: info.box_id,
            name,
            state: state_str,
            current_syscall: info.current_syscall,
            last_syscall: info.last_syscall,
            args,
        });
    }
    result
}

/// Find a process PID by thread ID (lock-free scan).
pub fn find_pid_by_thread(thread_id: usize) -> Option<Pid> {
    crate::process::table::find_process(|p| {
        if p.thread_id == Some(thread_id) { Some(p.pid) } else { None }
    })
}
