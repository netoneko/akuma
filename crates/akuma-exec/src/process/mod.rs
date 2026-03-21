//! Process Management
//!
//! Manages user processes including creation, execution, and termination.

pub mod types;
pub mod table;
pub mod channel;
pub mod children;
pub mod signal;
pub mod stats;
pub mod fd;
pub mod image;
pub mod spawn;
pub mod exec;

pub use types::*;
pub use table::*;
pub use channel::*;
pub use children::*;
pub use signal::*;
pub use stats::*;
pub use fd::*;
pub use spawn::*;
pub use exec::*;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use spinning_top::Spinlock;

use crate::elf_loader::{self, ElfError};
use crate::mmu::{self, UserAddressSpace};
use crate::runtime::{PhysFrame, FrameSource, runtime, config, with_irqs_disabled};
use akuma_terminal as terminal;

use self::image::{compute_heap_lazy_size, LAZY_STACK_MAX};

/// Initialize the process subsystem
pub fn init() {
    init_box_registry(); // Init Box 0
    crate::threading::set_cleanup_callback(on_thread_cleanup);
}

/// Callback invoked by the threading subsystem when a thread slot is recycled.
fn on_thread_cleanup(tid: usize) {
    let pid_opt = with_irqs_disabled(|| {
        table::THREAD_PID_MAP.lock().remove(&tid)
    });

    if let Some(pid) = pid_opt {
        let remaining_threads = with_irqs_disabled(|| {
            let map = table::THREAD_PID_MAP.lock();
            map.values().filter(|&&p| p == pid).count()
        });

        if remaining_threads == 0 {
            if let Some(_proc) = table::unregister_process(pid) {
            }
        }
    }
}

// Box registry re-exports
pub use crate::box_registry::{
    BoxInfo, register_box, unregister_box, list_boxes,
    find_box_by_name, get_box_name, get_box_info, find_primary_box,
    init_box_registry,
};

/// Write data to a process's stdin
pub fn write_to_process_stdin(pid: Pid, data: &[u8]) -> Result<(), &'static str> {
    let proc = children::lookup_process(pid).ok_or("Process not found")?;
    
    if let Some(target_pid) = proc.delegate_pid {
        return write_to_process_stdin(target_pid, data);
    }

    proc.stdin.lock().write_with_limit(data, config().proc_stdin_max_size);
    
    if let Some(ref channel) = proc.channel {
        channel.write_stdin(data);
        
        crate::threading::disable_preemption();
        if let Some(waker) = proc.terminal_state.lock().input_waker.lock().take() {
            waker.wake();
        }
        crate::threading::enable_preemption();
    }
    Ok(())
}

/// A user process
pub struct Process {
    pub pid: Pid,
    pub pgid: Pid,
    pub name: String,
    pub state: ProcessState,
    pub address_space: UserAddressSpace,
    pub context: UserContext,
    pub parent_pid: Pid,
    pub brk: usize,
    pub initial_brk: usize,
    pub entry_point: usize,
    pub memory: ProcessMemory,
    pub process_info_phys: usize,
    pub args: Vec<String>,
    pub cwd: String,
    pub stdin: Spinlock<StdioBuffer>,
    pub stdout: Spinlock<StdioBuffer>,
    pub exited: bool,
    pub exit_code: i32,
    pub dynamic_page_tables: Vec<PhysFrame>,
    pub mmap_regions: Vec<(usize, Vec<PhysFrame>)>,
    pub lazy_regions: Vec<LazyRegion>,
    pub fds: Arc<SharedFdTable>,
    pub thread_id: Option<usize>,
    pub spawner_pid: Option<Pid>,
    pub terminal_state: Arc<Spinlock<terminal::TerminalState>>,
    pub box_id: u64,
    pub namespace: Arc<akuma_isolation::Namespace>,
    pub channel: Option<Arc<ProcessChannel>>,
    pub delegate_pid: Option<Pid>,
    pub clear_child_tid: u64,
    pub robust_list_head: u64,
    pub robust_list_len: usize,
    pub signal_actions: Arc<SharedSignalTable>,
    pub signal_mask: u64,
    pub sigaltstack_sp: u64,
    pub sigaltstack_flags: i32,
    pub sigaltstack_size: u64,
    pub start_time_us: u64,
    pub current_syscall: AtomicU64,
    pub last_syscall: AtomicU64,
    pub syscall_stats: ProcessSyscallStats,
}

/// Shared signal action table for CLONE_SIGHAND semantics.
///
/// When threads are created with CLONE_THREAD (pthreads), they share this table
/// via Arc — matching Linux CLONE_SIGHAND behavior. Fork/Spawn creates a fresh table.
/// Kill all processes in a box and unregister it
pub fn kill_box(box_id: u64) -> Result<(), &'static str> {
    if box_id == 0 {
        return Err("Cannot kill Box 0 (Host)");
    }

    // 1. Get list of PIDs in this box
    let pids: Vec<Pid> = with_irqs_disabled(|| {
        let table = PROCESS_TABLE.lock();
        table.iter()
            .filter(|(_, proc)| proc.box_id == box_id)
            .map(|(&pid, _)| pid)
            .collect()
    });

    // 2. Kill each process
    for pid in pids {
        // kill_process handles unregistering and thread termination
        let _ = kill_process(pid);
    }

    // 3. Unregister the box from the global registry
    unregister_box(box_id);

    Ok(())
}

impl Process {
    /// Create a new process from ELF data
    pub fn from_elf(name: &str, args: &[String], env: &[String], elf_data: &[u8], interp_prefix: Option<&str>) -> Result<Self, ElfError> {
        let (entry_point, mut address_space, stack_pointer, brk, stack_bottom, stack_top, mmap_floor, _deferred) =
            elf_loader::load_elf_with_stack(elf_data, args, env, config().user_stack_size, interp_prefix)?;

        let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);

        let process_info_frame = (runtime().alloc_page_zeroed)().ok_or(ElfError::OutOfMemory)?;
        (runtime().track_frame)(process_info_frame, FrameSource::UserData);

        address_space
            .map_page(
                PROCESS_INFO_ADDR,
                process_info_frame.addr,
                crate::mmu::user_flags::RO | crate::mmu::flags::UXN | crate::mmu::flags::PXN,
            )
            .map_err(|_| ElfError::MappingFailed("process info page"))?;

        address_space.track_user_frame(process_info_frame);

        let memory = ProcessMemory::new(brk, stack_bottom, stack_top, mmap_floor);

