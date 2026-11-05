//! A process whose address space its kernel built: [`Process::adopted`].
//!
//! The tokio kernels (`akuma-tokio-kernel-{amd64,aarch64}`) load an image into a
//! page-table root their own task record owns, then register a `Process` over it
//! so the shared paths (glue's fd table, `fork_process`, `wait4`, `/proc`) find a
//! caller. The `Process` holds a `new_shared` view of that root: a second owner
//! would free it twice. Moved out of the amd64 half (B21 A5.6) so both machines
//! register their first process the same way.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize};

use spinning_top::Spinlock;

use super::{
    AssistedSpinlock, AtomicProcessState, Creds, LazyRegionMap, Pid, ProcAddressSpace, Process, ProcessImage,
    ProcessMemory, ProcessState, ProcessSyscallStats, SharedCreds, SharedFdTable, SharedSignalTable, StdioBuffer,
    UserContext,
};

impl Process {
    /// A process over the root `root_pa`, which its kernel's task record owns.
    ///
    /// `pid` is its own group leader, root-credentialed, with the stdio triple
    /// and no I/O channel (its stdio is the kernel's console), on thread `tid`.
    /// `entry`/`sp` seed the saved user context. `None` when the root view
    /// cannot be made (the address-space allocator is exhausted).
    #[must_use]
    pub fn adopted(pid: Pid, parent_pid: Pid, name: &str, root_pa: usize, entry: usize, sp: usize, tid: usize) -> Option<Self> {
        let space = crate::mmu::UserAddressSpace::new_shared(root_pa)?;
        Some(Self {
            pid,
            pgid: pid,
            tgid: pid,
            state: AtomicProcessState::new(ProcessState::Ready),
            address_space: ProcAddressSpace::new(space),
            parent_pid,
            brk: AtomicUsize::new(0),
            initial_brk: AtomicUsize::new(0),
            entry_point: AtomicUsize::new(entry),
            // The kernel's task record owns brk/mmap/stack truth; these feed
            // shared readers only.
            memory: ProcessMemory::new(0, 0, 0, 0),
            process_info_phys: AtomicUsize::new(0),
            image: Spinlock::new(ProcessImage {
                name: String::from(name),
                exe: String::from(name),
                args: Vec::new(),
                context: UserContext::new(entry, sp),
                text: akuma_primitives::TextPin::none(),
            }),
            cwd: String::from("/"),
            root: String::from("/"),
            stdin: Arc::new(Spinlock::new(StdioBuffer::new())),
            stdout: Arc::new(Spinlock::new(StdioBuffer::new())),
            exited: AtomicBool::new(false),
            exit_code: AtomicI32::new(0),
            dynamic_page_tables: Vec::new(),
            mmap_regions: AssistedSpinlock::new(Vec::new()),
            lazy_regions: Spinlock::new(LazyRegionMap::new()),
            fds: Arc::new(SharedFdTable::with_stdio()),
            thread_id: Some(tid),
            spawner_pid: None,
            ctty: AtomicU32::new(0),
            terminal_state: Arc::new(Spinlock::new(akuma_tty::TerminalState::default())),
            box_id: 0,
            creds: Arc::new(SharedCreds::new(Creds::ROOT)),
            namespace: akuma_isolation::global_namespace(),
            channel: None,
            delegate_pid: None,
            grabbed_by: None,
            clear_child_tid: AtomicU64::new(0),
            robust_list_head: 0,
            robust_list_len: 0,
            signal_actions: Arc::new(SharedSignalTable::new()),
            signal_mask: 0,
            fault_mutex: Spinlock::new(BTreeMap::new()),
            sigaltstack_sp: AtomicU64::new(0),
            sigaltstack_flags: AtomicI32::new(2), // SS_DISABLE
            sigaltstack_size: AtomicU64::new(0),
            start_time_us: (crate::runtime::runtime().uptime_us)(),
            current_syscall: AtomicU64::new(!0),
            last_syscall: AtomicU64::new(0),
            syscall_stats: ProcessSyscallStats::new(),
        })
    }

    /// [`Process::adopted`], registered, with `tid` mapped to it. `false` when
    /// the root view could not be made.
    #[must_use]
    pub fn register_adopted(pid: Pid, parent_pid: Pid, name: &str, root_pa: usize, entry: usize, sp: usize, tid: usize) -> bool {
        let Some(p) = Self::adopted(pid, parent_pid, name, root_pa, entry, sp, tid) else {
            return false;
        };
        super::register_process(pid, Box::new(p));
        super::thread_pid_map_insert(tid, pid);
        true
    }
}
