use alloc::string::String;
use alloc::format;

use crate::mmu;
use crate::runtime::{runtime, config, FrameSource};
use crate::process::types::{ProcessMemory, LazySource, SignalHandler, SignalAction, PROCESS_INFO_ADDR, ProcessInfo, Pid};
use crate::process::lifecycle::LifecycleGuard;
use super::Process;

/// True if some OTHER process shares `tgid` — a live CLONE_THREAD sibling of `pid`.
fn has_thread_group_siblings(pid: Pid, tgid: Pid) -> bool {
    let mut found = false;
    crate::process::table::for_each_process(|p| {
        found |= p.pid != pid && p.tgid == tgid;
    });
    found
}

/// POSIX execve destroys every other thread in the calling process's thread group
/// before the image is replaced. Without this, a CLONE_THREAD sibling (e.g. a
/// still-parked rayon/thread-pool worker) keeps running under the address space
/// `self.address_space` is about to be overwritten with — and dropping the old
/// `UserAddressSpace` frees (and PMM-poisons) its page-table frames while a peer
/// core's `TTBR0_EL1` can still be resident on them. See
/// docs/archive/PAGE_TABLE_UAF_BKL_STORM.md. Called after the new image has
/// loaded successfully (the point of no return for exec) and before the
/// destructive swap, matching the point close-on-exec fds are handled at.
fn kill_exec_siblings(pid: Pid, tgid: Pid) {
    if has_thread_group_siblings(pid, tgid) {
        super::kill_thread_group(pid, 0, 0);
    }
}

/// Maximum virtual address range registered for demand-paged stack growth.
/// Physical pages are only allocated on fault, so this costs nothing unless used.
/// 32 MB is enough for even the heaviest runtimes (Bun/JSC uses ~600KB–2MB).
pub(crate) const LAZY_STACK_MAX: usize = 32 * 1024 * 1024;

pub(crate) fn compute_heap_lazy_size(brk: usize, memory: &ProcessMemory) -> usize {
    const MIN_HEAP: usize = 16 * 1024 * 1024;
    const RESERVE_PAGES: usize = 2048; // 8MB

    let (_, _, free) = (runtime().pmm_stats)();
    let phys_cap = free.saturating_sub(RESERVE_PAGES) * crate::mmu::PAGE_SIZE;
    let va_cap = memory.next_mmap.load(core::sync::atomic::Ordering::Relaxed).saturating_sub(brk);

    core::cmp::max(core::cmp::min(phys_cap, va_cap), MIN_HEAP)
}