        log::debug!("[Process] PID {} memory: code_end=0x{:x}, stack=0x{:x}-0x{:x}, mmap=0x{:x}-0x{:x}",
            pid, brk, stack_bottom, stack_top, memory.next_mmap, memory.mmap_limit);

        // Register demand-paged regions for heap and stack growth.
        let heap_lazy_size = compute_heap_lazy_size(brk, &memory);
        push_lazy_region(pid, brk, heap_lazy_size, crate::mmu::user_flags::RW_NO_EXEC);
        let lazy_stack_start = stack_top.saturating_sub(LAZY_STACK_MAX);
        push_lazy_region(pid, lazy_stack_start, LAZY_STACK_MAX, crate::mmu::user_flags::RW_NO_EXEC);

        Ok(Self {
            pid,
            pgid: pid,
            name: String::from(name),
            state: ProcessState::Ready,
            address_space,
            context: UserContext::new(entry_point, stack_pointer),
            parent_pid: 0,
            brk,
            initial_brk: brk,
            entry_point,
            memory,
            process_info_phys: process_info_frame.addr,
            args: Vec::new(),
            cwd: String::from("/"),
            stdin: Spinlock::new(StdioBuffer::new()),
            stdout: Spinlock::new(StdioBuffer::new()),
            exited: false,
            exit_code: 0,
            dynamic_page_tables: Vec::new(),
            mmap_regions: Vec::new(),
            lazy_regions: Vec::new(),
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
            clear_child_tid: 0,
            robust_list_head: 0,
            robust_list_len: 0,
            signal_actions: Arc::new(SharedSignalTable::new()),
            signal_mask: 0,
            sigaltstack_sp: 0,
            sigaltstack_flags: 2, // SS_DISABLE
            sigaltstack_size: 0,
            start_time_us: (runtime().uptime_us)(),
            current_syscall: core::sync::atomic::AtomicU64::new(!0),
            last_syscall: core::sync::atomic::AtomicU64::new(0),
            syscall_stats: ProcessSyscallStats::new(),
})
    }

    /// Create a process from a large ELF file on disk, loading segments on demand.
    pub fn from_elf_path(name: &str, path: &str, file_size: usize, args: &[String], env: &[String], interp_prefix: Option<&str>) -> Result<Self, ElfError> {
        {
            let (allocated, heap_size) = (runtime().heap_stats)();
            log::debug!("[Process] heap before ELF load: {}MB / {}MB ({}%)",
                allocated / 1024 / 1024, heap_size / 1024 / 1024,
                if heap_size > 0 { allocated * 100 / heap_size } else { 0 });
        }
        let (entry_point, mut address_space, stack_pointer, brk, stack_bottom, stack_top, mmap_floor, deferred_segments) =
            elf_loader::load_elf_with_stack_from_path(path, file_size, args, env, config().user_stack_size, interp_prefix)?;

        let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);

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
            push_lazy_region_with_source(pid, seg.start_va, seg.size, seg.page_flags, source);
        }

        let process_info_frame = (runtime().alloc_page_zeroed)().ok_or(ElfError::OutOfMemory)?;
        (runtime().track_frame)(process_info_frame, FrameSource::UserData);

        address_space
            .map_page(
                PROCESS_INFO_ADDR,
                process_info_frame.addr,
                crate::mmu::user_flags::RO | crate::mmu::flags::UXN | crate::mmu::flags::PXN,
            )
            .map_err(|_| ElfError::MappingFailed("process info page"))?;

        address_space.track_user_frame(process_info_frame);

        let memory = ProcessMemory::new(brk, stack_bottom, stack_top, mmap_floor);

        log::debug!("[Process] PID {} memory: code_end=0x{:x}, stack=0x{:x}-0x{:x}, mmap=0x{:x}-0x{:x}",
            pid, brk, stack_bottom, stack_top, memory.next_mmap, memory.mmap_limit);

        let heap_lazy_size = compute_heap_lazy_size(brk, &memory);
        push_lazy_region(pid, brk, heap_lazy_size, crate::mmu::user_flags::RW_NO_EXEC);
        let lazy_stack_start = stack_top.saturating_sub(LAZY_STACK_MAX);
        push_lazy_region(pid, lazy_stack_start, LAZY_STACK_MAX, crate::mmu::user_flags::RW_NO_EXEC);

        Ok(Self {
            pid,
            pgid: pid,
            name: String::from(name),
            state: ProcessState::Ready,
            address_space,
            context: UserContext::new(entry_point, stack_pointer),
            parent_pid: 0,
            brk,
            initial_brk: brk,
            entry_point,
            memory,
            process_info_phys: process_info_frame.addr,
            args: Vec::new(),
            cwd: String::from("/"),
            stdin: Spinlock::new(StdioBuffer::new()),
            stdout: Spinlock::new(StdioBuffer::new()),
            exited: false,
            exit_code: 0,
            dynamic_page_tables: Vec::new(),
            mmap_regions: Vec::new(),
            lazy_regions: Vec::new(),
            fds: Arc::new(SharedFdTable::with_stdio()),
            thread_id: None,
            spawner_pid: None,
            terminal_state: Arc::new(Spinlock::new(terminal::TerminalState::default())),
            box_id: 0,
            namespace: akuma_isolation::global_namespace(),
            channel: None,
            delegate_pid: None,
            clear_child_tid: 0,
            robust_list_head: 0,
            robust_list_len: 0,
            signal_actions: Arc::new(SharedSignalTable::new()),
            signal_mask: 0,
            sigaltstack_sp: 0,
            sigaltstack_flags: 2, // SS_DISABLE
            sigaltstack_size: 0,
            start_time_us: (runtime().uptime_us)(),
            current_syscall: core::sync::atomic::AtomicU64::new(!0),
            last_syscall: core::sync::atomic::AtomicU64::new(0),
            syscall_stats: ProcessSyscallStats::new(),
})
    }

    /// Set command line arguments for this process
    ///
    /// Arguments will be passed to the process via the ProcessInfo page.
    pub fn set_args(&mut self, args: &[&str]) {
        self.args = args.iter().map(|s| String::from(*s)).collect();
    }
    
    /// Set current working directory for this process
    pub fn set_cwd(&mut self, cwd: &str) {
        self.cwd = String::from(cwd);
    }

    /// Start executing this process (enters user mode)
    ///
    /// This function does not return normally - it jumps to user space.
    /// When the process makes a syscall or exception, control returns to kernel.
    pub fn run(&mut self) -> ! {
        self.state = ProcessState::Running;

        // Activate the user address space
        self.address_space.activate();

        // Jump to user mode
        unsafe {
            enter_user_mode(&self.context);
        }
    }

    /// Prepare process for execution (internal helper)
    ///
    /// Sets up process state and writes process info to the info page.
    /// Does NOT register in process table or enter userspace.
    pub(crate) fn prepare_for_execution(&mut self) {
        self.state = ProcessState::Running;

        // Reset per-process I/O state
        self.reset_io();

        // Write process info to the physical page (before activating address space)
        unsafe {
            let info_ptr = crate::mmu::phys_to_virt(self.process_info_phys) as *mut ProcessInfo;
            let info = ProcessInfo::new(self.pid, self.parent_pid, self.box_id);
            core::ptr::write(info_ptr, info);
        }
    }

    // ========== Per-Process I/O Methods (thread-safe with size limits) ==========

    /// Set stdin data for this process (with size limit)
    pub fn set_stdin(&mut self, data: &[u8]) {
        let mut stdin = self.stdin.lock();
        stdin.set_with_limit(data, config().proc_stdin_max_size);
    }

    /// Read from this process's stdin
    /// Returns number of bytes read
    pub fn read_stdin(&mut self, buf: &mut [u8]) -> usize {
        let mut stdin = self.stdin.lock();
        stdin.read(buf)
    }

    /// Write to this process's stdout (with size limit)
    ///
    /// Applies "last write wins" policy: if adding data would exceed
    /// PROC_STDOUT_MAX_SIZE, clears buffer before writing.
    pub fn write_stdout(&mut self, data: &[u8]) {
        let mut stdout = self.stdout.lock();
        stdout.write_with_limit(data, config().proc_stdout_max_size);
    }

    /// Take captured stdout (transfers ownership)
    pub fn take_stdout(&mut self) -> Vec<u8> {
        let mut stdout = self.stdout.lock();
        core::mem::take(&mut stdout.data)
    }

    /// Get current program break
    pub fn get_brk(&self) -> usize {
        self.brk
    }

    /// Set program break, returns new value.
    /// Maps any new pages between old and new brk.
    /// Returns the exact requested value (matching Linux brk ABI).
    pub fn set_brk(&mut self, new_brk: usize) -> usize {
        if new_brk < self.initial_brk {
            return self.brk;
        }
        let aligned = (new_brk + 0xFFF) & !0xFFF;
        let old_top = (self.brk + 0xFFF) & !0xFFF;
        if aligned > old_top {
            let mut page = old_top;
            while page < aligned {
                if !self.address_space.is_range_mapped(page, 0x1000) {
                    let _ = self.address_space.alloc_and_map(page, crate::mmu::user_flags::RW_NO_EXEC);
                }
                page += 0x1000;
            }
        }
        self.brk = new_brk;
        self.brk
    }

    /// Reset I/O state for execution
    pub fn reset_io(&mut self) {
        self.stdin.lock().pos = 0;
        self.stdout.lock().clear();
        self.exited = false;
        self.exit_code = 0;
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Free any remaining dynamically allocated page table frames
        // This handles the case where the process is dropped without execute() being called
        for frame in self.dynamic_page_tables.drain(..) {
            (runtime().free_page)(frame);
        }
    }
}

