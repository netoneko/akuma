//! A process whose address space its kernel built: [`Process::adopted`].
//!
//! The tokio kernels (`akuma-tokio-kernel-{amd64,aarch64}`) load an image into a
//! page-table root their own task record owns, then register a `Process` over it
//! so the shared paths (glue's fd table, `fork_process`, `wait4`, `/proc`) find a
//! caller. The `Process` holds a `new_shared` view of that root: a second owner
//! would free it twice. Moved out of the amd64 half (B21 A5.6) so both machines
//! register their first process the same way.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use spinning_top::Spinlock;

use super::{Creds, Pid, Process, ProcessMemory, SharedFdTable};

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
        Some(Self::fresh(super::FreshProcess {
            pid,
            parent_pid,
            name: String::from(name),
            args: Vec::new(),
            address_space: space,
            entry,
            sp,
            brk: 0,
            // The kernel's task record owns brk/mmap/stack truth; these feed
            // shared readers only.
            memory: ProcessMemory::new(0, 0, 0, 0),
            process_info_phys: 0,
            mmap_regions: Vec::new(),
            fds: Arc::new(SharedFdTable::with_stdio()),
            thread_id: Some(tid),
            terminal_state: Arc::new(Spinlock::new(akuma_tty::TerminalState::default())),
            creds: Creds::ROOT,
            channel: None,
        }))
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
