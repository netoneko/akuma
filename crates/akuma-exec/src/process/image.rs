use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::format;
use core::sync::atomic::Ordering;

use akuma_terminal as terminal;
use spinning_top::Spinlock;

use crate::elf_loader::{self, DeferredLazySegment, ElfError, LoadedWithStack};
use crate::mmu;
use crate::runtime::{runtime, config, track_frame, FrameSource, PhysFrame};
use crate::process::types::{ProcessMemory, LazySource, SignalHandler, SignalAction, PROCESS_INFO_ADDR, ProcessInfo, Pid};
use crate::process::lifecycle::LifecycleGuard;
use super::{
    LazyRegionMap, NEXT_PID, Process, ProcessState, ProcessSyscallStats, SharedFdTable,
    SharedSignalTable, StdioBuffer, UserContext,
};

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

    let (_, _, free) = akuma_pmm::stats();
    let phys_cap = free.saturating_sub(RESERVE_PAGES) * crate::mmu::PAGE_SIZE;
    let va_cap = memory.next_mmap.load(core::sync::atomic::Ordering::Relaxed).saturating_sub(brk);

    core::cmp::max(core::cmp::min(phys_cap, va_cap), MIN_HEAP)
}

/// Where an image being executed comes from.
///
/// These are the two combinations `execve` picks between by file size
/// (`HEAP_SLURP_MAX`, `process/spawn.rs`): a small binary is slurped into the
/// kernel heap and mapped eagerly, a large one stays on disk and is demand-paged.
/// Passing the choice as a value is what lets one implementation serve both
/// `Process` constructors and both image replacers — each of the four used to be
/// copy-pasted per source.
enum ImageSource<'a> {
    /// An image already resident in the kernel heap.
    Bytes(&'a [u8]),
    /// An image left on disk, read a piece at a time. `file_size` is only ever
    /// reported in the loader's debug log; the read length comes from the phdrs.
    Path { path: &'a str, file_size: usize },
}

impl ImageSource<'_> {
    /// Load the image and build its initial user stack.
    fn load(
        &self,
        args: &[String],
        env: &[String],
        interp_prefix: Option<&str>,
    ) -> Result<LoadedWithStack, ElfError> {
        let stack_size = config().user_stack_size;
        match *self {
            Self::Bytes(data) => {
                elf_loader::load_elf_with_stack(data, args, env, stack_size, interp_prefix)
            }
            Self::Path { path, file_size } => elf_loader::load_elf_with_stack_from_path(
                path, file_size, args, env, stack_size, interp_prefix,
            ),
        }
    }
}

/// Register the loader's demand-paged segments as lazy regions.
///
/// `segments` is empty for an eagerly-mapped image (`LoadedWithStack::
/// deferred_segments` is a property of the mapping strategy, not of the caller),
/// so this is a no-op on that path rather than a branch either caller has to make.
fn push_deferred_regions(lazy: &mut LazyRegionMap, segments: &[DeferredLazySegment]) {
    for seg in segments {
        let source = match &seg.file_source {
            Some(fs) => LazySource::file(
                fs.path.clone(),
                fs.mount_id,
                fs.inode,
                fs.file_offset,
                fs.filesz,
                fs.segment_va,
            ),
            None => LazySource::Zero,
        };
        lazy.push(seg.start_va, seg.size, seg.page_flags, source);
    }
}

impl Process {
    /// Replace current process image with a new ELF binary (execve core)
    pub fn replace_image(&mut self, elf_data: &[u8], args: &[String], env: &[String]) -> Result<(), String> {
        self.replace_image_from(ImageSource::Bytes(elf_data), args, env)
    }

    /// Replace current process image using on-demand loading from a file path.
    pub fn replace_image_from_path(&mut self, path: &str, file_size: usize, args: &[String], env: &[String]) -> Result<(), String> {
        self.replace_image_from(ImageSource::Path { path, file_size }, args, env)
    }