/// Enter user mode with the given context
///
/// This sets up the CPU state and performs an ERET to EL0.
/// Does not return.
#[cfg(target_os = "none")]
#[inline(never)]
#[allow(dead_code)]
pub unsafe fn enter_user_mode(ctx: &UserContext) -> ! {
    // SAFETY: This inline asm sets up CPU state and ERETs to user mode.
    // x30 is pinned as the context pointer and loaded last to avoid corruption.
    unsafe {
        core::arch::asm!(
            // Set system registers from named operands (consumed before GP loads)
            "msr sp_el0, {sp_user}",
            "msr elr_el1, {pc}",
            "msr spsr_el1, {spsr}",
            "msr tpidr_el0, {tls}",
            // Load x0-x29 from context struct (x30 = ctx pointer, stable throughout)
            "ldp x0, x1, [x30]",
            "ldp x2, x3, [x30, #16]",
            "ldp x4, x5, [x30, #32]",
            "ldp x6, x7, [x30, #48]",
            "ldp x8, x9, [x30, #64]",
            "ldp x10, x11, [x30, #80]",
            "ldp x12, x13, [x30, #96]",
            "ldp x14, x15, [x30, #112]",
            "ldp x16, x17, [x30, #128]",
            "ldp x18, x19, [x30, #144]",
            "ldp x20, x21, [x30, #160]",
            "ldp x22, x23, [x30, #176]",
            "ldp x24, x25, [x30, #192]",
            "ldp x26, x27, [x30, #208]",
            "ldp x28, x29, [x30, #224]",
            // Load x30 last (overwrites ctx pointer, no longer needed)
            "ldr x30, [x30, #240]",
            "eret",
            in("x30") ctx as *const UserContext,
            sp_user = in(reg) ctx.sp,
            pc = in(reg) ctx.pc,
            spsr = in(reg) ctx.spsr,
            tls = in(reg) ctx.tpidr,
            options(noreturn)
        )
    }
}

#[cfg(not(target_os = "none"))]
#[allow(dead_code)]
pub unsafe fn enter_user_mode(_ctx: &UserContext) -> ! {
    panic!("not on bare metal")
}