impl Process {
    /// Replace current process image with a new ELF binary (execve core)
    pub fn replace_image(&mut self, elf_data: &[u8], args: &[String], env: &[String]) -> Result<(), String> {
        crate::process::lifecycle_trace("[FORK-DBG] replace_image: loading ELF\n");
        let interp_prefix: Option<&str> = None;
        let (entry_point, mut address_space, sp, brk, stack_bottom, stack_top, mmap_floor, _deferred) =
            crate::elf_loader::load_elf_with_stack(elf_data, args, env, config().user_stack_size, interp_prefix)
            .map_err(|e| format!("Failed to load ELF: {}", e))?;
        crate::process::lifecycle_trace("[FORK-DBG] replace_image: ELF loaded, deactivating old AS\n");

        kill_exec_siblings(self.pid, self.tgid);

        // Serialize the DESTRUCTIVE window against preemption under shared-kernel SMP —
        // from the AS deactivate/swap and `mmap_regions.clear()` onward the process is
        // half-built and must not be observable mid-flight (runbook hypothesis 1, the
        // SMP=4 heterogeneous SIGSEGVs). Acquired AFTER the ELF load above: the load
        // allocates/copies for milliseconds and (in the `from_path` variant) does block
        // I/O — holding the preemption-disable guard across such waits wedges the box
        // (see `process/lifecycle.rs` and the spawn.rs load-phase note).
        let _lifecycle = LifecycleGuard::acquire();

        crate::process::lifecycle_trace("[FORK-DBG] replace_image: deactivating\n");
        mmu::UserAddressSpace::deactivate();
        crate::process::lifecycle_trace("[FORK-DBG] replace_image: swapping AS\n");
        self.address_space = address_space;
        crate::process::lifecycle_trace("[FORK-DBG] replace_image: AS swapped\n");
        self.entry_point = entry_point;
        self.brk = brk;
        self.initial_brk = brk;
        self.memory = ProcessMemory::new(brk, stack_bottom, stack_top, mmap_floor);
        self.mmap_regions.clear();
        self.lazy_regions.lock().clear();
        self.dynamic_page_tables.clear();
        self.args = args.to_vec();
        self.clear_child_tid = 0;

        // Written through the owned field rather than the pid-keyed
        // `push_lazy_region`: that would resolve `self.pid` back to this very
        // `Process` through a *shared* table lookup while we hold `&mut self`.
        let heap_lazy_size = compute_heap_lazy_size(brk, &self.memory);
        let lazy_stack_start = stack_top.saturating_sub(LAZY_STACK_MAX);
        {
            let mut lazy = self.lazy_regions.lock();
            lazy.push(brk, heap_lazy_size, crate::mmu::user_flags::RW_NO_EXEC, LazySource::Zero);
            lazy.push(lazy_stack_start, LAZY_STACK_MAX, crate::mmu::user_flags::RW_NO_EXEC, LazySource::Zero);
        }

        if config().syscall_debug_info_enabled {
            log::debug!("[Process] PID {} replaced: entry=0x{:x}, brk=0x{:x}, stack=0x{:x}-0x{:x}, sp=0x{:x}",
                self.pid, entry_point, brk, stack_bottom, stack_top, sp);
        }

        // Update context for the next run
        self.context = crate::process::UserContext::new(entry_point, sp);
        
        // Re-write process info page in the NEW address space
        let process_info_frame = (runtime().alloc_page_zeroed)().ok_or("OOM process info")?;
        (runtime().track_frame)(process_info_frame, FrameSource::UserData);
        
        self.address_space
            .map_page(
                PROCESS_INFO_ADDR,
                process_info_frame.addr,
                mmu::user_flags::RO | mmu::flags::UXN | mmu::flags::PXN,
            )
            .map_err(|_| "Failed to map process info")?;
            
        self.address_space.track_user_frame(process_info_frame);
        self.process_info_phys = process_info_frame.addr;

        unsafe {
            let info_ptr = mmu::phys_to_virt(self.process_info_phys) as *mut ProcessInfo;
            let info = ProcessInfo::new(self.pid, self.parent_pid, self.box_id);
            core::ptr::write(info_ptr, info);
        }

        // Reset I/O state (but keep FDs and Channel!)
        self.reset_io();

        // POSIX: on exec, custom signal handlers are reset to SIG_DFL; SIG_IGN is preserved.
        // Also disable the alternate signal stack — it pointed into the old address space.
        {
            let mut actions = self.signal_actions.actions.lock();
            for action in actions.iter_mut() {
                if matches!(action.handler, SignalHandler::UserFn(_)) {
                    *action = SignalAction::default();
                }
            }
        }
        self.sigaltstack_sp = 0;
        self.sigaltstack_size = 0;
        self.sigaltstack_flags = 2; // SS_DISABLE

        Ok(())
    }

