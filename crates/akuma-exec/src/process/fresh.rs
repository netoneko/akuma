//! A process that did not fork from one: [`Process::fresh`].
//!
//! Three kernels make one. The AArch64 kernel loads an ELF (`Process::from_elf`,
//! `image.rs`); the tokio kernels adopt a root their task record owns
//! (`Process::adopted`, `adopted.rs`); the amd64 kernel registers what its own
//! loader built (`register_exec_process`, `amd64/src/usermode.rs`). Each used to
//! spell the whole 45-field `Process` literal, and 28 of those initialisers were
//! the same text in all three — a new process's signal table, sigaltstack,
//! robust list, cwd, namespace, stdio buffers, counters. They are written once,
//! here; [`FreshProcess`] is what the three actually differ in.
//!
//! The struct literal still names every field, so a field added to `Process`
//! fails to compile here until it is given a fresh-process value — once, for all
//! three kernels, instead of three times with room for the values to disagree.
//! The clone family (`Process::inherit_from`, `mod.rs`) is not a fresh process
//! and keeps its own literal: nearly every field it sets comes from the parent.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize};

use akuma_mmap::MmapRegion;
use spinning_top::Spinlock;

use super::{
    AssistedSpinlock, AtomicProcessState, Creds, LazyRegionMap, Pid, ProcAddressSpace, Process, ProcessChannel,
    ProcessImage, ProcessMemory, ProcessState, ProcessSyscallStats, SharedCreds, SharedFdTable, SharedSignalTable,
    StdioBuffer, UserContext,
};

/// Everything that differs between two fresh processes.
///
/// `name` is both `ProcessImage::name` and `::exe`; `brk` is both the current
/// and the initial break; `entry`/`sp` seed the saved user context and
/// `entry` is also `Process::entry_point`. Everything else a new process starts
/// with is [`Process::fresh`]'s and is the same for every kernel.
pub struct FreshProcess {
    pub pid: Pid,
    pub parent_pid: Pid,
    pub name: String,
    pub args: Vec<String>,
    pub address_space: crate::mmu::UserAddressSpace,
    pub entry: usize,
    pub sp: usize,
    pub brk: usize,
    pub memory: ProcessMemory,
    /// Physical address of the read-only process-info page, or 0 when the
    /// kernel maps none (amd64, the tokio kernels).
    pub process_info_phys: usize,
    pub mmap_regions: Vec<MmapRegion>,
    pub lazy_regions: LazyRegionMap,
    pub fds: Arc<SharedFdTable>,
    /// The scheduler slot that runs it, when one is already known.
    pub thread_id: Option<usize>,
    pub terminal_state: Arc<Spinlock<akuma_tty::TerminalState>>,
    pub creds: Creds,
    pub channel: Option<Arc<ProcessChannel>>,
}

impl Process {
    /// A new process from `seed`: its own group leader (`pgid == tgid == pid`),
    /// `Ready`, at `/`, no controlling terminal, default signal dispositions and
    /// an empty mask, `SS_DISABLE` on the alternate stack, in box 0's namespace,
    /// started now.
    #[must_use]
    pub fn fresh(seed: FreshProcess) -> Self {
        let FreshProcess {
            pid,
            parent_pid,
            name,
            args,
            address_space,
            entry,
            sp,
            brk,
            memory,
            process_info_phys,
            mmap_regions,
            lazy_regions,
            fds,
            thread_id,
            terminal_state,
            creds,
            channel,
        } = seed;
        Self {
            pid,
            pgid: pid,
            tgid: pid, // group leader = self
            state: AtomicProcessState::new(ProcessState::Ready),
            address_space: ProcAddressSpace::new(address_space),
            image: Spinlock::new(ProcessImage {
                exe: name.clone(),
                name,
                args,
                context: UserContext::new(entry, sp),
                text: akuma_primitives::TextPin::none(),
            }),
            parent_pid,
            brk: AtomicUsize::new(brk),
            initial_brk: AtomicUsize::new(brk),
            entry_point: AtomicUsize::new(entry),
            memory,
            process_info_phys: AtomicUsize::new(process_info_phys),
            cwd: String::from("/"),
            root: String::from("/"),
            stdin: Arc::new(Spinlock::new(StdioBuffer::new())),
            stdout: Arc::new(Spinlock::new(StdioBuffer::new())),
            exited: AtomicBool::new(false),
            exit_code: AtomicI32::new(0),
            dynamic_page_tables: Vec::new(),
            mmap_regions: AssistedSpinlock::new(mmap_regions),
            lazy_regions: Spinlock::new(lazy_regions),
            fds,
            thread_id,
            // Set when spawned by another process.
            spawner_pid: None,
            ctty: AtomicU32::new(0),
            terminal_state,
            box_id: 0,
            creds: Arc::new(SharedCreds::new(creds)),
            namespace: akuma_isolation::global_namespace(),
            channel,
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
        }
    }
}