/// Execute a boxed process - enters user mode and never returns
///
/// This function takes ownership of the Box<Process>, registers it in the
/// PROCESS_TABLE (which takes ownership), then enters userspace via ERET.
///
/// MEMORY MANAGEMENT:
/// Previously, Process lived on the thread closure's stack, but execute() never
/// returns (it ERETs to userspace). When the process exits, return_to_kernel()
/// is called from the exception handler context, so the closure never completes
/// and Process::drop() was never called, leaking all physical pages.
///
/// Now, the Process is heap-allocated via Box and owned by PROCESS_TABLE.
/// When return_to_kernel() calls unregister_process(), the Box is returned
/// and dropped, calling Process::drop() -> UserAddressSpace::drop() which
/// frees all physical pages (code, data, stack, heap, page tables).
#[allow(dead_code)]
fn execute_boxed(mut process: Box<Process>) -> ! {
    // Prepare the process (set state, write process info page)
    process.prepare_for_execution();
    
    // Get PID and context pointer before registering (which moves the Box)
    let pid = process.pid;
    
    // Get raw pointer to access process after registration
    // SAFETY: The Box is moved to PROCESS_TABLE which keeps it alive.
    // The pointer remains valid until unregister_process() is called,
    // which only happens in return_to_kernel() after we've left userspace.
    let proc_ptr = &mut *process as *mut Process;
    
    // Register the process in the table - this transfers ownership of the Box
    // to PROCESS_TABLE. The process memory will be freed when unregister_process
    // returns the Box and it goes out of scope.
    register_process(pid, process);
    
    // Get reference back through the raw pointer
    // SAFETY: process is now owned by PROCESS_TABLE and won't move or be freed
    // until unregister_process is called (which happens after we exit userspace)
    let proc_ref = unsafe { &mut *proc_ptr };
    
    // Activate the user address space (sets TTBR0)
    proc_ref.address_space.activate();

    // Now safe to enable IRQs - TTBR0 is set to user tables
    (runtime().enable_irqs)();

    // Enter user mode via ERET - this never returns
    // When user calls exit(), the exception handler calls return_to_kernel()
    // which unregisters the process (dropping the Box and freeing memory)
    unsafe {
        enter_user_mode(&proc_ref.context);
    }
}

/// Check if process has exited and return to kernel if so
/// Called from exception handler after each syscall
#[unsafe(no_mangle)]
pub extern "C" fn check_process_exit() -> bool {
    // Use per-process exit flag instead of global
    match current_process() {
        Some(proc) => proc.exited,
        None => false,
    }
}

/// Return to kernel after process exit
/// 
/// Called from exception handler when process exits.
/// 
/// UNIFIED CONTEXT ARCHITECTURE:
/// Instead of restoring from KernelContext and returning to run_user_until_exit,
/// we now clean up directly and terminate the thread. This eliminates the dual
/// context system (THREAD_CONTEXTS vs KernelContext) that was a source of bugs.
/// 
/// The thread is marked as terminated and the scheduler will reclaim it.
/// Kill all threads sharing the same address space (L0 page table).
/// Used by exit_group and when the address-space owner exits to prevent
/// sibling threads from running with freed page tables.
pub fn kill_thread_group(my_pid: Pid, l0_phys: usize) {
    let siblings: Vec<(Pid, Option<usize>)> = with_irqs_disabled(|| {
        let table = PROCESS_TABLE.lock();
        table.iter()
            .filter(|(pid, proc)| **pid != my_pid && proc.address_space.l0_phys() == l0_phys)
            .map(|(pid, proc)| (*pid, proc.thread_id))
            .collect()
    });

    for (sib_pid, sib_tid) in &siblings {
        if let Some(proc) = lookup_process(*sib_pid) {
            cleanup_process_fds(proc);
        }
        clear_lazy_regions(*sib_pid);

        if let Some(tid) = sib_tid {
            // DO NOT remove from THREAD_PID_MAP yet - wait for cleanup_callback
            if let Some(channel) = remove_channel(*tid) {
                channel.set_exited(137);
            }
        }

        // DO NOT unregister process yet - wait for cleanup_callback
        // Just mark as exited/zombie so wait4 can see it
        if let Some(proc) = lookup_process(*sib_pid) {
             proc.exited = true;
             proc.exit_code = 137;
             proc.state = ProcessState::Zombie(137);
        }

        if let Some(tid) = sib_tid {
            crate::threading::mark_thread_terminated(*tid);
            // Wake the thread so it exits naturally
            crate::threading::get_waker_for_thread(*tid).wake();
        }
    }

    if !siblings.is_empty() {
        log::debug!("[Process] Killed {} sibling thread(s) for PID {}",
            siblings.len(), my_pid);
    }
}

