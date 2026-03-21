//! Process Management
//!
//! Manages user processes including creation, execution, and termination.

pub mod types;
pub mod table;
pub mod channel;
pub mod children;
pub mod signal;

pub use types::*;
pub use table::*;
pub use channel::*;
pub use children::*;
pub use signal::*;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use spinning_top::Spinlock;

use crate::elf_loader::{self, ElfError};
use crate::mmu::{self, UserAddressSpace};
use crate::runtime::{PhysFrame, FrameSource, runtime, config, with_irqs_disabled};
use akuma_terminal as terminal;

static PROCESS_SYSCALL_STATS_ENABLED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub fn enable_process_syscall_stats(enabled: bool) {
    PROCESS_SYSCALL_STATS_ENABLED.store(enabled, Ordering::Relaxed);
}

fn process_syscall_stats_enabled() -> bool {
    PROCESS_SYSCALL_STATS_ENABLED.load(Ordering::Relaxed)
}

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

pub struct SharedFdTable {
    pub table: Spinlock<BTreeMap<u32, FileDescriptor>>,
    pub cloexec: Spinlock<BTreeSet<u32>>,
    pub nonblock: Spinlock<BTreeSet<u32>>,
    pub next_fd: AtomicU32,
}

impl SharedFdTable {
    pub fn new() -> Self {
        Self {
            table: Spinlock::new(BTreeMap::new()),
            cloexec: Spinlock::new(BTreeSet::new()),
            nonblock: Spinlock::new(BTreeSet::new()),
            next_fd: AtomicU32::new(3),
        }
    }

    pub fn with_stdio() -> Self {
        let mut fd_map = BTreeMap::new();
        fd_map.insert(0, FileDescriptor::Stdin);
        fd_map.insert(1, FileDescriptor::Stdout);
        fd_map.insert(2, FileDescriptor::Stderr);
        Self {
            table: Spinlock::new(fd_map),
            cloexec: Spinlock::new(BTreeSet::new()),
            nonblock: Spinlock::new(BTreeSet::new()),
            next_fd: AtomicU32::new(3),
        }
    }

    /// Deep copy for fork (separate fd table, with pipe ref bumps).
    /// Strips EpollFd entries since epoll instances are not reference-counted.
    #[must_use]
    pub fn clone_deep_for_fork(&self) -> Self {
        let cloned: BTreeMap<u32, FileDescriptor> = self.table.lock().iter()
            .filter(|(_, fd)| !matches!(fd, FileDescriptor::EpollFd(_)))
            .map(|(&k, v)| (k, v.clone()))
            .collect();
        for entry in cloned.values() {
            match entry {
                FileDescriptor::PipeWrite(id) => (crate::runtime::runtime().pipe_clone_ref)(*id, true),
                FileDescriptor::PipeRead(id) => (crate::runtime::runtime().pipe_clone_ref)(*id, false),
                _ => {}
            }
        }
        Self {
            table: Spinlock::new(cloned),
            cloexec: Spinlock::new(self.cloexec.lock().clone()),
            nonblock: Spinlock::new(self.nonblock.lock().clone()),
            next_fd: AtomicU32::new(self.next_fd.load(Ordering::Relaxed)),
        }
    }

    /// Explicitly close all underlying kernel resources and clear the table.
    /// This is used during process exit to ensure immediate cleanup.
    pub fn close_all(&self) {
        let fds: alloc::vec::Vec<FileDescriptor> = {
            let mut table = self.table.lock();
            let items: alloc::vec::Vec<FileDescriptor> = table.values().cloned().collect();
            table.clear(); // Ensure we don't close twice
            items
        };
        
        for fd in fds {
            match fd {
                FileDescriptor::Socket(idx) => {
                    (runtime().remove_socket)(idx);
                }
                FileDescriptor::ChildStdout(child_pid) => {
                    remove_child_channel(child_pid);
                }
                FileDescriptor::PipeWrite(pipe_id) => {
                    (runtime().pipe_close_write)(pipe_id);
                }
                FileDescriptor::PipeRead(pipe_id) => {
                    (runtime().pipe_close_read)(pipe_id);
                }
                FileDescriptor::EventFd(efd_id) => {
                    (runtime().eventfd_close)(efd_id);
                }
                FileDescriptor::EpollFd(epoll_id) => {
                    (runtime().epoll_destroy)(epoll_id);
                }
                FileDescriptor::PidFd(pidfd_id) => {
                    (runtime().pidfd_close)(pidfd_id);
                }
                _ => {}
            }
        }
    }
}