    /// Replace current process image using on-demand loading from a file path.
    pub fn replace_image_from_path(&mut self, path: &str, file_size: usize, args: &[String], env: &[String]) -> Result<(), String> {
        let interp_prefix: Option<&str> = None;
        let (entry_point, mut address_space, sp, brk, stack_bottom, stack_top, mmap_floor, deferred_segments) =
            crate::elf_loader::load_elf_with_stack_from_path(path, file_size, args, env, config().user_stack_size, interp_prefix)
            .map_err(|e| format!("Failed to load ELF: {}", e))?;

        kill_exec_siblings(self.pid, self.tgid);

        // Guard the destructive window only — acquired AFTER the header read above,
        // which does block I/O. See the comment in `replace_image`.
        let _lifecycle = LifecycleGuard::acquire();

        mmu::UserAddressSpace::deactivate();

        self.address_space = address_space;
        self.entry_point = entry_point;
        self.brk = brk;
        self.initial_brk = brk;
        self.memory = ProcessMemory::new(brk, stack_bottom, stack_top, mmap_floor);
        self.mmap_regions.clear();
        self.lazy_regions.lock().clear();
        self.dynamic_page_tables.clear();
        self.args = args.to_vec();
        self.clear_child_tid = 0;

        {
            let mut lazy = self.lazy_regions.lock();
            for seg in &deferred_segments {
                let source = match &seg.file_source {
                    Some(fs) => LazySource::File {
                        path: fs.path.clone(),
                        inode: fs.inode,
                        file_offset: fs.file_offset,
                        filesz: fs.filesz,
                        segment_va: fs.segment_va,
                    },
                    None => LazySource::Zero,
                };
                lazy.push(seg.start_va, seg.size, seg.page_flags, source);
            }
        }

        // Written through the owned field rather than the pid-keyed
        // `push_lazy_region`: that would resolve `self.pid` back to this very
        // `Process` through a *shared* table lookup while we hold `&mut self`.
        let heap_lazy_size = compute_heap_lazy_size(brk, &self.memory);
        let lazy_stack_start = stack_top.saturating_sub(LAZY_STACK_MAX);
        {
            let mut lazy = self.lazy_regions.lock();
            lazy.push(brk, heap_lazy_size, crate::mmu::user_flags::RW_NO_EXEC, LazySource::Zero);
            lazy.push(lazy_stack_start, LAZY_STACK_MAX, crate::mmu::user_flags::RW_NO_EXEC, LazySource::Zero);
        }

        if config().syscall_debug_info_enabled {
            log::debug!("[Process] PID {} replaced (on-demand): entry=0x{:x}, brk=0x{:x}, stack=0x{:x}-0x{:x}, sp=0x{:x}",
                self.pid, entry_point, brk, stack_bottom, stack_top, sp);
        }

        self.context = crate::process::UserContext::new(entry_point, sp);

        let process_info_frame = (runtime().alloc_page_zeroed)().ok_or("OOM process info")?;
        (runtime().track_frame)(process_info_frame, FrameSource::UserData);

        self.address_space
            .map_page(
                PROCESS_INFO_ADDR,
                process_info_frame.addr,
                mmu::user_flags::RO | mmu::flags::UXN | mmu::flags::PXN,
            )
            .map_err(|_| "Failed to map process info")?;

        self.address_space.track_user_frame(process_info_frame);
        self.process_info_phys = process_info_frame.addr;

        unsafe {
            let info_ptr = mmu::phys_to_virt(self.process_info_phys) as *mut ProcessInfo;
            let info = ProcessInfo::new(self.pid, self.parent_pid, self.box_id);
            core::ptr::write(info_ptr, info);
        }

        self.reset_io();

        // POSIX: on exec, custom signal handlers are reset to SIG_DFL; SIG_IGN is preserved.
        // Also disable the alternate signal stack — it pointed into the old address space.
        {
            let mut actions = self.signal_actions.actions.lock();
            for action in actions.iter_mut() {
                if matches!(action.handler, SignalHandler::UserFn(_)) {
                    *action = SignalAction::default();
                }
            }
        }
        self.sigaltstack_sp = 0;
        self.sigaltstack_size = 0;
        self.sigaltstack_flags = 2; // SS_DISABLE

        Ok(())
    }
}