/// Exit code is communicated via ProcessChannel for async callers.
#[unsafe(no_mangle)]
pub extern "C" fn return_to_kernel(exit_code: i32) -> ! {
    let lr: u64;
    #[cfg(target_os = "none")]
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr); }
    #[cfg(not(target_os = "none"))]
    { lr = 0; }
    let tid = crate::threading::current_thread_id();
    log::debug!("[RTK] code={} tid={} LR={:#x}", exit_code, tid, lr);
    
    // Check if this thread was already killed externally (by kill_process).
    // If so, cleanup has already been done - just skip to the yield loop.
    // This handles the race where kill_process() terminates the thread while
    // it's still running, and it later reaches this exit path.
    let already_terminated = crate::threading::is_thread_terminated(tid);
    
    // Get process info before cleanup (skip if already killed)
    let pid = if !already_terminated {
        if let Some(proc) = current_process() {
            let pid = proc.pid;
            
            // Clean up all open FDs for this process (sockets, child channels)
            // This must happen before unregistering the process so we can access fd_table
            cleanup_process_fds(proc);
            
            Some(pid)
        } else {
            None
        }
    } else {
        None
    };
    
    // Set exit code on ProcessChannel if registered for this thread
    // This notifies async callers (SSH shell, etc.) that the process exited
    // Safe to call even if already removed by kill_process - just returns None
    if let Some(channel) = remove_channel(tid) {
        channel.set_exited(exit_code);
    }
    
    // Clean up THREAD_PID_MAP entry for thread clones
    with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().remove(&tid);
    });

    // CLONE_CHILD_CLEARTID: write 0 to the TID address and wake futex.
    // Must happen while user address space is still active.
    // Verify the page is actually mapped before writing — the address may
    // point to a lazily-mapped page that was never faulted in, and writing
    // from EL1 won't trigger demand paging (only EL0 faults do).
    if !already_terminated {
        if let Some(proc) = lookup_process(pid.unwrap_or(0)) {
            let tid_addr = proc.clear_child_tid;
            if tid_addr != 0 && crate::mmu::is_current_user_page_mapped(tid_addr as usize) {
                unsafe { core::ptr::write(tid_addr as *mut u32, 0); }
                (runtime().futex_wake)(tid_addr as usize, i32::MAX);
            }

            // Robust futex list cleanup: walk the list and mark owned futexes
            // with FUTEX_OWNER_DIED so waiters don't deadlock.
            let robust_head = proc.robust_list_head;
            if robust_head != 0 {
                const FUTEX_OWNER_DIED: u32 = 0x40000000;
                const ROBUST_LIST_LIMIT: usize = 2048;
                let my_tid = proc.pid;
                // robust_list_head layout: { next: *mut robust_list, futex_offset: long, list_op_pending: *mut robust_list }
                if crate::mmu::is_current_user_page_mapped(robust_head as usize) {
                    let futex_offset = unsafe {
                        core::ptr::read((robust_head as usize + 8) as *const i64)
                    };
                    let pending_ptr = unsafe {
                        core::ptr::read((robust_head as usize + 16) as *const u64)
                    };

                    // Walk the linked list
                    let mut entry = unsafe { core::ptr::read(robust_head as *const u64) };
                    let mut count = 0usize;
                    while entry != robust_head && entry != 0 && count < ROBUST_LIST_LIMIT {
                        if crate::mmu::is_current_user_page_mapped(entry as usize) {
                            let futex_addr = (entry as i64 + futex_offset) as usize;
                            if crate::mmu::is_current_user_page_mapped(futex_addr) {
                                let word = unsafe { core::ptr::read(futex_addr as *const u32) };
                                if (word & 0x3FFFFFFF) == my_tid {
                                    unsafe { core::ptr::write(futex_addr as *mut u32, word | FUTEX_OWNER_DIED); }
                                    (runtime().futex_wake)(futex_addr, 1);
                                }
                            }
                            entry = unsafe { core::ptr::read(entry as *const u64) };
                        } else {
                            break;
                        }
                        count += 1;
                    }

                    // Handle pending operation
                    if pending_ptr != 0 && crate::mmu::is_current_user_page_mapped(pending_ptr as usize) {
                        let futex_addr = (pending_ptr as i64 + futex_offset) as usize;
                        if crate::mmu::is_current_user_page_mapped(futex_addr) {
                            let word = unsafe { core::ptr::read(futex_addr as *const u32) };
                            if (word & 0x3FFFFFFF) == my_tid {
                                unsafe { core::ptr::write(futex_addr as *mut u32, word | FUTEX_OWNER_DIED); }
                                (runtime().futex_wake)(futex_addr, 1);
                            }
                        }
                    }
                }
            }
        }
    }

    // Deactivate user address space - restore boot TTBR0
    // CRITICAL: This must happen BEFORE we drop the Process (via unregister_process)
    // because Drop frees the page tables. If we drop first, TTBR0 would point to
    // freed memory causing a crash on any TLB miss.
    crate::mmu::UserAddressSpace::deactivate();
    
    // Now unregister and DROP the process
    // This calls Process::drop() -> UserAddressSpace::drop() which frees:
    // - All user pages (code, data, stack, heap, mmap)
    // - All page table frames (L0, L1, L2, L3)
    // - The ASID
    // This fixes the memory leak where processes would never free their pages.
    if let Some(pid) = pid {
        // Check if this was a primary process for an active box.
        // If so, the entire box should be shut down.
        let box_to_kill = find_primary_box(pid);

        if let Some(bid) = box_to_kill {
            log::debug!("[Process] Primary PID {} exited, shutting down box {:08x}", pid, bid);
            // kill_box handles unregistering the box and killing remaining PIDs
            if let Err(e) = kill_box(bid) {
                log::debug!("[Process] Error: Failed to kill box {:08x}: {}", bid, e);
            }
        }

        // If this process owns the address space (not shared), kill all
        // sibling CLONE_VM threads BEFORE dropping. Dropping the owner frees
        // all page tables; siblings still using them would cause EL1 faults.
        if let Some(proc) = lookup_process(pid) {
            if !proc.address_space.is_shared() {
                let l0_phys = proc.address_space.l0_phys();
                kill_thread_group(pid, l0_phys);
            }
        }

        let (start_us, proc_name) = lookup_process(pid)
            .map(|p| (p.start_time_us, p.name.clone()))
            .unwrap_or((0, alloc::string::String::from("?")));
        let elapsed_us = (runtime().uptime_us)().saturating_sub(start_us);
        let secs = elapsed_us / 1_000_000;
        let frac = (elapsed_us % 1_000_000) / 10_000; // centiseconds

        if process_syscall_stats_enabled() {
            if let Some(proc) = lookup_process(pid) {
                proc.syscall_stats.dump(pid, &proc_name, elapsed_us);
            }
        }

        clear_lazy_regions(pid);
        let _dropped_process = unregister_process(pid);
        log::debug!("[Process] PID {} thread {} exited ({}) [{}.{:02}s]", pid, tid, exit_code, secs, frac);
    } else {
        log::debug!("[Process] Thread {} exited ({})", tid, exit_code);
    }
    
    // Mark thread as terminated so scheduler stops scheduling it
    // Idempotent - safe to call even if already marked by kill_process
    crate::threading::mark_current_terminated();
    
    // Yield forever - thread is terminated, scheduler will reclaim it
    // Thread 0's cleanup routine will free the thread slot
    loop {
        crate::threading::yield_now();
    }
}

/// Process exit path used when recovering from an EL1 data abort (EC=0x25).
///
/// Identical to `return_to_kernel` except it skips all user-memory reads/writes
/// (CLONE_CHILD_CLEARTID and robust-futex list cleanup). Those writes use the
/// same EL1→user-VA path that triggered the original fault; attempting them here
/// would cause a second EC=0x25, redirecting ELR back to this function and
/// overflowing the kernel stack.
///
/// Skipping CLEARTID and robust-futex cleanup is safe because:
/// - The process is already marked Zombie before this runs.
/// - `kill_thread_group` has already terminated all sibling threads, so there
///   are no live waiters to wake via FUTEX_OWNER_DIED.
pub extern "C" fn return_to_kernel_from_fault(exit_code: i32) -> ! {
    let tid = crate::threading::current_thread_id();
    log::debug!("[RTK-FAULT] code={} tid={}", exit_code, tid);

    let already_terminated = crate::threading::is_thread_terminated(tid);

    let pid = if !already_terminated {
        if let Some(proc) = current_process() {
            let pid = proc.pid;
            cleanup_process_fds(proc);
            Some(pid)
        } else {
            None
        }
    } else {
        None
    };

    if let Some(channel) = remove_channel(tid) {
        channel.set_exited(exit_code);
    }

    with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().remove(&tid);
    });

    // SKIP: CLEARTID write — would re-trigger EC=0x25
    // SKIP: robust futex list cleanup — would re-trigger EC=0x25

    crate::mmu::UserAddressSpace::deactivate();

    if let Some(pid) = pid {
        let box_to_kill = find_primary_box(pid);
        if let Some(bid) = box_to_kill {
            log::debug!("[Process] Primary PID {} exited, shutting down box {:08x}", pid, bid);
            if let Err(e) = kill_box(bid) {
                log::debug!("[Process] Error: Failed to kill box {:08x}: {}", bid, e);
            }
        }

        if let Some(proc) = lookup_process(pid) {
            if !proc.address_space.is_shared() {
                let l0_phys = proc.address_space.l0_phys();
                kill_thread_group(pid, l0_phys);
            }
        }

        let start_us = lookup_process(pid)
            .map(|p| p.start_time_us)
            .unwrap_or(0);
        let elapsed_us = (runtime().uptime_us)().saturating_sub(start_us);
        let secs = elapsed_us / 1_000_000;
        let frac = (elapsed_us % 1_000_000) / 10_000;

        clear_lazy_regions(pid);
        let _dropped_process = unregister_process(pid);
        log::debug!("[Process] PID {} thread {} faulted ({}) [{}.{:02}s]", pid, tid, exit_code, secs, frac);
    } else {
        log::debug!("[Process] Thread {} faulted ({})", tid, exit_code);
    }

    crate::threading::mark_current_terminated();

    loop {
        crate::threading::yield_now();
    }
}