impl Drop for SharedFdTable {
    fn drop(&mut self) {
        self.close_all();
    }
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

// ============================================================================
pub struct ProcessSyscallStats {
    counts: [AtomicU64; Self::MAX_NR],
    times_us: [AtomicU64; Self::MAX_NR],
    pub pagefaults: AtomicU64,
    pub pagefault_pages: AtomicU64,
}

impl ProcessSyscallStats {
    const MAX_NR: usize = 512;

    pub const fn new() -> Self {
        Self {
            counts: [const { AtomicU64::new(0) }; Self::MAX_NR],
            times_us: [const { AtomicU64::new(0) }; Self::MAX_NR],
            pagefaults: AtomicU64::new(0),
            pagefault_pages: AtomicU64::new(0),
        }
    }

    pub fn inc(&self, nr: u64) {
        let idx = nr as usize;
        if idx < Self::MAX_NR {
            self.counts[idx].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn add_time_us(&self, nr: u64, us: u64) {
        let idx = nr as usize;
        if idx < Self::MAX_NR {
            self.times_us[idx].fetch_add(us, Ordering::Relaxed);
        }
    }

    pub fn inc_pagefault(&self, pages: u64) {
        self.pagefaults.fetch_add(1, Ordering::Relaxed);
        self.pagefault_pages.fetch_add(pages, Ordering::Relaxed);
    }

    pub fn dump(&self, pid: Pid, name: &str, elapsed_us: u64) {
        use alloc::format;
        use alloc::vec::Vec;

        let mut total: u64 = 0;
        let mut total_time_us: u64 = 0;
        let mut entries: Vec<(usize, u64, u64)> = Vec::new();
        for i in 0..Self::MAX_NR {
            let c = self.counts[i].load(Ordering::Relaxed);
            if c > 0 {
                let t = self.times_us[i].load(Ordering::Relaxed);
                total += c;
                total_time_us += t;
                entries.push((i, c, t));
            }
        }
        if total == 0 { return; }

        // Sort by time spent (descending) — shows the slowest syscalls first
        entries.sort_by(|a, b| b.2.cmp(&a.2));

        let secs = elapsed_us / 1_000_000;
        let frac = (elapsed_us % 1_000_000) / 10_000;
        let rate = if elapsed_us > 0 { total * 1_000_000 / elapsed_us } else { 0 };
        let (pmm_total, _pmm_alloc, pmm_free) = (runtime().pmm_stats)();
        let pf = self.pagefaults.load(Ordering::Relaxed);
        let pf_pg = self.pagefault_pages.load(Ordering::Relaxed);

        let mut top = alloc::string::String::new();
        for (i, (nr, count, time)) in entries.iter().enumerate() {
            if i > 0 { top.push(' '); }
            let sname = syscall_name(*nr);
            let time_ms = *time / 1000;
            if sname.is_empty() {
                let _ = core::fmt::Write::write_fmt(&mut top, format_args!("nr{}={}({}ms)", nr, count, time_ms));
            } else {
                let _ = core::fmt::Write::write_fmt(&mut top, format_args!("{}={}({}ms)", sname, count, time_ms));
            }
            if i >= 9 { break; }
        }

        let total_time_ms = total_time_us / 1000;
        let msg = format!(
            "[PSTATS] PID {} ({}) {}.{:02}s: {} syscalls ({}/s) in_kernel={}ms pmm={}free/{}tot pgfault={}({}pg) | {}\n",
            pid, name, secs, frac, total, rate, total_time_ms,
            pmm_free, pmm_total, pf, pf_pg, top,
        );
        (runtime().print_str)(&msg);
    }
}

fn syscall_name(nr: usize) -> &'static str {
    match nr {
        0 => "io_setup", 29 => "ioctl", 46 => "ftruncate",
        48 => "faccessat", 56 => "openat", 57 => "close",
        59 => "pipe2", 61 => "getdents64", 62 => "lseek",
        63 => "read", 64 => "write", 65 => "readv",
        66 => "writev", 67 => "pread64", 68 => "pwrite64",
        72 => "pselect6", 73 => "ppoll",
        78 => "readlinkat", 79 => "fstatat", 80 => "fstat",
        93 => "exit", 94 => "exit_group",
        96 => "set_tid_address", 98 => "futex",
        99 => "set_robust_list",
        113 => "clock_gettime", 115 => "clock_nanosleep",
        124 => "sched_yield",
        130 => "tkill", 131 => "tgkill",
        134 => "rt_sigaction", 135 => "rt_sigprocmask",
        160 => "uname", 167 => "prctl",
        172 => "getpid", 174 => "getuid", 175 => "geteuid",
        176 => "getgid", 177 => "getegid", 178 => "gettid",
        198 => "socket", 200 => "bind", 201 => "listen",
        202 => "accept", 203 => "connect",
        204 => "getsockname", 205 => "getpeername",
        206 => "sendto", 207 => "recvfrom",
        208 => "setsockopt", 209 => "getsockopt",
        210 => "shutdown",
        214 => "brk",
        215 => "munmap", 216 => "mremap", 222 => "mmap",
        226 => "mprotect", 233 => "madvise",
        220 => "clone", 221 => "execve",
        260 => "wait4",
        261 => "prlimit64",
        278 => "getrandom",
        281 => "memfd_create",
        282 => "membarrier",
        20 => "epoll_create1", 21 => "epoll_ctl", 22 => "epoll_pwait",
        25 => "fcntl",
        26 => "inotify_init1", 27 => "inotify_add_watch",
        35 => "unlinkat",
        85 => "timerfd_create", 86 => "timerfd_settime",
        19 => "eventfd2",
        34 => "mkdirat", 45 => "truncate",
        291 => "statx",
        435 => "clone3", 439 => "faccessat2",
        _ => "",
    }
}


/// Dump syscall stats for all running processes (called periodically from heartbeat).
pub fn dump_running_process_stats() {
    if !process_syscall_stats_enabled() { return; }
    let pids: Vec<(Pid, alloc::string::String, u64)> = with_irqs_disabled(|| {
        let table = PROCESS_TABLE.lock();
        table.iter()
            .filter(|(_, p)| !p.exited && p.start_time_us > 0)
            .map(|(&pid, p)| (pid, p.name.clone(), p.start_time_us))
            .collect()
    });
    let now = (runtime().uptime_us)();
    for (pid, name, start_us) in pids {
        let elapsed = now.saturating_sub(start_us);
        if elapsed < 10_000_000 { continue; } // skip processes running < 10s
        if let Some(proc) = lookup_process(pid) {
            proc.syscall_stats.dump(pid, &name, elapsed);
        }
    }
}

/// Maximum virtual address range registered for demand-paged stack growth.
/// Physical pages are only allocated on fault, so this costs nothing unless used.
/// 32 MB is enough for even the heaviest runtimes (Bun/JSC uses ~600KB–2MB).
const LAZY_STACK_MAX: usize = 32 * 1024 * 1024;

fn compute_heap_lazy_size(brk: usize, memory: &ProcessMemory) -> usize {
    const MIN_HEAP: usize = 16 * 1024 * 1024;
    const RESERVE_PAGES: usize = 2048; // 8MB

    let (_, _, free) = (runtime().pmm_stats)();
    let phys_cap = free.saturating_sub(RESERVE_PAGES) * crate::mmu::PAGE_SIZE;
    let va_cap = memory.next_mmap.saturating_sub(brk);

    core::cmp::max(core::cmp::min(phys_cap, va_cap), MIN_HEAP)
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

    /// Replace current process image with a new ELF binary (execve core)
    pub fn replace_image(&mut self, elf_data: &[u8], args: &[String], env: &[String]) -> Result<(), String> {
        let interp_prefix: Option<&str> = None;
        let (entry_point, mut address_space, sp, brk, stack_bottom, stack_top, mmap_floor, _deferred) =
            crate::elf_loader::load_elf_with_stack(elf_data, args, env, config().user_stack_size, interp_prefix)
            .map_err(|e| format!("Failed to load ELF: {}", e))?;

        mmu::UserAddressSpace::deactivate();
        self.address_space = address_space;
        self.entry_point = entry_point;
        self.brk = brk;
        self.initial_brk = brk;
        self.memory = ProcessMemory::new(brk, stack_bottom, stack_top, mmap_floor);
        self.mmap_regions.clear();
        self.lazy_regions.clear();
        clear_lazy_regions(self.pid);
        self.dynamic_page_tables.clear();
        self.args = args.to_vec();
        self.clear_child_tid = 0;

        let heap_lazy_size = compute_heap_lazy_size(brk, &self.memory);
        push_lazy_region(self.pid, brk, heap_lazy_size, crate::mmu::user_flags::RW_NO_EXEC);
        let lazy_stack_start = stack_top.saturating_sub(LAZY_STACK_MAX);
        push_lazy_region(self.pid, lazy_stack_start, LAZY_STACK_MAX, crate::mmu::user_flags::RW_NO_EXEC);

        if config().syscall_debug_info_enabled {
            log::debug!("[Process] PID {} replaced: entry=0x{:x}, brk=0x{:x}, stack=0x{:x}-0x{:x}, sp=0x{:x}",
                self.pid, entry_point, brk, stack_bottom, stack_top, sp);
        }

        // Update context for the next run
        self.context = UserContext::new(entry_point, sp);
        
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

        mmu::UserAddressSpace::deactivate();

        self.address_space = address_space;
        self.entry_point = entry_point;
        self.brk = brk;
        self.initial_brk = brk;
        self.memory = ProcessMemory::new(brk, stack_bottom, stack_top, mmap_floor);
        self.mmap_regions.clear();
        self.lazy_regions.clear();
        clear_lazy_regions(self.pid);
        self.dynamic_page_tables.clear();
        self.args = args.to_vec();
        self.clear_child_tid = 0;

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
            push_lazy_region_with_source(self.pid, seg.start_va, seg.size, seg.page_flags, source);
        }

        let heap_lazy_size = compute_heap_lazy_size(brk, &self.memory);
        push_lazy_region(self.pid, brk, heap_lazy_size, crate::mmu::user_flags::RW_NO_EXEC);
        let lazy_stack_start = stack_top.saturating_sub(LAZY_STACK_MAX);
        push_lazy_region(self.pid, lazy_stack_start, LAZY_STACK_MAX, crate::mmu::user_flags::RW_NO_EXEC);

        if config().syscall_debug_info_enabled {
            log::debug!("[Process] PID {} replaced (on-demand): entry=0x{:x}, brk=0x{:x}, stack=0x{:x}-0x{:x}, sp=0x{:x}",
                self.pid, entry_point, brk, stack_bottom, stack_top, sp);
        }

        self.context = UserContext::new(entry_point, sp);

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
    fn prepare_for_execution(&mut self) {
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

    // ========== File Descriptor Table Methods ==========

    /// Allocate a new file descriptor and insert the entry atomically
    ///
    /// This is the correct pattern to avoid race conditions:
    /// the FD number is allocated and inserted while holding the lock.
    pub fn alloc_fd(&self, entry: FileDescriptor) -> u32 {
        with_irqs_disabled(|| {
            let mut table = self.fds.table.lock();
            let fd = self.fds.next_fd.fetch_add(1, Ordering::SeqCst);
            table.insert(fd, entry);
            fd
        })
    }

    /// Get a file descriptor entry (cloned)
    pub fn get_fd(&self, fd: u32) -> Option<FileDescriptor> {
        with_irqs_disabled(|| {
            self.fds.table.lock().get(&fd).cloned()
        })
    }

    /// Remove and return a file descriptor entry
    pub fn remove_fd(&self, fd: u32) -> Option<FileDescriptor> {
        with_irqs_disabled(|| {
            self.fds.table.lock().remove(&fd)
        })
    }

    /// Set a file descriptor entry at a specific FD number, replacing any existing entry
    pub fn set_fd(&self, fd: u32, entry: FileDescriptor) {
        with_irqs_disabled(|| {
            self.fds.table.lock().insert(fd, entry);
        });
    }

    /// Atomically replace a file descriptor, returning the old entry if one existed.
    /// Use this instead of get_fd + set_fd when you need to close the old entry,
    /// to avoid a TOCTOU race on shared fd tables (CLONE_FILES).
    pub fn swap_fd(&self, fd: u32, entry: FileDescriptor) -> Option<FileDescriptor> {
        with_irqs_disabled(|| {
            self.fds.table.lock().insert(fd, entry)
        })
    }

    /// Update a file descriptor entry (for file position updates, etc.)
    pub fn update_fd<F>(&self, fd: u32, f: F) -> bool
    where
        F: FnOnce(&mut FileDescriptor),
    {
        with_irqs_disabled(|| {
            let mut table = self.fds.table.lock();
            if let Some(entry) = table.get_mut(&fd) {
                f(entry);
                true
            } else {
                false
            }
        })
    }

    pub fn set_cloexec(&self, fd: u32) {
        with_irqs_disabled(|| {
            self.fds.cloexec.lock().insert(fd);
        });
    }

    pub fn clear_cloexec(&self, fd: u32) {
        with_irqs_disabled(|| {
            self.fds.cloexec.lock().remove(&fd);
        });
    }

    pub fn is_cloexec(&self, fd: u32) -> bool {
        with_irqs_disabled(|| {
            self.fds.cloexec.lock().contains(&fd)
        })
    }

    pub fn set_nonblock(&self, fd: u32) {
        with_irqs_disabled(|| {
            self.fds.nonblock.lock().insert(fd);
        });
    }

    pub fn clear_nonblock(&self, fd: u32) {
        with_irqs_disabled(|| {
            self.fds.nonblock.lock().remove(&fd);
        });
    }

    pub fn is_nonblock(&self, fd: u32) -> bool {
        with_irqs_disabled(|| {
            self.fds.nonblock.lock().contains(&fd)
        })
    }

    /// Close all FDs marked close-on-exec, returning them for cleanup.
    pub fn close_cloexec_fds(&self) -> Vec<(u32, FileDescriptor)> {
        with_irqs_disabled(|| {
            let cloexec: Vec<u32> = self.fds.cloexec.lock().iter().copied().collect();
            let mut closed = Vec::new();
            let mut table = self.fds.table.lock();
            for fd in &cloexec {
                if let Some(entry) = table.remove(fd) {
                    closed.push((*fd, entry));
                }
            }
            self.fds.cloexec.lock().clear();
            closed
        })
    }

    /// Get a reference to the shared fd table (for direct access in sys_close_range, etc.)
    pub fn fd_table(&self) -> &Arc<SharedFdTable> {
        &self.fds
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

/// Execute an ELF binary from the filesystem with per-process I/O (blocking)
///
/// This spawns the process on a user thread and polls for completion.
/// Use exec_async() for non-blocking execution.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments (first arg is conventionally the program name)
/// * `stdin` - Optional stdin data for the process
///
/// # Returns
/// Tuple of (exit_code, stdout_data), or error message
pub fn exec_with_io(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>) -> Result<(i32, Vec<u8>), String> {
    exec_with_io_cwd(path, args, None, stdin, None)
}

/// exec_with_io with explicit cwd
pub fn exec_with_io_cwd(path: &str, args: Option<&[&str]>, env: Option<&[String]>, stdin: Option<&[u8]>, cwd: Option<&str>) -> Result<(i32, Vec<u8>), String> {
    // Spawn process with channel and cwd
    let (thread_id, channel, _pid) = spawn_process_with_channel_cwd(path, args, env, stdin, cwd)?;
    
    // For non-interactive execution, if no stdin was provided, mark it as closed
    // so the process doesn't block forever if it tries to read from it.
    if stdin.is_none() {
        channel.close_stdin();
    }

    // Poll until process exits (blocking)
    loop {
        if channel.has_exited() || crate::threading::is_thread_terminated(thread_id) {
            break;
        }
        // Yield to let process run
        crate::threading::yield_now();
    }
    
    // Collect output
    let mut stdout_data = Vec::new();
    while let Some(data) = channel.try_read() {
        stdout_data.extend_from_slice(&data);
    }
    
    // Cleanup terminated thread
    crate::threading::cleanup_terminated();
    
    Ok((channel.exit_code(), stdout_data))
}

/// Execute an ELF binary from the filesystem (legacy API for backwards compatibility)
///
/// # Arguments
/// * `path` - Path to the ELF binary
///
/// # Returns
/// Exit code of the process, or error message
#[allow(dead_code)]
pub fn exec(path: &str) -> Result<i32, String> {
    let (exit_code, _stdout) = exec_with_io(path, None, None)?;
    Ok(exit_code)
}

/// Spawn a process on a user thread for concurrent execution
///
/// This function creates a new process from the ELF file and spawns it on a
/// dedicated user thread (slots 8-31). The process runs concurrently with
/// other threads and processes.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
///
/// # Returns
/// Thread ID of the spawned thread, or error message
pub fn spawn_process(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>) -> Result<usize, String> {
    let (thread_id, _channel, _pid) = spawn_process_with_channel(path, args, stdin)?;
    Ok(thread_id)
}

/// Spawn a process on a user thread with a channel for I/O
///
/// Like spawn_process, but returns a ProcessChannel that can be used to
/// read the process's output and check its exit status.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
/// * `cwd` - Optional current working directory (defaults to "/")
///
/// # Returns
/// Tuple of (thread_id, channel, pid) or error message
pub fn spawn_process_with_channel(
    path: &str,
    args: Option<&[&str]>,
    stdin: Option<&[u8]>,
) -> Result<(usize, Arc<ProcessChannel>, Pid), String> {
    spawn_process_with_channel_cwd(path, args, None, stdin, None)
}

/// Spawn a process on a user thread with a channel for I/O and specified cwd
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
/// * `cwd` - Optional current working directory (defaults to "/")
///
/// # Returns
/// Tuple of (thread_id, channel, pid) or error message
pub fn spawn_process_with_channel_cwd(
    path: &str,
    args: Option<&[&str]>,
    env: Option<&[String]>,
    stdin: Option<&[u8]>,
    cwd: Option<&str>,
) -> Result<(usize, Arc<ProcessChannel>, Pid), String> {
    spawn_process_with_channel_ext(path, args, env, stdin, cwd, 0)
}

/// Extended version of spawn_process_with_channel
pub fn spawn_process_with_channel_ext(
    path: &str,
    args: Option<&[&str]>,
    env: Option<&[String]>,
    stdin: Option<&[u8]>,
    cwd: Option<&str>,
    box_id: u64,
) -> Result<(usize, Arc<ProcessChannel>, Pid), String> {
    if crate::threading::user_threads_available() == 0 {
        return Err("No available user threads for process execution".into());
    }

    // Reject new processes under memory pressure to prevent OOM cascade
    if (runtime().is_memory_low)() {
        return Err("Kernel memory low, cannot spawn new process".into());
    }

    // If the box has a namespace with mounts (SubdirFs at /), activate a
    // per-thread namespace override so that runtime().read_file and
    // resolve_symlinks go through the container's mount table.
    let container_ns = if box_id != 0 {
        (runtime().get_box_namespace)(box_id)
    } else {
        None
    };
    let use_ns_override = container_ns.as_ref().is_some_and(|ns| !ns.mount.lock().is_empty());

    if use_ns_override {
        (runtime().set_spawn_namespace)(container_ns.as_ref().unwrap().clone());
    }

    let resolved = (runtime().resolve_symlinks)(path);
    let elf_path = &resolved;

    let mut full_args = Vec::new();
    full_args.push(path.to_string());
    if let Some(arg_slice) = args {
        for arg in arg_slice {
            full_args.push(arg.to_string());
        }
    }

    let mut full_env = match env {
        Some(e) if !e.is_empty() => e.to_vec(),
        _ => DEFAULT_ENV.iter().map(|s| String::from(*s)).collect(),
    };

    if box_id != 0 && !full_env.iter().any(|e| e.starts_with("HOSTNAME=")) {
        if let Some(name) = get_box_name(box_id) {
            let hostname: String = core::iter::once("box-")
                .flat_map(|s| s.chars())
                .chain(name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }))
                .collect();
            full_env.push(format!("HOSTNAME={hostname}"));
        }
    }

    let mut process = match (runtime().read_file)(elf_path) {
        Ok(elf_data) => {
            let result = Process::from_elf(elf_path, &full_args, &full_env, &elf_data, None);
            if use_ns_override { (runtime().clear_spawn_namespace)(); }
            result.map_err(|e| format!("Failed to load ELF: {}", e))?
        }
        Err(_) => {
            let file_size = (runtime().file_size)(elf_path)
                .map_err(|e| {
                    if use_ns_override { (runtime().clear_spawn_namespace)(); }
                    format!("Failed to stat {}: {}", elf_path, e)
                })? as usize;
            let result = Process::from_elf_path(elf_path, elf_path, file_size, &full_args, &full_env, None);
            if use_ns_override { (runtime().clear_spawn_namespace)(); }
            result.map_err(|e| format!("Failed to load ELF: {}", e))?
        }
    };

    // Always create a fresh channel per spawned process.
    // Reusing the parent's channel would cause the child's set_exited() call
    // to contaminate the parent's channel, leaking exit codes.
    let channel = Arc::new(ProcessChannel::new());
    
    // Seed the channel with initial stdin data if provided.
    // Empty stdin (Some(b"")) keeps stdin open so sys_write enables ONLCR
    // translation — use this for subprocesses that need terminal-style output.
    if let Some(data) = stdin {
        if !data.is_empty() {
            channel.write_stdin(data);
            channel.close_stdin();
        }
    }

    // Set the channel in the process struct (UNIFIED I/O)
    process.channel = Some(channel.clone());

    // Inherit terminal state from caller if available
    if let Some(shared_state) = current_terminal_state() {
        if config().syscall_debug_info_enabled {
            log::debug!("[Process] Inheriting shared terminal state at {:p} for PID {}", Arc::as_ptr(&shared_state), process.pid);
        }
        process.terminal_state = shared_state;
        
        // Auto-delegate foreground to the new process.
        // For interactive spawns, the child should start in the foreground.
        let pid_to_delegate = process.pid;
        process.terminal_state.lock().foreground_pgid = pid_to_delegate;
    } else {
        if config().syscall_debug_info_enabled {
            log::debug!("[Process] NO shared terminal state found for caller thread {}, using default for PID {}", crate::threading::current_thread_id(), process.pid);
        }
    }

    // Save arguments in process struct for ProcessInfo page
    process.args = if let Some(arg_slice) = args {
        arg_slice.iter().map(|s| String::from(*s)).collect()
    } else {
        Vec::new()
    };

    // Set up stdin if provided
    if let Some(data) = stdin {
        process.set_stdin(data);
    }
    
    // Set up cwd if provided
    if let Some(dir) = cwd {
        process.set_cwd(dir);
    }

    // Set up isolation context (Inherit from caller by default)
    let (caller_box_id, caller_namespace) = match read_current_pid() {
        Some(pid) => {
            if let Some(proc) = lookup_process(pid) {
                (proc.box_id, proc.namespace.clone())
            } else {
                (0, akuma_isolation::global_namespace())
            }
        }
        None => (0, akuma_isolation::global_namespace()),
    };

    if box_id != 0 {
        process.box_id = box_id;
        if let Some(ns) = (runtime().get_box_namespace)(box_id) {
            process.namespace = ns;
        } else {
            process.namespace = caller_namespace;
        }
    } else {
        process.box_id = caller_box_id;
        process.namespace = caller_namespace;
    }

    if config().syscall_debug_info_enabled {
        log::debug!("[Process] Spawning {} (box_id={}, ns_id={})", path, process.box_id, process.namespace.id);
    }

    // Set spawner PID (the process that called spawn, if any)
    // This is used by procfs to control who can write to stdin
    process.spawner_pid = read_current_pid();
    
    // Get the PID before boxing
    let pid = process.pid;

    // Box the process for heap allocation (fallible to avoid kernel panic on OOM)
    let boxed_process = Box::try_new(process)
        .map_err(|_| format!("Failed to allocate Process struct for {path}"))?;

    // CRITICAL: Register the process in the table immediately.
    // This ensures that lookup_process(pid) works as soon as this function returns,
    // allowing reattach() to succeed without races.
    register_process(pid, boxed_process);

    // Register the channel for the thread ID placeholder (0 for now, will be updated)
    // Actually, current_channel() now uses the field in Process struct, so this is mostly for legacy.
    register_channel(0, channel.clone());

    // Spawn on a user thread
    let thread_id = crate::threading::spawn_user_thread_fn_for_process(move || {
        let tid = crate::threading::current_thread_id();
        
        // Update thread_id in the registered process
        if let Some(p) = lookup_process(pid) {
            p.thread_id = Some(tid);
            
            // Move the channel registration to the correct TID
            remove_channel(0);
            register_channel(tid, p.channel.as_ref().unwrap().clone());
            
            // Execute the process (already in the table)
            run_registered_process(pid);
        } else {
            log::debug!("[Process] FATAL: PID {} disappeared during spawn", pid);
            loop { crate::threading::yield_now(); }
        }
    })
    .map_err(|e| format!("Failed to spawn thread: {}", e))?;

    // Set the thread ID in the process table entry for the parent to see immediately
    if let Some(p) = lookup_process(pid) {
        p.thread_id = Some(thread_id);
    }

    Ok((thread_id, channel, pid))
}

/// Execute a process that is already registered in the PROCESS_TABLE
fn run_registered_process(pid: Pid) -> ! {
    let proc = lookup_process(pid).expect("Process not found in run_registered_process");
    
    // Prepare the process (set state, write process info page)
    proc.prepare_for_execution();
    
    // Activate the user address space (sets TTBR0)
    proc.address_space.activate();

    // Now safe to enable IRQs - TTBR0 is set to user tables
    (runtime().enable_irqs)();

    // Enter user mode via ERET - this never returns
    unsafe {
        enter_user_mode(&proc.context);
    }
}

/// Execute a binary asynchronously and return its output when complete
///
/// Spawns the process on a user thread and polls for completion,
/// yielding to other async tasks while waiting. Returns the buffered
/// output when the process exits.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
///
/// # Returns
/// Tuple of (exit_code, stdout_data) or error message
pub async fn exec_async(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>) -> Result<(i32, Vec<u8>), String> {
    exec_async_cwd(path, args, None, stdin, None).await
}

/// exec_async with explicit cwd and env
pub async fn exec_async_cwd(path: &str, args: Option<&[&str]>, env: Option<&[String]>, stdin: Option<&[u8]>, cwd: Option<&str>) -> Result<(i32, Vec<u8>), String> {

    // Spawn process with channel and cwd
    let (thread_id, channel, _pid) = spawn_process_with_channel_cwd(path, args, env, stdin, cwd)?;

    // For non-interactive execution, if no stdin was provided, mark it as closed
    if stdin.is_none() {
        channel.close_stdin();
    }

    // Wait for process to complete
    // Each iteration yields once (returns Pending) so block_on can yield to scheduler
    loop {
        // Check if process has exited or was interrupted
        if channel.has_exited() || crate::threading::is_thread_terminated(thread_id) {
            break;
        }

        if channel.is_interrupted() {
            break;
        }

        // Yield once - this returns Pending, block_on yields, then we get polled again
        YieldOnce::new().await;
    }

    // Collect all output
    let output = channel.read_all();
    let exit_code = if channel.is_interrupted() && !channel.has_exited() {
        130 // Interrupted exit code
    } else {
        channel.exit_code()
    };

    // Final cleanup
    crate::threading::cleanup_terminated();

    Ok((exit_code, output))
}

/// Get the process channel for a running process by thread ID
///
/// Used by the SSH shell to get a handle for interrupting a process.
pub fn get_process_channel(thread_id: usize) -> Option<Arc<ProcessChannel>> {
    get_channel(thread_id)
}

/// Execute a binary with streaming output to an async writer
///
/// Spawns the process on a user thread and streams output to the
/// provided writer as it becomes available. This allows real-time
/// output display while keeping SSH responsive.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
/// * `output` - Async writer to stream output to
///
/// # Returns
/// Exit code or error message
pub async fn exec_streaming<W>(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>, output: &mut W) -> Result<i32, String>
where
    W: embedded_io_async::Write,
{
    exec_streaming_cwd(path, args, None, stdin, None, output).await
}

/// exec_streaming with explicit cwd and env
pub async fn exec_streaming_cwd<W>(path: &str, args: Option<&[&str]>, env: Option<&[String]>, stdin: Option<&[u8]>, cwd: Option<&str>, output: &mut W) -> Result<i32, String>
where
    W: embedded_io_async::Write,
{
    // Spawn process with channel and cwd
    let (thread_id, channel, _pid) = spawn_process_with_channel_cwd(path, args, env, stdin, cwd)?;

    // For non-interactive streaming, if no stdin was provided, mark it as closed
    if stdin.is_none() {
        channel.close_stdin();
    }

    // Stream output until process exits
    loop {
        // Read available data
        if let Some(data) = channel.try_read() {
            if let Err(_e) = output.write_all(&data).await {
                // Writer failed, likely connection closed
                break;
            }
        }

        // Check if process has exited
        if channel.has_exited() || crate::threading::is_thread_terminated(thread_id) {
            break;
        }

        if channel.is_interrupted() {
            break;
        }

        // Yield to scheduler
        YieldOnce::new().await;
    }

    // Drain remaining output
    if let Some(data) = channel.try_read() {
        let _ = output.write_all(&data).await;
    }

    let exit_code = if channel.is_interrupted() && !channel.has_exited() {
        130 // Interrupted
    } else {
        channel.exit_code()
    };

    // Final cleanup
    crate::threading::cleanup_terminated();

    Ok(exit_code)
}

/// Reattach I/O from a caller process (or kernel) to a target PID
pub fn reattach_process_ext(caller_pid: Option<Pid>, target_pid: Pid) -> Result<(), &'static str> {
    // 1. Validate hierarchy permissions
    let (caller_box_id, channel) = if let Some(pid) = caller_pid {
        let caller = lookup_process(pid).ok_or("Caller not found")?;
        (caller.box_id, caller.channel.clone())
    } else {
        // Kernel caller (e.g. built-in SSH shell)
        // System threads use thread-ID based channel lookup
        let tid = crate::threading::current_thread_id();
        let ch = get_channel(tid).ok_or("Kernel thread has no channel")?;
        (0, Some(ch)) // Kernel is Box 0
    };

    let target_box_id = {
        let target = lookup_process(target_pid).ok_or("Target not found")?;
        target.box_id
    };

    let mut allowed = false;
    if caller_box_id == 0 {
        allowed = true; // Host/Kernel can reattach anything
    } else if target_box_id == caller_box_id {
        allowed = true; // Same box
    } else if let Some(pid) = caller_pid {
        // Check if caller created the target's box (child box)
        if let Some(info) = get_box_info(target_box_id) {
            if info.creator_pid == pid {
                allowed = true;
            }
        }
    }

    if !allowed {
        return Err("Permission denied: cannot reattach process outside hierarchy");
    }

    // 2. Perform the delegation
    if let Some(pid) = caller_pid {
        let caller = lookup_process(pid).ok_or("Caller not found")?;
        caller.delegate_pid = Some(target_pid);
    } else {
        // For kernel caller, we don't have a 'Process' struct to set delegate_pid,
        // but we still want to link the channel to the target.
    }

    // Target process now uses caller's output channel
    {
        let target = lookup_process(target_pid).ok_or("Target not found")?;
        target.channel = channel;
    }

    if config().syscall_debug_info_enabled {
        log::debug!("[Process] Reattached (caller={:?}) -> PID {}", caller_pid, target_pid);
    }

    Ok(())
}

/// Reattach I/O from the current process to a target PID
pub fn reattach_process(target_pid: Pid) -> Result<(), &'static str> {
    let caller_pid = read_current_pid(); // Can be None for kernel threads
    reattach_process_ext(caller_pid, target_pid)
}