    /// The execve core, shared by both public replacers.
    ///
    /// The `[FORK-DBG]` lifecycle traces below existed only on the in-memory path
    /// until these two were merged, so whichever half of exec a binary's size
    /// selected was invisible to lifecycle tracing. They now cover both sources.
    fn replace_image_from(&mut self, source: ImageSource<'_>, args: &[String], env: &[String]) -> Result<(), String> {
        crate::process::lifecycle_trace("[FORK-DBG] replace_image: loading ELF\n");
        let interp_prefix: Option<&str> = None;
        let loaded = source.load(args, env, interp_prefix)
            .map_err(|e| format!("Failed to load ELF: {}", e))?;
        crate::process::lifecycle_trace("[FORK-DBG] replace_image: ELF loaded, deactivating old AS\n");

        kill_exec_siblings(self.pid, self.tgid);

        // Serialize the DESTRUCTIVE window against preemption under shared-kernel SMP —
        // from the AS deactivate/swap and `mmap_regions.clear()` onward the process is
        // half-built and must not be observable mid-flight (runbook hypothesis 1, the
        // SMP=4 heterogeneous SIGSEGVs). Acquired AFTER the ELF load above: the load
        // allocates/copies for milliseconds and, for an `ImageSource::Path`, does block
        // I/O — holding the preemption-disable guard across such waits wedges the box
        // (see `process/lifecycle.rs` and the spawn.rs load-phase note).
        let _lifecycle = LifecycleGuard::acquire();

        crate::process::lifecycle_trace("[FORK-DBG] replace_image: deactivating\n");
        mmu::as_trace(format_args!(
            "[AS-EXEC] pid={} old_l0=0x{:x} old_asid=0x{:x} new_l0=0x{:x} new_asid=0x{:x} core={}\n",
            self.pid, self.address_space.l0_phys(), self.address_space.asid(),
            loaded.address_space.l0_phys(), loaded.address_space.asid(), crate::bkl::current_core_id()));
        mmu::UserAddressSpace::deactivate();
        crate::process::lifecycle_trace("[FORK-DBG] replace_image: swapping AS\n");
        self.address_space = loaded.address_space;
        crate::process::lifecycle_trace("[FORK-DBG] replace_image: AS swapped\n");
        self.entry_point = loaded.entry_point;
        self.brk = loaded.brk;
        self.initial_brk = loaded.brk;
        self.memory = ProcessMemory::new(loaded.brk, loaded.stack_bottom, loaded.stack_top, loaded.mmap_floor);
        self.mmap_regions.clear();
        self.lazy_regions.lock().clear();
        self.dynamic_page_tables.clear();
        self.args = args.to_vec();
        self.clear_child_tid = 0;

        // Written through the owned field rather than the pid-keyed
        // `push_lazy_region`: that would resolve `self.pid` back to this very
        // `Process` through a *shared* table lookup while we hold `&mut self`.
        let heap_lazy_size = compute_heap_lazy_size(loaded.brk, &self.memory);
        let lazy_stack_start = loaded.stack_top.saturating_sub(LAZY_STACK_MAX);
        {
            let mut lazy = self.lazy_regions.lock();
            // Demand-paged segments first, then heap and stack — the order the
            // on-demand path has always used.
            push_deferred_regions(&mut lazy, &loaded.deferred_segments);
            lazy.push(loaded.brk, heap_lazy_size, crate::mmu::user_flags::RW_NO_EXEC, LazySource::Zero);
            lazy.push(lazy_stack_start, LAZY_STACK_MAX, crate::mmu::user_flags::RW_NO_EXEC, LazySource::Zero);
        }

        if config().syscall_debug_info_enabled {
            let how = if loaded.deferred_segments.is_empty() { "" } else { " (on-demand)" };
            log::debug!("[Process] PID {} replaced{}: entry=0x{:x}, brk=0x{:x}, stack=0x{:x}-0x{:x}, sp=0x{:x}",
                self.pid, how, loaded.entry_point, loaded.brk, loaded.stack_bottom, loaded.stack_top, loaded.sp);
        }

        // Update context for the next run
        self.context = crate::process::UserContext::new(loaded.entry_point, loaded.sp);

        // Re-write process info page in the NEW address space
        let process_info_frame = akuma_pmm::alloc_page_zeroed().map(PhysFrame::new).ok_or("OOM process info")?;
        track_frame(process_info_frame, FrameSource::UserData);

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

    /// Create a new process from ELF data
    pub fn from_elf(name: &str, args: &[String], env: &[String], elf_data: &[u8], interp_prefix: Option<&str>) -> Result<Self, ElfError> {
        Self::from_image(name, ImageSource::Bytes(elf_data), args, env, interp_prefix)
    }

    /// Create a process from a large ELF file on disk, loading segments on demand.
    pub fn from_elf_path(name: &str, path: &str, file_size: usize, args: &[String], env: &[String], interp_prefix: Option<&str>) -> Result<Self, ElfError> {
        Self::from_image(name, ImageSource::Path { path, file_size }, args, env, interp_prefix)
    }

    /// The constructor both public entry points run.
    fn from_image(name: &str, source: ImageSource<'_>, args: &[String], env: &[String], interp_prefix: Option<&str>) -> Result<Self, ElfError> {
        {
            // Reported for both sources, not just the on-demand one: the eager
            // source is the one that has already slurped the whole file into this
            // heap, so it is the more interesting of the two to see a figure for.
            let (allocated, heap_size) = (runtime().heap_stats)();
            log::debug!("[Process] heap before ELF load: {}MB / {}MB ({}%)",
                allocated / 1024 / 1024, heap_size / 1024 / 1024,
                (allocated * 100).checked_div(heap_size).unwrap_or(0));
        }
        let mut loaded = source.load(args, env, interp_prefix)?;

        let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);

        // Register demand-paged regions: the image's own lazy segments (empty
        // unless it was loaded on demand), then heap and stack growth.
        //
        // Built as a local and moved into the struct below. The pid-keyed
        // `push_lazy_region` cannot be used here: it resolves `pid` through the
        // process table, and this `Process` does not exist yet — let alone is
        // registered — so every region would be silently dropped and the first
        // heap or deep-stack touch would SIGSEGV.
        let mut lazy_regions = LazyRegionMap::new();
        push_deferred_regions(&mut lazy_regions, &loaded.deferred_segments);

        let process_info_frame = akuma_pmm::alloc_page_zeroed().map(PhysFrame::new).ok_or(ElfError::OutOfMemory)?;
        track_frame(process_info_frame, FrameSource::UserData);

        loaded.address_space
            .map_page(
                PROCESS_INFO_ADDR,
                process_info_frame.addr,
                crate::mmu::user_flags::RO | crate::mmu::flags::UXN | crate::mmu::flags::PXN,
            )
            .map_err(|_| ElfError::MappingFailed("process info page"))?;

        loaded.address_space.track_user_frame(process_info_frame);

        let memory = ProcessMemory::new(loaded.brk, loaded.stack_bottom, loaded.stack_top, loaded.mmap_floor);

        log::debug!("[Process] PID {} memory: code_end=0x{:x}, stack=0x{:x}-0x{:x}, mmap=0x{:x}-0x{:x}",
            pid, loaded.brk, loaded.stack_bottom, loaded.stack_top, memory.next_mmap.load(Ordering::Relaxed), memory.mmap_limit);

        let heap_lazy_size = compute_heap_lazy_size(loaded.brk, &memory);
        lazy_regions.push(loaded.brk, heap_lazy_size, crate::mmu::user_flags::RW_NO_EXEC, LazySource::Zero);
        let lazy_stack_start = loaded.stack_top.saturating_sub(LAZY_STACK_MAX);
        lazy_regions.push(lazy_stack_start, LAZY_STACK_MAX, crate::mmu::user_flags::RW_NO_EXEC, LazySource::Zero);

        Ok(Self {
            pid,
            pgid: pid,
            tgid: pid, // group leader = self
            name: String::from(name),
            state: ProcessState::Ready,
            address_space: loaded.address_space,
            context: UserContext::new(loaded.entry_point, loaded.sp),
            parent_pid: 0,
            brk: loaded.brk,
            initial_brk: loaded.brk,
            entry_point: loaded.entry_point,
            memory,
            process_info_phys: process_info_frame.addr,
            args: Vec::new(),
            cwd: String::from("/"),
            stdin: Arc::new(Spinlock::new(StdioBuffer::new())),
            stdout: Arc::new(Spinlock::new(StdioBuffer::new())),
            exited: false,
            exit_code: 0,
            dynamic_page_tables: Vec::new(),
            mmap_regions: Vec::new(),
            lazy_regions: Spinlock::new(lazy_regions),
            fds: Arc::new(SharedFdTable::with_stdio()),
            thread_id: None,
            // Spawner PID - set when spawned by another process
            spawner_pid: None,
            // Terminal State - default for new processes
            terminal_state: Arc::new(Spinlock::new(terminal::TerminalState::default())),

            box_id: 0,
            namespace: akuma_isolation::global_namespace(),
            channel: None,
            delegate_pid: None,
            grabbed_by: None,
            clear_child_tid: 0,
            robust_list_head: 0,
            robust_list_len: 0,
            signal_actions: Arc::new(SharedSignalTable::new()),
            signal_mask: 0,
            fault_mutex: Spinlock::new(BTreeMap::new()),
            vm_lock: Spinlock::new(()),
            as_lock: Spinlock::new(()),
            sigaltstack_sp: 0,
            sigaltstack_flags: 2, // SS_DISABLE
            sigaltstack_size: 0,
            start_time_us: (runtime().uptime_us)(),
            current_syscall: core::sync::atomic::AtomicU64::new(!0),
            last_syscall: core::sync::atomic::AtomicU64::new(0),
            syscall_stats: ProcessSyscallStats::new(),
        })
    }
}