/// Clean up all file descriptors owned by a process.
///
/// With shared fd tables (CLONE_FILES), only the last thread referencing the
/// table performs actual cleanup. Other threads just drop their Arc reference.
fn cleanup_process_fds(proc: &Process) {
    if Arc::strong_count(&proc.fds) == 1 {
        proc.fds.close_all();
    }
}

pub fn waitpid(pid: Pid) -> Option<(Pid, i32)> {
    if let Some(ch) = get_child_channel(pid) {
        if ch.has_exited() {
            return Some((pid, ch.exit_code()));
        }
    }
    None
}

/// Fork the current process (deep copy)
/// Returns the new PID to the parent
pub fn fork_process(child_pid: u32, stack_ptr: u64) -> Result<u32, &'static str> {
    if (runtime().is_memory_low)() {
        return Err("Kernel memory low, cannot fork");
    }
    let parent = current_process().ok_or("No current process")?;
    let parent_pid = parent.pid;
    
    // 1. Create new address space
    let mut new_address_space = mmu::UserAddressSpace::new().ok_or("Failed to create address space")?;
    
    // 2. Allocate process info page
    let process_info_frame = (runtime().alloc_page_zeroed)().ok_or("OOM process info")?;
    (runtime().track_frame)(process_info_frame, FrameSource::UserData);
    
    new_address_space
        .map_page(
            PROCESS_INFO_ADDR,
            process_info_frame.addr,
            mmu::user_flags::RO | mmu::flags::UXN | mmu::flags::PXN,
        )
        .map_err(|_| "Failed to map process info")?;
    new_address_space.track_user_frame(process_info_frame);

    // 3. Create Process struct (fallible allocation to avoid kernel panic on OOM)
    let mut new_proc = Box::try_new(Process {
        pid: child_pid,
        pgid: parent.pgid,
        name: parent.name.clone(),
        parent_pid: parent_pid,
        state: ProcessState::Ready,
        context: UserContext::default(), // Will be updated below
        address_space: new_address_space,
        entry_point: parent.entry_point,
        brk: parent.brk,
        initial_brk: parent.initial_brk,
        memory: parent.memory.clone(),
        process_info_phys: process_info_frame.addr,
        args: parent.args.clone(),
        cwd: parent.cwd.clone(),
        stdin: Spinlock::new(StdioBuffer::new()),
        stdout: Spinlock::new(StdioBuffer::new()),
        exited: false,
        exit_code: 0,
        dynamic_page_tables: Vec::new(),
        mmap_regions: Vec::new(),
        lazy_regions: Vec::new(),
        fds: Arc::new(parent.fds.clone_deep_for_fork()),
        thread_id: None,
        spawner_pid: parent.spawner_pid,
        terminal_state: parent.terminal_state.clone(),
        box_id: parent.box_id,
        namespace: parent.namespace.clone(),
        channel: parent.channel.clone(),
        delegate_pid: None,
        clear_child_tid: 0,
        robust_list_head: 0,
        robust_list_len: 0,
        signal_actions: Arc::new(SharedSignalTable::new()), // Fork creates fresh table
        signal_mask: parent.signal_mask,
        sigaltstack_sp: parent.sigaltstack_sp,
        sigaltstack_flags: parent.sigaltstack_flags,
        sigaltstack_size: parent.sigaltstack_size,
        start_time_us: (runtime().uptime_us)(),
        current_syscall: core::sync::atomic::AtomicU64::new(!0),
        last_syscall: core::sync::atomic::AtomicU64::new(0),
        syscall_stats: ProcessSyscallStats::new(),
    }).map_err(|_| "Failed to allocate Process struct (ENOMEM)")?;
    
    // 4. Perform memory copy
    let stack_top = parent.memory.stack_top;
    let stack_size = config().user_stack_size; 
    let stack_start = stack_top - stack_size;
    
    // Snapshot parent's L0 page table pointer so we can translate VAs to
    // physical addresses without relying on TTBR0 staying valid across
    // potential context switches during the (long) copy.
    let parent_l0 = {
        let ttbr0 = mmu::get_current_ttbr0();
        let l0_addr = ttbr0 & 0x0000_FFFF_FFFF_F000;
        mmu::phys_to_virt(l0_addr) as *const u64
    };

    fn copy_range_phys(parent_l0: *const u64, src_va: usize, len: usize, dest_as: &mut mmu::UserAddressSpace) -> Result<(), &'static str> {
        let pages = (len + mmu::PAGE_SIZE - 1) / mmu::PAGE_SIZE;
        let mut copied = 0usize;
        for i in 0..pages {
            let va = src_va + i * mmu::PAGE_SIZE;
            if let Some(src_phys) = mmu::translate_user_va(parent_l0, va) {
                let frame = dest_as.alloc_and_map(va, mmu::user_flags::RW)?;
                unsafe {
                    let src_ptr = mmu::phys_to_virt(src_phys & !0xFFF) as *const u8;
                    let dest_ptr = mmu::phys_to_virt(frame.addr);
                    core::ptr::copy_nonoverlapping(src_ptr, dest_ptr, mmu::PAGE_SIZE);
                }
                copied += 1;
            }
        }
        if config().syscall_debug_info_enabled && copied < pages {
            log::debug!("[fork] copy_range WARNING: 0x{:x}..0x{:x}: {}/{} pages copied ({} unmapped)",
                src_va, src_va + len, copied, pages, pages - copied);
        }
        Ok(())
    }

    copy_range_phys(parent_l0, stack_start, stack_size, &mut new_proc.address_space)?;

    // Copy code+heap range.  Derive code_start from code_end (which is
    // always in the main binary's range) rather than entry_point (which
    // points into the interpreter for dynamically-linked binaries).
    let code_start = if parent.memory.code_end >= 0x1000_0000 {
        0x1000_0000 // PIE binary base
    } else {
        0x400000
    };
    if parent.brk > code_start {
        copy_range_phys(parent_l0, code_start, parent.brk - code_start, &mut new_proc.address_space)?;
    }

    // Copy dynamic linker / interpreter region (0x3000_0000).  These pages
    // are mapped by the ELF loader but not tracked in mmap_regions.
    let interp_base = 0x3000_0000usize;
    let interp_scan_size = 2 * 1024 * 1024; // 2 MB — covers even large musl builds
    if mmu::translate_user_va(parent_l0, interp_base).is_some() {
        copy_range_phys(parent_l0, interp_base, interp_scan_size, &mut new_proc.address_space)?;
    }

    // Copy mmap regions so forked children can run built-in applets (e.g.
    // busybox sh pipes) without crashing on unmapped pages.  We cap total
    // copied pages to avoid OOM when a parent has huge file mappings.
    const MAX_FORK_MMAP_PAGES: usize = 2048; // 8 MB cap
    let mut total_copied_pages: usize = 0;
    let mut child_mmap_regions: Vec<(usize, Vec<PhysFrame>)> = Vec::new();

    for (va_start, parent_frames) in &parent.mmap_regions {
        if total_copied_pages + parent_frames.len() > MAX_FORK_MMAP_PAGES {
            if config().syscall_debug_info_enabled {
                log::debug!("[fork] skipping mmap region 0x{:x} ({} pages) — would exceed cap",
                    va_start, parent_frames.len());
            }
            continue;
        }
        let mut child_frames: Vec<PhysFrame> = Vec::new();
        let mut ok = true;
        for (i, pf) in parent_frames.iter().enumerate() {
            let page_va = va_start + i * mmu::PAGE_SIZE;
            match (runtime().alloc_page_zeroed)() {
                Some(frame) => {
                    (runtime().track_frame)(frame, FrameSource::UserData);
                    unsafe {
                        let src = mmu::phys_to_virt(pf.addr) as *const u8;
                        let dst = mmu::phys_to_virt(frame.addr);
                        core::ptr::copy_nonoverlapping(src, dst, mmu::PAGE_SIZE);
                    }
                    if new_proc.address_space.map_page(page_va, frame.addr, mmu::user_flags::RW).is_err() {
                        ok = false;
                        break;
                    }
                    new_proc.address_space.track_user_frame(frame);
                    child_frames.push(frame);
                }
                None => { ok = false; break; }
            }
        }
        if ok {
            total_copied_pages += child_frames.len();
            child_mmap_regions.push((*va_start, child_frames));
        } else {
            if config().syscall_debug_info_enabled {
                log::debug!("[fork] OOM copying mmap region 0x{:x}, skipping rest", va_start);
            }
            break;
        }
    }

    new_proc.mmap_regions = child_mmap_regions;
    new_proc.lazy_regions = Vec::new(); // managed via LAZY_REGION_TABLE
    new_proc.memory.next_mmap = parent.memory.next_mmap;

    // Copy physically-mapped pages from parent's lazy regions.
    // clone_lazy_regions() (called later) copies only metadata; any pages
    // already demand-paged in the parent (e.g. Go goroutine structs in the
    // Go heap arena) must be explicitly copied so the child doesn't get
    // zeroed pages and dereference a nil goroutine pointer.
    // copy_range_phys skips unmapped pages, so sparse regions are cheap.
    // We cap total pages to avoid excessive copy time for large heaps.
    {
        const MAX_FORK_LAZY_PAGES: usize = 4096; // 16 MB cap
        let lazy_ranges: alloc::vec::Vec<(usize, usize)> = with_irqs_disabled(|| {
            let table = LAZY_REGION_TABLE.lock();
            table.get(&parent_pid)
                .map(|regions| regions.iter().map(|r| (r.start_va, r.size)).collect())
                .unwrap_or_default()
        });
        let mut lazy_pages_copied = 0usize;
        'lazy_copy: for (va, size) in lazy_ranges {
            let pages = (size + mmu::PAGE_SIZE - 1) / mmu::PAGE_SIZE;
            for i in 0..pages {
                if lazy_pages_copied >= MAX_FORK_LAZY_PAGES {
                    break 'lazy_copy;
                }
                let page_va = va + i * mmu::PAGE_SIZE;
                if let Some(src_phys) = mmu::translate_user_va(parent_l0, page_va) {
                    if let Ok(frame) = new_proc.address_space.alloc_and_map(page_va, mmu::user_flags::RW) {
                        unsafe {
                            let src = mmu::phys_to_virt(src_phys & !0xFFF) as *const u8;
                            let dst = mmu::phys_to_virt(frame.addr);
                            core::ptr::copy_nonoverlapping(src, dst, mmu::PAGE_SIZE);
                        }
                        lazy_pages_copied += 1;
                    }
                }
            }
        }
        if config().syscall_debug_info_enabled && lazy_pages_copied > 0 {
            log::debug!("[fork] copied {} lazy pages from pid={}", lazy_pages_copied, parent_pid);
        }
    }
    
    // 5. Write ProcessInfo to child's process info page
    unsafe {
        let info_ptr = mmu::phys_to_virt(new_proc.process_info_phys) as *mut ProcessInfo;
        let info = ProcessInfo::new(child_pid, parent_pid, new_proc.box_id);
        core::ptr::write(info_ptr, info);
    }

    // 6. Capture parent's user context and create child context
    let parent_tid = crate::threading::current_thread_id();
    let parent_ctx = crate::threading::get_saved_user_context(parent_tid).ok_or("No saved context")?;
    
    let mut child_ctx = parent_ctx;
    child_ctx.x0 = 0;    // fork returns 0 to child
    child_ctx.spsr = 0;  // Clean EL0t with interrupts enabled
    if stack_ptr != 0 {
        child_ctx.sp = stack_ptr;
    }

    // Store context in the Process struct (entry_point_trampoline uses proc.context)
    new_proc.context = child_ctx;

    // 7. Allocate thread but keep it INITIALIZING
    let tid = crate::threading::spawn_user_thread_initializing(
        entry_point_trampoline as extern "C" fn() -> !, 
        core::ptr::null_mut(), 
        false
    )?;
    
    new_proc.thread_id = Some(tid);
    crate::threading::update_thread_context(tid, &child_ctx);

    // 8. Create a ProcessChannel for exit notification only.
    // The child keeps parent.channel (set in struct init above) for I/O so its
    // stdout writes are visible on the same SSH stream as the parent.
    // The exit-tracking channel is separate to avoid contaminating the I/O channel.
    let exit_channel = Arc::new(ProcessChannel::new());
    register_channel(tid, exit_channel.clone());
    register_child_channel(child_pid, exit_channel, parent_pid);

    // Register process BEFORE marking thread READY
    register_process(child_pid, new_proc);
    clone_lazy_regions(parent_pid, child_pid);
    
    // Now safe to start the thread
    crate::threading::mark_thread_ready(tid);
    
    Ok(child_pid)
}

/// Clone a thread within the same process (CLONE_THREAD | CLONE_VM).
/// The child shares the parent's address space and file descriptors.
pub fn clone_thread(stack: u64, tls: u64, parent_tid_ptr: u64, child_tid_ptr: u64) -> Result<u32, &'static str> {
    if (runtime().is_memory_low)() {
        return Err("Kernel memory low, cannot clone thread");
    }
    let parent = current_process().ok_or("No current process")?;
    let parent_pid = parent.pid;
    let child_pid = allocate_pid();

    let parent_l0_phys = parent.address_space.ttbr0() & 0x0000_FFFF_FFFF_F000;
    let shared_as = mmu::UserAddressSpace::new_shared(parent_l0_phys as usize)
        .ok_or("Failed to create shared address space")?;

    let mut new_proc = Box::try_new(Process {
        pid: child_pid,
        pgid: parent.pgid,
        name: parent.name.clone(),
        parent_pid: parent_pid,
        state: ProcessState::Ready,
        context: UserContext::default(),
        address_space: shared_as,
        entry_point: parent.entry_point,
        brk: parent.brk,
        initial_brk: parent.initial_brk,
        memory: parent.memory.clone(),
        process_info_phys: parent.process_info_phys,
        args: parent.args.clone(),
        cwd: parent.cwd.clone(),
        stdin: Spinlock::new(StdioBuffer::new()),
        stdout: Spinlock::new(StdioBuffer::new()),
        exited: false,
        exit_code: 0,
        dynamic_page_tables: Vec::new(),
        mmap_regions: Vec::new(),
        lazy_regions: Vec::new(), // managed via LAZY_REGION_TABLE
        fds: parent.fds.clone(), // Arc::clone — shared fd table (CLONE_FILES)
        thread_id: None,
        spawner_pid: parent.spawner_pid,
        terminal_state: parent.terminal_state.clone(),
        box_id: parent.box_id,
        namespace: parent.namespace.clone(),
        channel: parent.channel.clone(),
        delegate_pid: None,
        clear_child_tid: child_tid_ptr,
        robust_list_head: 0,
        robust_list_len: 0,
        signal_actions: parent.signal_actions.clone(), // Shared table (Arc clone)
        signal_mask: parent.signal_mask,
        sigaltstack_sp: parent.sigaltstack_sp,
        sigaltstack_flags: parent.sigaltstack_flags,
        sigaltstack_size: parent.sigaltstack_size,
        start_time_us: (runtime().uptime_us)(),
        current_syscall: core::sync::atomic::AtomicU64::new(!0),
        last_syscall: core::sync::atomic::AtomicU64::new(0),
        syscall_stats: ProcessSyscallStats::new(),
    }).map_err(|_| "Failed to allocate Process struct (ENOMEM)")?;

    let parent_tid = crate::threading::current_thread_id();
    let parent_ctx = crate::threading::get_saved_user_context(parent_tid).ok_or("No saved context")?;

    let mut child_ctx = parent_ctx;
    child_ctx.x0 = 0;
    child_ctx.sp = stack;
    child_ctx.tpidr = tls;
    child_ctx.spsr = 0;

    new_proc.context = child_ctx;

    let tid = crate::threading::spawn_user_thread_initializing(
        entry_point_trampoline as extern "C" fn() -> !,
        core::ptr::null_mut(),
        false
    )?;

    new_proc.thread_id = Some(tid);
    crate::threading::update_thread_context(tid, &child_ctx);

    let exit_channel = Arc::new(ProcessChannel::new());
    register_channel(tid, exit_channel.clone());
    register_child_channel(child_pid, exit_channel, parent_pid);

    // Register in THREAD_PID_MAP so current_process() works for this thread
    with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().insert(tid, child_pid);
    });

    register_process(child_pid, new_proc);
    clone_lazy_regions(parent_pid, child_pid);

    // Write child TID/PID to parent_tid_ptr (CLONE_PARENT_SETTID)
    if parent_tid_ptr != 0 {
        unsafe { core::ptr::write(parent_tid_ptr as *mut u32, child_pid); }
    }
    // Write child TID/PID to child_tid_ptr (CLONE_CHILD_CLEARTID)
    if child_tid_ptr != 0 {
        unsafe { core::ptr::write(child_tid_ptr as *mut u32, child_pid); }
    }

    crate::threading::mark_thread_ready(tid);

    if config().syscall_debug_info_enabled {
        log::debug!("[syscall] clone_thread: PID {} -> thread PID {} (tid {})", parent_pid, child_pid, tid);
    }

    Ok(child_pid)
}

/// Allocate a new unique PID (uses the same global counter as Process::from_elf)
pub fn allocate_pid() -> Pid {
    NEXT_PID.fetch_add(1, Ordering::SeqCst)
}

/// Trampoline for new process threads
/// Called by threading::spawn_user_thread
pub extern "C" fn entry_point_trampoline() -> ! {
    let tid = crate::threading::current_thread_id();
    let mut proc_ptr: *mut Process = core::ptr::null_mut();
    
    with_irqs_disabled(|| {
        let mut processes = PROCESS_TABLE.lock();
        for proc in processes.values_mut() {
            if proc.thread_id == Some(tid) {
                proc_ptr = &mut **proc as *mut Process;
                break;
            }
        }
    });
    
    if proc_ptr.is_null() {
        log::debug!("[process] FATAL: No process found for thread {}", tid);
        crate::threading::mark_current_terminated();
        loop { crate::threading::yield_now(); }
    }
    
    unsafe {
        (*proc_ptr).run();
    }
}



