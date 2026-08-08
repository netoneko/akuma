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
pub mod diag;
pub mod lifecycle;
pub mod bkl_guard;
pub mod reclaim;

pub use lifecycle::LifecycleGuard;
pub use bkl_guard::{process_bkl_drop_enabled, set_process_bkl_drop_enabled, ProcessBklGuard};

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
use alloc::collections::BTreeSet;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, AtomicBool, AtomicUsize, Ordering};

/// Rate-limit counter for the `[KTG-MISMATCH]` tripwire in [`kill_thread_group`].
static KTG_MISMATCHES: AtomicUsize = AtomicUsize::new(0);

/// Rate-limit counter for the `[KTG]` caller trace in [`kill_thread_group`].
static KTG_TRACES: AtomicUsize = AtomicUsize::new(0);

/// Rate-limit counter for the `[KTG-STALE]` tripwire: a grace-expired hard kill
/// aimed at a thread slot that had been recycled to an unrelated process.
/// Only the `kernel_smp_shared` grace-wait has that window, so only it reports.
#[cfg(kernel_smp_shared)]
static KTG_STALE_SKIPS: AtomicUsize = AtomicUsize::new(0);

/// Rate-limit counter for the `[KTG-STALE-CH]` tripwire: PHASE 2 about to evict
/// and exit-stamp a per-tid channel whose slot has been recycled to an unrelated
/// process. Unlike `KTG_STALE_SKIPS` this is not smp-shared-only: PHASE 2 runs
/// in every build, and the snapshot can be stale on entry (a sibling that died
/// before the group kill started keeps its recorded `thread_id`).
static KTG_STALE_CH_SKIPS: AtomicUsize = AtomicUsize::new(0);

/// Rate-limit counter for the `[ORPHAN-KILL]` tripwire in
/// [`kill_children_whose_parent_in`]: a forked child (e.g. a linker) being
/// force-reaped because its parent is exiting while the child is still alive.
/// Investigating the truncated/empty linker-output failure
/// (docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md §4) — a
/// child killed here mid-write would explain a `0666`, never-`chmod`-to-`0777`
/// output exactly.
static ORPHAN_KILL_TRACES: AtomicUsize = AtomicUsize::new(0);

pub static FORK_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
use spinning_top::Spinlock;

use crate::elf_loader::{self, ElfError};
use crate::mmu::{self, UserAddressSpace};
use crate::runtime::{PhysFrame, FrameSource, runtime, config, with_irqs_disabled};
use akuma_terminal as terminal;


use self::image::{compute_heap_lazy_size, LAZY_STACK_MAX};

pub(crate) struct FmtBuf<'a> { buf: &'a mut [u8], pos: &'a mut usize }
impl core::fmt::Write for FmtBuf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let avail = self.buf.len() - *self.pos;
        let n = bytes.len().min(avail);
        self.buf[*self.pos..*self.pos + n].copy_from_slice(&bytes[..n]);
        *self.pos += n;
        Ok(())
    }
}

/// Is fork/exec/thread-spawn lifecycle tracing on? (`SYSCALL_DEBUG_INFO_ENABLED`)
///
/// These traces are step-by-step markers left over from the fork and
/// thread-spawn investigations. They were unconditional, which put ~20 serial
/// lines on every `fork()`, 5 on every `execve`, and 2 on every thread spawn —
/// measured at ~3.5 K lines from a single short boot plus a few probes. A `-j4`
/// in-VM cargo build does all three continuously, so the UART cost is large and,
/// worse, it perturbs the timing of exactly the paths where the remaining
/// thread-spawn race lives. Keep them (they are genuinely useful when a fork
/// wedges) but behind the flag.
#[inline]
pub(crate) fn lifecycle_trace_on() -> bool {
    config().syscall_debug_info_enabled
}

/// Emit one lifecycle trace line, when [`lifecycle_trace_on`].
#[inline]
pub(crate) fn lifecycle_trace(s: &str) {
    if lifecycle_trace_on() {
        (runtime().print_str)(s);
    }
}

/// Page count to copy `len` bytes one page at a time. Returns [`None`] if the
/// computation overflows `usize` (would otherwise wrap and loop forever).
#[must_use]
pub fn fork_page_count_for_len(len: usize) -> Option<usize> {
    if len == 0 {
        return Some(0);
    }
    len.checked_add(mmu::PAGE_SIZE - 1)?.checked_div(mmu::PAGE_SIZE)
}

/// Upper bound on **brk/heap** pages copied eagerly during [`fork_process`].
/// Without this, a huge `brk` (common for Go) can spend minutes in the copy
/// loop and appear as a full-system hang. Raise if legitimate workloads exceed
/// this (clear error instead of silent multi‑minute copy).
const MAX_FORK_BRK_COPY_PAGES: usize = 8 * 1024 * 1024; // 32 GiB of pages

const FORK_COPY_PROGRESS_INTERVAL_PAGES: usize = 8192;

/// Pages handled per `as_lock` hold in [`fork_process`]'s CoW share/demote pass —
/// the `no-bkl-process` carve-out's chunk size (see [`bkl_guard`] constraint 2).
///
/// Each hold masks local IRQs, so it must stay short and bounded: the whole point of
/// the carve-out is that the BKL is dropped for the copy, and a hold spanning the
/// entire copy would starve this core's timer for milliseconds *and* reopen the AB-BA
/// wedge (this core holding `as_lock` while a nested IRQ hard-spins for the BKL,
/// against a peer holding the BKL and waiting on `as_lock` in `munmap`). Each hold
/// also carries fixed overhead (lock, DSB, TLB range invalidate), so it must not be
/// tiny either. 64 pages = 256 KiB of VA per hold, and stays well under
/// `flush_tlb_range_all_asid`'s 512-page full-flush threshold so the per-chunk
/// invalidate remains a targeted `tlbi vaae1is` sweep.
pub const FORK_AS_CHUNK_PAGES: usize = 64;

/// Sets [`FORK_IN_PROGRESS`] for its lifetime and clears it on **every** exit path.
///
/// The flag only drives the timer handler's `[TMR]` log frequency (`src/timer.rs`), but
/// the old inline `store(true)`/`store(false)` pair leaked it permanently whenever the
/// copy loop took a `?` early-return (OOM mid-fork), leaving the console logging 10×
/// for the rest of the boot. RAII also pins the ordering the `no-bkl-process` carve-out
/// wants: declared BEFORE [`ProcessBklGuard`], so the flag is set while still BKL-held
/// and cleared only after the BKL has been re-acquired (locals drop in reverse
/// declaration order).
struct ForkInProgressGuard;

impl ForkInProgressGuard {
    #[inline]
    fn new() -> Self {
        FORK_IN_PROGRESS.store(true, Ordering::Release);
        Self
    }
}

impl Drop for ForkInProgressGuard {
    #[inline]
    fn drop(&mut self) {
        FORK_IN_PROGRESS.store(false, Ordering::Release);
    }
}

/// CoW-share one VA range from a parent address space into a child, **and demote the
/// parent's copy of it to read-only** in the same pass.
///
/// For each mapped page in `[va_start, va_start + len)`: increment the CoW refcount,
/// map the same PA into `child_as` as RO (preserving UXN/PXN from the parent's PTE),
/// track the frame in the child's address space, and drop the parent's PTE to RO so the
/// parent's own later writes fault too. Returns the number of pages shared.
///
/// The work is split across a chunked [`Process::as_lock`] hold (see
/// [`FORK_AS_CHUNK_PAGES`]) into two phases, and the split is what makes
/// [`fork_process`]'s `no-bkl-process` window sound:
///
/// - **Phase A, under `as_lock`** — snapshot the parent PTEs, take the CoW reference,
///   demote, invalidate. These four MUST be one atomic step against a peer core's CoW
///   fault on this address space. Split them and this race is live: fork reads a PTE
///   naming frame X, a peer's fault breaks X (`cow_ref_dec` → 0 → frame freed, VA
///   remapped to Y), and fork then `cow_ref_inc`s the freed X and hands it to the child.
///   The fault handler performs its own break under this same lock, so one hold
///   serializes the two. The phase is allocation-free apart from the refcount map:
///   `scratch` must be pre-reserved to `FORK_AS_CHUNK_PAGES` by the caller, outside
///   every lock, so the collect reuses capacity instead of growing under an IRQ mask.
///
/// - **Phase B, unlocked** — build the child's page tables. This is the expensive,
///   allocating half (page-table frames, the user-frame Vec) and must stay OUT of an
///   IRQ-masked hold. The child is private stack-local state until `fork_process`'s
///   step 8, so no other core can observe it regardless.
///
/// The demote used to be a separate second walk over every range after all sharing was
/// done; merging it here is what makes the per-page transition atomic, and it drops a
/// full redundant page-table walk of the parent.
///
/// # Safety-relevant preconditions
/// `parent_l0` must be the live L0 root of the address space that `as_lock` guards —
/// for a `CLONE_THREAD` group that is the **leader's** lock, resolved by
/// [`address_space_owner_pid_for_fault`], not the calling thread's.
pub fn cow_share_and_demote_range(
    parent_l0: *const u64,
    as_lock: &Spinlock<()>,
    va_start: usize,
    len: usize,
    child_as: &mut mmu::UserAddressSpace,
    scratch: &mut alloc::vec::Vec<(usize, usize, u64)>,
    label: &str,
) -> Result<usize, &'static str> {
    let pages = fork_page_count_for_len(len).ok_or("CoW share page count overflow")?;
    let mut count = 0usize;
    let mut done = 0usize;
    while done < pages {
        let chunk = (pages - done).min(FORK_AS_CHUNK_PAGES);
        let chunk_va = done
            .checked_mul(mmu::PAGE_SIZE)
            .and_then(|off| va_start.checked_add(off))
            .ok_or("CoW share VA overflow")?;

        // ── Phase A: parent page table, under `as_lock`, IRQs masked ──
        {
            #[cfg(kernel_smp_shared)]
            let _asg = AsLockHold::new(as_lock);
            #[cfg(not(kernel_smp_shared))]
            let _ = as_lock;
            mmu::collect_mapped_pages_with_flags_into(parent_l0, chunk_va, chunk, scratch);
            for (_, pa, _) in scratch.iter() {
                // Inserts with count=2 on first share.
                (runtime().cow_ref_inc)(*pa);
            }
            // SAFETY: `parent_l0` is the live L0 root of the address space this
            // `as_lock` guards (see the preconditions above).
            unsafe {
                mmu::demote_range_to_ro(parent_l0.cast_mut(), chunk_va, chunk);
            }
            // Invalidate before releasing the lock so a sibling thread on a peer core
            // can't keep writing through a stale RW TLB entry for a page the child now
            // references. (The old code demoted every range and then flushed once at
            // the very end; with the demote merged into the share, that single trailing
            // flush would leave this window open for the whole copy instead of for one
            // chunk.) `chunk` <= FORK_AS_CHUNK_PAGES keeps this a targeted
            // `tlbi vaae1is` sweep, under `flush_tlb_range_all_asid`'s full-flush
            // threshold.
            mmu::flush_tlb_range_all_asid(chunk_va, chunk);
        }

        // ── Phase B: child page table, unlocked, BKL-free ──
        for &(va, pa, pte_flags) in scratch.iter() {
            // Force AP to RO, preserving UXN/PXN from the original PTE
            let child_flags = (pte_flags & !(mmu::flags::AP_RO_ALL)) | mmu::flags::AP_RO_ALL;
            child_as.map_page(va, pa, child_flags)?;
            child_as.track_user_frame(PhysFrame::new(pa));
        }
        count += scratch.len();
        done += chunk;
    }
    if config().syscall_debug_info_enabled && count > 0 {
        log::debug!("[fork-cow] {} shared {} pages", label, count);
    }
    Ok(count)
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
            table::unregister_process(pid);
        }
    }
    // No fallback scan needed: spawn_process_with_channel now registers
    // in THREAD_PID_MAP, so the primary path above handles all processes.
}

// Box registry re-exports
pub use crate::box_registry::{
    BoxInfo, register_box, unregister_box, list_boxes,
    find_box_by_name, get_box_name, get_box_info, find_primary_box,
    init_box_registry, registry_snapshot,
};

/// Box permission checks. Every syscall that crosses a box boundary — register,
/// kill, spawn-into, set-stack — gates on these; see
/// `docs/reference/subsystems/containers.md`.
pub use crate::box_registry::access as box_access;

/// Write data to a process's stdin
pub fn write_to_process_stdin(pid: Pid, data: &[u8]) -> Result<(), &'static str> {
    let proc = children::lookup_process_shared(pid).ok_or("Process not found")?;
    
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

/// Signal end-of-input on a process's stdin (the SSH client closed its stdin
/// via CHANNEL_EOF). Marks the channel's stdin closed AND wakes a reader parked
/// in `read(stdin)` so it re-checks and returns 0 (EOF). Without the wake, a
/// blocked reader (e.g. `cat`) sleeps forever on `schedule_blocking(u64::MAX)` —
/// the bug that made `exec cat` hang for the full idle timeout. Mirrors the wake
/// path in `write_to_process_stdin`.
pub fn close_process_stdin(pid: Pid) -> Result<(), &'static str> {
    let proc = children::lookup_process_shared(pid).ok_or("Process not found")?;

    if let Some(target_pid) = proc.delegate_pid {
        return close_process_stdin(target_pid);
    }

    if let Some(ref channel) = proc.channel {
        channel.close_stdin();

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
    /// Thread group leader PID (like Linux's tgid).
    /// For the group leader: tgid == pid.
    /// For clone_thread children: tgid == parent's tgid.
    /// kill() delivers signals to all threads with matching tgid.
    /// exit_group() kills all threads with matching tgid.
    pub tgid: Pid,
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
    pub stdin: Arc<Spinlock<StdioBuffer>>,
    pub stdout: Arc<Spinlock<StdioBuffer>>,
    pub exited: bool,
    pub exit_code: i32,
    pub dynamic_page_tables: Vec<PhysFrame>,
    pub mmap_regions: Vec<MmapRegion>,
    /// Per-process demand-paged lazy regions, keyed by `start_va`. Owned here
    /// (not in a global table) so a `BTreeMap::insert` that OOMs under the lock
    /// cannot self-deadlock on teardown — the field drops inside `Process::drop`
    /// on the reclaim path, and `return_to_kernel*` no longer re-acquires it. See
    /// docs/archive/LAZY_REGION_TABLE_ALLOC_UNDER_LOCK_HANG.md and
    /// [`LazyRegionMap`].
    pub lazy_regions: Spinlock<LazyRegionMap>,
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
    /// Per-page demand-paging serialization slots: `page_va -> holder_thread_id`.
    /// Maps each in-flight faulting page to the thread currently resolving it so a
    /// sibling faulting the same page can detect a holder that died mid-fault (its
    /// RAII release guard never ran) and reclaim the slot instead of spinning
    /// forever. See [`fault_slot_acquire`]/[`fault_slot_release`].
    pub fault_mutex: Spinlock<BTreeMap<usize, usize>>,
    /// Serializes all access to [`Process::mmap_regions`] AND `Process::memory`'s
    /// mmap free-list (`ProcessMemory::free_regions`, via [`Process::vm_alloc_mmap`]/
    /// [`Process::vm_free_mmap`]) across the thread group. CLONE_VM threads share one
    /// address-space-owner Process, so concurrent `sys_mmap`/`munmap`/`mremap`
    /// (push/remove) and the page-fault eager fallback (iter) would otherwise race on
    /// the plain `Vec`s — a half-completed reallocation read by another thread
    /// corrupts it and hangs the kernel (observed under llama's burst of
    /// graph-buffer mmaps). All access goes through the `vm_*` helper methods, which
    /// hold this lock with IRQs disabled for a *pure Vec/bookkeeping operation only*
    /// — no allocation, file I/O, mapping, or frame-free is performed while it is
    /// held (those happen on the returned frames after the lock is released), so it
    /// can never be held across a yield.
    pub vm_lock: Spinlock<()>,
    /// Serializes hardware page-table mutation for this address space across cores
    /// under shared-kernel SMP (real M5b). The BKL-free user page-fault path takes
    /// this (instead of the BKL) for its short PTE-install window; every AS-mutating
    /// syscall (`mmap`/`munmap`/`mprotect`/`brk`/teardown/CoW-mark) takes it in
    /// addition to the BKL it already holds, so a concurrent fault on the same
    /// address space excludes correctly. Unlike [`Process::vm_lock`] (which guards
    /// only the `mmap_regions` `Vec` for pure Vec ops), this covers the raw
    /// page-table writes in `mmu::map_user_page*` / `AddressSpace::{un,}map_page`.
    /// Held only for short windows with preemption disabled — NEVER across alloc,
    /// block I/O, or a context switch (see [`Process::with_as_locked`] and
    /// docs/archive/SMP_SHARED_M5_FAULT_LOCK_PLAN.md). CLONE_VM members share the
    /// leader's, keyed by `tgid`, exactly like [`Process::fault_mutex`]. Lock order:
    /// `BKL > as_lock > {PMM, page_table_frames, user_frames, fault_mutex, ...}`.
    pub as_lock: Spinlock<()>,
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
    let pids: Vec<Pid> = table::collect_pids(|p| p.box_id == box_id);

    // 2. Kill each process
    for pid in pids {
        // kill_process handles unregistering and thread termination
        let _ = kill_process(pid);
    }

    // 3. Unregister the box from the global registry
    unregister_box(box_id);

    Ok(())
}

/// RAII hold of an address space's `as_lock` with IRQs disabled — see
/// [`Process::as_lock_hold`]. Field drop order (declaration order) releases the lock
/// before restoring DAIF. smp-shared only.
#[cfg(kernel_smp_shared)]
pub struct AsLockHold<'a> {
    _g: spinning_top::guard::SpinlockGuard<'a, ()>,
    _irq: crate::runtime::IrqGuard,
}

#[cfg(kernel_smp_shared)]
impl<'a> AsLockHold<'a> {
    #[inline]
    pub fn new(as_lock: &'a Spinlock<()>) -> Self {
        let _irq = crate::runtime::IrqGuard::new();
        let _g = as_lock.lock();
        AsLockHold { _g, _irq }
    }
}

impl Process {
    /// Run `f` with exclusive access to [`Process::mmap_regions`], serialized by
    /// [`Process::vm_lock`] with IRQs disabled. This is the ONLY sanctioned way to
    /// touch `mmap_regions` — it prevents the data race where CLONE_VM threads
    /// concurrently push/remove/iter the shared `Vec` and corrupt it (kernel hang
    /// under llama's graph-buffer mmap burst).
    ///
    /// `f` MUST perform only pure `Vec` operations (push/remove/iter/clone) and
    /// MUST NOT allocate frames, map pages, free frames, read files, or yield —
    /// return any frames to unmap/free and do that work AFTER this returns, with
    /// the lock released. Holding `vm_lock` across a yield/alloc would risk a
    /// single-core deadlock.
    pub fn vm_with_regions<R>(&self, f: impl FnOnce(&mut Vec<MmapRegion>) -> R) -> R {
        with_irqs_disabled(|| {
            let _g = self.vm_lock.lock();
            // SAFETY: `vm_lock` (held here) serializes every `vm_with_regions`
            // caller across the thread group, so this is the unique live
            // reference to `mmap_regions` for the closure's duration. Interior
            // mutability via raw pointer is required because the field is a plain
            // `Vec` and callers hold only `&Process` (the address-space owner is
            // already aliased `&mut` across CLONE_VM threads by `lookup_process`).
            let regions = unsafe {
                &mut *(core::ptr::addr_of!(self.mmap_regions)
                    as *mut Vec<MmapRegion>)
            };
            f(regions)
        })
    }

    /// Allocate `size` bytes of mmap VA space, serialized by the SAME
    /// [`Process::vm_lock`] that guards [`Process::mmap_regions`]. `ProcessMemory::
    /// alloc_mmap`'s free-list fast path (`free_regions`) was, before this, a plain
    /// unguarded `Vec` — exclusivity relied entirely on the BKL, which is not just a
    /// gap for any future BKL-free mm-syscall carve-out but a live bug today: an IRQ
    /// preemption of a CLONE_VM sibling thread mid-`alloc_mmap` (the exact race class
    /// `vm_lock` itself was introduced to close for `mmap_regions`, see that field's
    /// doc comment) can interleave a second thread's `alloc_mmap`/`free_mmap` call on
    /// the same `free_regions` `Vec`. Reusing `vm_lock` costs nothing new: `alloc_mmap`
    /// is already a pure bump-pointer/free-list operation with no allocation, I/O, or
    /// yield, matching `vm_lock`'s existing discipline exactly. `next_mmap`'s own CAS
    /// (see [`ProcessMemory::alloc_mmap`]) still stands for callers that can't take
    /// this lock, but every syscall-level caller should go through this method now.
    pub fn vm_alloc_mmap(&self, size: usize) -> Option<usize> {
        with_irqs_disabled(|| {
            let _g = self.vm_lock.lock();
            // SAFETY: `vm_lock` (held here) serializes every `vm_alloc_mmap`/
            // `vm_free_mmap` caller across the thread group, so this is the unique
            // live reference to `memory` for the closure's duration — same
            // reasoning as `vm_with_regions`'s `mmap_regions` access.
            let memory = unsafe { &mut *(core::ptr::addr_of!(self.memory) as *mut ProcessMemory) };
            memory.alloc_mmap(size)
        })
    }

    /// Return `[start, start+size)` to the mmap free-list. Counterpart to
    /// [`Process::vm_alloc_mmap`] — see that method's doc for why this needs
    /// `vm_lock` at all.
    pub fn vm_free_mmap(&self, start: usize, size: usize) {
        with_irqs_disabled(|| {
            let _g = self.vm_lock.lock();
            // SAFETY: see `vm_alloc_mmap`.
            let memory = unsafe { &mut *(core::ptr::addr_of!(self.memory) as *mut ProcessMemory) };
            memory.free_mmap(start, size);
        })
    }

    /// Run `f` holding this address space's page-table lock ([`Process::as_lock`])
    /// with preemption disabled — the shared-kernel-SMP (M5b) primitive that lets a
    /// user page fault mutate page tables **without** the Big Kernel Lock while still
    /// excluding concurrent AS-mutating syscalls and sibling faults on the same
    /// address space.
    ///
    /// `f` MUST be short and self-contained: PTE writes + frame tracking only. It MUST
    /// NOT allocate frames, do block I/O, or yield while held — do that work before/
    /// after (on private frames), exactly like [`Process::vm_with_regions`]. IRQs are
    /// disabled for the whole hold, so (a) `as_lock` is never carried across a context
    /// switch (the "spinlock across switch" deadlock class), and (b) a nested timer IRQ
    /// can't acquire the BKL behind our back and leak it (an `as_lock`→BKL inversion) —
    /// the fault fast path holds `as_lock` *instead of* the BKL. See
    /// docs/archive/SMP_SHARED_M5_FAULT_LOCK_PLAN.md.
    ///
    /// On builds without `kernel_smp_shared` this compiles to just `f()` — no lock, no
    /// IRQ change — so single-core/size/extreme/multikernel builds are unaffected.
    #[inline]
    pub fn with_as_locked<R>(&self, f: impl FnOnce() -> R) -> R {
        #[cfg(kernel_smp_shared)]
        {
            // Locals drop in reverse declaration order: `_g` (release lock) before
            // `_irq` (restore DAIF) — release the lock before re-enabling IRQs.
            let _irq = crate::runtime::IrqGuard::new();
            let _g = self.as_lock.lock();
            f()
        }
        #[cfg(not(kernel_smp_shared))]
        {
            f()
        }
    }

    /// Run `f` with `&mut UserAddressSpace` under this address space's
    /// [`Process::as_lock`] with IRQs disabled (on `smp-shared`; a plain call
    /// elsewhere, exactly like [`Process::with_as_locked`]) — the accessor that
    /// lets the shared-reference (`&Process`) syscall paths call the `&mut self`
    /// page-table mutators (`unmap_page*`, `update_page_flags*`,
    /// `unmap_and_free_page*`, `map_and_track`, …) without materializing a
    /// `&mut Process`.
    ///
    /// Interior mutability follows [`Process::vm_with_regions`]'s discipline:
    /// `as_lock` (held here, IRQs masked) is what serializes every page-table
    /// mutator on this address space across cores, so the raw-pointer `&mut` is
    /// the unique live reference for the closure's duration. On builds without
    /// `kernel_smp_shared` there is no lock — exclusion is the pre-existing
    /// single-core/BKL invariant, byte-for-byte the shape the old
    /// `&'static mut Process` call sites relied on.
    ///
    /// Same rules as [`Process::with_as_locked`]/[`AsLockHold`]: keep the closure
    /// short — PTE edits, frame bookkeeping, TLB flushes. It MUST NOT allocate
    /// page frames (the PMM OOM/reclaim path re-enters `as_lock`), do block I/O,
    /// or yield. Allocate frames before, free them after.
    #[inline]
    pub fn with_address_space<R>(&self, f: impl FnOnce(&mut UserAddressSpace) -> R) -> R {
        #[cfg(kernel_smp_shared)]
        {
            // Locals drop in reverse declaration order: `_g` (release lock) before
            // `_irq` (restore DAIF) — release the lock before re-enabling IRQs.
            let _irq = crate::runtime::IrqGuard::new();
            let _g = self.as_lock.lock();
            // SAFETY: `as_lock` is held with IRQs masked; see the doc comment.
            let aspace = unsafe {
                &mut *(core::ptr::addr_of!(self.address_space) as *mut UserAddressSpace)
            };
            f(aspace)
        }
        #[cfg(not(kernel_smp_shared))]
        {
            // SAFETY: single-core / BKL-serialized, the invariant every previous
            // `&'static mut Process` caller of these mutators already relied on.
            let aspace = unsafe {
                &mut *(core::ptr::addr_of!(self.address_space) as *mut UserAddressSpace)
            };
            f(aspace)
        }
    }

    /// RAII hold of this address space's [`Process::as_lock`] with IRQs disabled — the
    /// counterpart to [`Process::with_as_locked`] for the `&mut Process` syscall paths
    /// (`mmap`/`munmap`/`mprotect`/`brk`/teardown) where a closure would conflict with
    /// the disjoint `&mut self.address_space` edits it guards.
    ///
    /// Construct as `AsLockHold::new(&proc.as_lock)` so only the `as_lock` *field* is
    /// borrowed, leaving `proc.address_space` free to mutate (disjoint-field borrow).
    /// Same discipline as [`Process::with_as_locked`]: short window, no alloc/IO/yield.
    /// smp-shared only; call sites gate the `let _g = …` line on `cfg(kernel_smp_shared)`.
    #[cfg(kernel_smp_shared)]
    #[must_use]
    pub fn as_lock_hold(as_lock: &Spinlock<()>) -> AsLockHold<'_> {
        AsLockHold::new(as_lock)
    }

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
            pid, brk, stack_bottom, stack_top, memory.next_mmap.load(Ordering::Relaxed), memory.mmap_limit);

        // Register demand-paged regions for heap and stack growth.
        //
        // Built as a local and moved into the struct below. The pid-keyed
        // `push_lazy_region` cannot be used here: it resolves `pid` through the
        // process table, and this `Process` does not exist yet — let alone is
        // registered — so every region would be silently dropped and the first
        // heap or deep-stack touch would SIGSEGV.
        let mut lazy_regions = LazyRegionMap::new();
        let heap_lazy_size = compute_heap_lazy_size(brk, &memory);
        lazy_regions.push(brk, heap_lazy_size, crate::mmu::user_flags::RW_NO_EXEC, LazySource::Zero);
        let lazy_stack_start = stack_top.saturating_sub(LAZY_STACK_MAX);
        lazy_regions.push(lazy_stack_start, LAZY_STACK_MAX, crate::mmu::user_flags::RW_NO_EXEC, LazySource::Zero);

        Ok(Self {
            pid,
            pgid: pid,
            tgid: pid, // group leader = self
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

        // Collected into a local and moved into the struct below — see the
        // sibling constructor for why the pid-keyed API can't reach a `Process`
        // that hasn't been built yet.
        let mut lazy_regions = LazyRegionMap::new();
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
            lazy_regions.push(seg.start_va, seg.size, seg.page_flags, source);
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
            pid, brk, stack_bottom, stack_top, memory.next_mmap.load(Ordering::Relaxed), memory.mmap_limit);

        let heap_lazy_size = compute_heap_lazy_size(brk, &memory);
        lazy_regions.push(brk, heap_lazy_size, crate::mmu::user_flags::RW_NO_EXEC, LazySource::Zero);
        let lazy_stack_start = stack_top.saturating_sub(LAZY_STACK_MAX);
        lazy_regions.push(lazy_stack_start, LAZY_STACK_MAX, crate::mmu::user_flags::RW_NO_EXEC, LazySource::Zero);

        Ok(Self {
            pid,
            pgid: pid,
            tgid: pid, // group leader = self
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
            stdin: Arc::new(Spinlock::new(StdioBuffer::new())),
            stdout: Arc::new(Spinlock::new(StdioBuffer::new())),
            exited: false,
            exit_code: 0,
            dynamic_page_tables: Vec::new(),
            mmap_regions: Vec::new(),
            lazy_regions: Spinlock::new(lazy_regions),
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
    /// Activate this process's address space and eret to its saved context.
    ///
    /// Called only from [`entry_point_trampoline`], for a thread's *first* entry to
    /// EL0.
    pub fn run(&mut self) -> ! {
        // Last gate before installing an address space and eret'ing into it: this
        // thread slot must actually belong to this process. `THREAD_PID_MAP` naming a
        // *different* pid is proof it does not — running anyway would activate a
        // foreign address space and jump to this process's entry point inside it,
        // which is the `N × INTERP_BASE + 0x6c964` class (runbook §2h). A missing
        // entry is not proof of anything (some spawn paths register late), so only a
        // positive disagreement refuses.
        let tid = crate::threading::current_thread_id();
        if let Some(owner) = table::pid_for_thread(tid)
            && owner != self.pid
        {
            let mut buf = [0u8; 128];
            let mut pos = 0usize;
            let _ = core::fmt::write(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
                format_args!("[RUN-REFUSED] tid={} belongs to pid={} but pid={} tried to run it\n",
                    tid, owner, self.pid));
            if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
            crate::threading::mark_current_terminated();
            loop { crate::threading::yield_now(); }
        }

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
    pub fn read_stdin(&self, buf: &mut [u8]) -> usize {
        let mut stdin = self.stdin.lock();
        stdin.read(buf)
    }

    /// Write to this process's stdout (with size limit)
    ///
    /// Applies "last write wins" policy: if adding data would exceed
    /// PROC_STDOUT_MAX_SIZE, clears buffer before writing.
    pub fn write_stdout(&self, data: &[u8]) {
        let mut stdout = self.stdout.lock();
        stdout.write_with_limit(data, config().proc_stdout_max_size);
    }

    /// Take captured stdout (transfers ownership)
    pub fn take_stdout(&self) -> Vec<u8> {
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
    ///
    /// `&self`: page installs go through [`Process::with_address_space`]
    /// (`as_lock` + IRQs masked on smp-shared, frame allocated outside the hold,
    /// same shape as the old inline `AsLockHold`), and the `brk` scalar store is
    /// serialized under [`Process::vm_lock`] (pure bookkeeping, matching that
    /// lock's discipline). Concurrent *readers* of `brk` race the store exactly
    /// as they did against the old `&mut self` write.
    pub fn set_brk(&self, new_brk: usize) -> usize {
        if new_brk < self.initial_brk {
            return self.brk;
        }
        let aligned = (new_brk + 0xFFF) & !0xFFF;
        let old_top = (self.brk + 0xFFF) & !0xFFF;
        if aligned > old_top {
            let mut page = old_top;
            while page < aligned {
                if !self.address_space.is_range_mapped(page, 0x1000) {
                    // Allocate the frame OUTSIDE `as_lock` (the PMM OOM/reclaim path can
                    // re-enter it), then install it under the hold so a concurrent
                    // BKL-free fault on this address space excludes correctly.
                    let rt = runtime();
                    if let Some(frame) = (rt.alloc_page_zeroed)() {
                        (rt.track_frame)(frame, FrameSource::ElfLoader);
                        let _ = self.with_address_space(|aspace| {
                            aspace.map_and_track(page, frame, crate::mmu::user_flags::RW_NO_EXEC)
                        });
                    }
                }
                page += 0x1000;
            }
        }
        with_irqs_disabled(|| {
            let _g = self.vm_lock.lock();
            // SAFETY: `vm_lock` serializes this store against other `vm_*` bookkeeping;
            // see the doc comment above.
            unsafe { *(core::ptr::addr_of!(self.brk) as *mut usize) = new_brk; }
        });
        new_brk
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
    // Tripwire for the SMP=4 mixed-EL corruption: refuse silence if this EL0 entry
    // would land in kernel text (poison minted upstream — see update_thread_context).
    if ctx.pc >= 0x4000_0000 {
        let mut buf = [0u8; 128];
        let mut pos = 0usize;
        let _ = core::fmt::write(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
            format_args!("[EUM POISON] enter_user_mode pc={:#x} spsr={:#x} tid={}\n",
                ctx.pc, ctx.spsr, crate::threading::current_thread_id()));
        if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
    }
    // This `eret` drops to EL0 without returning through the syscall wrapper (initial
    // process launch / execve), so the SVC epilogue's `clear_current_trap_frame` never
    // runs for the trap that got us here. On the execve path that leaves the slot
    // pointing at the abandoned execve trap frame while userspace runs — stale for
    // every reader until the next SVC republishes. No live frame exists at an ERET to
    // user, so clear it unconditionally.
    crate::threading::clear_current_trap_frame();
    // Real shared-kernel SMP: this `eret` drops to EL0 without returning through the
    // syscall wrapper (initial process launch / execve), so release the BKL here —
    // otherwise it would stay held while running userspace. No-op unless
    // `cfg(kernel_smp_shared)`.
    crate::bkl::leave_kernel();
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
/// When return_to_kernel() calls unregister_process(), the process is retired
/// (see that function's doc comment) and later dropped by
/// `reclaim_retired_processes` after a cooldown, calling Process::drop() ->
/// UserAddressSpace::drop() which frees all physical pages (code, data, stack,
/// heap, page tables).
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
    match current_process_shared() {
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
/// Kill all threads in the same thread group (matching tgid).
/// Used by exit_group and when the address-space owner exits to prevent
/// sibling threads from running with freed page tables.
/// May a grace-expired hard kill terminate slot `tid` on behalf of sibling `sib_pid`?
///
/// Both halves are load-bearing, and each guards a distinct way the old
/// unconditional terminate went wrong:
///
/// 1. **Still a straggler.** The grace loop's own definition of a straggler is
///    "has not consumed its kill request". A sibling with no pending request either
///    already acted on it (it is self-terminating at its boundary) or never had one
///    armed — in neither case is force warranted. Measured on the pre-fix arm: 179
///    of 261 hard kills, 69 %, had `pending_kill=false`.
/// 2. **Slot still theirs.** `siblings` is a snapshot up to `KILL_GRACE_US` (2 s) old
///    and the thread-slot recycle cooldown is ~10 ms, so the recorded tid may have
///    been handed to an unrelated process many times over. Terminating it kills that
///    process's thread and strands the process itself: registered, un-exited, with no
///    thread to run — so it can never reach `exit_group`, is never reaped, and its
///    parent's `wait4` never returns.
///
/// Note the two overlap heavily but neither implies the other: the recycler clears
/// `PENDING_KILL` when it frees a slot, so a recycled slot usually fails (1) first.
/// (2) still has to be checked, for a slot recycled *and* re-armed within the window.
///
/// Same rule `unregister_process` applies (`table.rs`, "consult THREAD_PID_MAP");
/// see `docs/archive/STALE_THREAD_SLOT_KILL.md`.
pub fn grace_kill_should_terminate(sib_pid: Pid, tid: usize) -> bool {
    // The gate is **ownership alone**: THREAD_PID_MAP must still say this slot
    // belongs to the sibling we recorded it for. That is what stops the hard kill
    // from taking out an unrelated process's thread when the slot was recycled
    // during the 2 s window — the orphan class in
    // docs/archive/GRACE_EXPIRED_HARD_KILL_ORPHANS.md, and the reason this
    // predicate exists at all.
    //
    // It deliberately does **not** also require `has_pending_kill`. That conjunct
    // looked conservative and was not. The request is consumed at the EL1→EL0
    // boundary (`take_thread_kill_request`), and a thread parked in an *untimed*
    // `FUTEX_WAIT` never reaches one: `request_thread_kill` wakes it, it re-checks
    // its futex, finds it unsatisfied, and re-parks. Anything that clears the flag
    // without the thread dying therefore made this return false — the sibling was
    // spared and stayed parked forever, its `Process` never reaped and its parent's
    // `wait4` blocked for the rest of the boot. Measured on a *normal* rustc exit
    // (`[KTG] my_pid=122 code=0 siblings=2 first=Some((123, Some(16)))`) with tid=16
    // still in `FUTEX_WAIT` 557 s later and no `[KTG-STALE]` line to show for it.
    //
    // Ownership is the safety property; the pending flag was only ever evidence
    // about *timing*, and after a 2 s grace it is evidence of nothing.
    table::pid_for_thread(tid) == Some(sib_pid)
}

pub fn kill_thread_group(my_pid: Pid, _l0_phys: usize, exit_code: i32) {
    // Find tgid for the calling process
    let tgid = table::get_process_ptr(my_pid)
        .map(|ptr| unsafe { (*ptr).tgid })
        .unwrap_or(my_pid);

    let mut siblings: Vec<(Pid, Option<usize>)> = Vec::new();
    table::for_each_process(|p| {
        if p.pid != my_pid && p.tgid == tgid {
            siblings.push((p.pid, p.thread_id));
        }
    });
    // Who is tearing down which group, ungated. A group kill is rare and always
    // interesting: it is the only path that terminates threads it does not own, so
    // when a long-lived process loses a worker thread this line is what names the
    // killer. `my_tgid` is printed separately from `my_pid` because the sibling set
    // is selected by tgid alone — a caller whose row carries somebody else's tgid
    // takes out somebody else's threads, and that is invisible from `my_pid`.
    if KTG_TRACES.fetch_add(1, Ordering::Relaxed) < 512 {
        let mut buf = [0u8; 176];
        let mut pos = 0;
        let _ = core::fmt::write(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
            format_args!(
                "[KTG] my_pid={} my_tgid={} by_tid={} code={} siblings={} first={:?}\n",
                my_pid, tgid, crate::threading::current_thread_id(), exit_code,
                siblings.len(), siblings.first()));
        if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
    }

    // Tripwire: every slot this function is about to act on comes from the
    // recorded `p.thread_id`, which PHASE 2 below deliberately leaves set on dead
    // siblings. `unregister_process` already refuses to terminate a slot that
    // THREAD_PID_MAP proves was recycled (table.rs "stale tid"), but PHASE 1's
    // `request_thread_kill` and PHASE 2's `THREAD_PID_MAP.remove` have no such
    // guard — so a disagreement here is a kill (or a map eviction) aimed at
    // whoever holds the slot now. Print both readings and which siblings have no
    // live thread at all; silent when they agree, so it costs nothing in steady
    // state. Rate-limited: a teardown storm must not flood the console.
    for (sib_pid, sib_tid) in &siblings {
        let map_tid = table::thread_for_pid(*sib_pid);
        if map_tid != *sib_tid && KTG_MISMATCHES.fetch_add(1, Ordering::Relaxed) < 64 {
            let mut buf = [0u8; 160];
            let mut pos = 0;
            let _ = core::fmt::write(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
                format_args!(
                    "[KTG-MISMATCH] my_pid={} tgid={} sib_pid={} thread_id={:?} map_tid={:?} \
                     slot_owner={:?}\n",
                    my_pid, tgid, sib_pid, sib_tid, map_tid,
                    sib_tid.and_then(table::pid_for_thread)));
            if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
        }
    }

    // PHASE 1: prevent siblings from running further kernel work.
    //
    // Real shared-kernel SMP: a sibling preempted mid-EL1 may hold kernel
    // spinlocks (e.g. BLOCK_DEVICE during a demand-paging disk read). Hard-
    // marking it TERMINATED strands those locks forever — every later disk-
    // dependent path spins (the sshd "freeze"; lldb-confirmed 2026-07-22).
    // Instead post a deferred-kill request: the sibling stays schedulable,
    // finishes its critical section, releases every lock, and self-terminates
    // at its EL1→EL0 boundary (take_thread_kill_request, checked in
    // rust_sync_el0_handler). We then grace-wait for all siblings to reach
    // TERMINATED before PHASE 2 cleanup, so the sibling can't touch its
    // Process/fds while we tear them down.
    //
    // Single-core / non-SMP: the caller is the only EL1 thread, so no sibling
    // can be mid-critical-section; the direct mark is safe and the default
    // build stays byte-for-byte unchanged.
    #[cfg(kernel_smp_shared)]
    {
        for (sib_pid, sib_tid) in &siblings {
            if let Some(tid) = sib_tid {
                // Ownership test before posting the kill: a sibling that died before
                // this group kill even started keeps its recorded `thread_id`, and by
                // now the slot can be FREE or another process's. Posting a deferred
                // kill to a FREE slot plants a flag its next claimant may inherit;
                // posting to a recycled slot aims at an innocent thread. Same rule as
                // the grace-expiry hard kill below.
                if grace_kill_should_terminate(*sib_pid, *tid) {
                    crate::threading::request_thread_kill(*tid);
                }
            }
        }
        // Grace-wait: yield (dropping the BKL under smp-shared) so preempted
        // siblings can run to their boundary and self-terminate. Bounded by a
        // timeout; a sibling stuck in a non-yielding EL1 loop (a separate bug)
        // is hard-terminated as a last resort — the lock may leak, but that is
        // strictly better than hanging the caller's exit_group forever.
        const KILL_GRACE_US: u64 = 2_000_000; // 2 s
        let started = (crate::runtime::runtime().uptime_us)();
        loop {
            let all_done = siblings.iter().all(|(sib_pid, t)| {
                t.is_none_or(|tid| {
                    // Only *death* completes a kill.
                    //
                    // This used to also accept `!has_pending_kill(tid)`, which
                    // conflates "the sibling consumed the request" with "the
                    // sibling acted on it". The request is consumed at the EL1→EL0
                    // boundary (`take_thread_kill_request`), and a thread parked in
                    // an **untimed** `FUTEX_WAIT` never reaches that boundary: it is
                    // woken by `request_thread_kill`, re-checks its futex, finds it
                    // unsatisfied and re-parks. Anything that clears the flag
                    // without the thread dying therefore made it read as done — so
                    // the loop broke immediately, the grace-expiry hard kill below
                    // never ran, and the sibling was left running forever with its
                    // `Process` un-reaped and its parent's `wait4` blocked.
                    //
                    // Measured: `[KTG] my_pid=122 my_tgid=122 code=0 siblings=2
                    // first=Some((123, Some(16)))` — a *normal* rustc exit — with
                    // tid=16 still parked in `FUTEX_WAIT` 557 s later and cargo's
                    // wait4 thread parked 583 s. No `[KTG-STALE]`, no hard kill.
                    //
                    // The second arm is the same ownership test the expiry path
                    // uses: if the slot is no longer this sibling's, there is
                    // nothing left for us to kill and waiting on it is pointless.
                    crate::threading::is_thread_terminated(tid)
                        || !grace_kill_should_terminate(*sib_pid, tid)
                })
            });
            if all_done { break; }
            if (crate::runtime::runtime().uptime_us)() - started > KILL_GRACE_US {
                // Count what the loop below will actually act on — live threads we
                // still own — not what still carries a pending flag: a sibling that
                // swallowed its request is exactly the case this path exists for.
                let stragglers = siblings.iter()
                    .filter(|(sib_pid, t)| t.is_some_and(|tid| {
                        !crate::threading::is_thread_terminated(tid)
                            && grace_kill_should_terminate(*sib_pid, tid)
                    }))
                    .count();
                log::warn!(
                    "[ktg] pid={}: grace expired, hard-terminating {} straggler(s)",
                    my_pid, stragglers);
                // Terminate only the actual stragglers, and only while the slot is
                // still the sibling's.
                //
                // `siblings` is a snapshot taken up to KILL_GRACE_US (2 s) ago, and a
                // thread slot's recycle cooldown is ~10 ms — so by the time this runs a
                // sibling that already died can have had its slot handed to any number
                // of unrelated processes. The old loop terminated every recorded tid
                // unconditionally, which meant:
                //   * it killed threads that were never stragglers (`has_pending_kill`
                //     false — either they consumed the request, or the recycler cleared
                //     the flag when the slot was reused), and
                //   * it killed whoever holds the slot NOW, leaving *that* process
                //     registered with no thread at all: unschedulable, unable to reach
                //     exit_group, never reaped, with its parent's wait4 blocked forever.
                // Measured directly: `[TERM] tid=23 pid=Some(92) state=5 pending_kill=false`
                // from this line, followed by `[PROC-ORPHAN] pid=92 tgid=14` for the rest
                // of the boot — an innocent cargo worker thread, killed as a "straggler".
                //
                // This is the same rule `unregister_process` already applies (table.rs,
                // "consult THREAD_PID_MAP"); the grace path simply never got it.
                for (sib_pid, t) in &siblings {
                    if let Some(tid) = t {
                        if crate::threading::is_thread_terminated(*tid) {
                            continue;
                        }
                        if !grace_kill_should_terminate(*sib_pid, *tid) {
                            let owner = table::pid_for_thread(*tid);
                            if KTG_STALE_SKIPS.fetch_add(1, Ordering::Relaxed) < 64 {
                                let mut buf = [0u8; 160];
                                let mut pos = 0;
                                let _ = core::fmt::write(
                                    &mut FmtBuf { buf: &mut buf, pos: &mut pos },
                                    format_args!(
                                        "[KTG-STALE] my_pid={} sib_pid={} tid={} recycled to \
                                         pid={:?} — not terminating\n",
                                        my_pid, sib_pid, tid, owner));
                                if let Ok(s) = core::str::from_utf8(&buf[..pos]) {
                                    (runtime().print_str)(s);
                                }
                            }
                            continue;
                        }
                        // [KTG-HARD] is the candidate-2 marker for the page-table-UAF
                        // storm hunt: a hard-terminated straggler's core never runs the
                        // switch-out that would move its TTBR0_EL1 off the dying AS.
                        mmu::as_trace(format_args!(
                            "[KTG-HARD] my_pid={} sib_pid={} tid={} core={}\n",
                            my_pid, sib_pid, tid, crate::bkl::current_core_id()));
                        crate::threading::mark_thread_terminated(*tid);
                    }
                }
                break;
            }
            crate::threading::blocking_relax();
        }
    }
    #[cfg(not(kernel_smp_shared))]
    {
        // PHASE 1 (single-core): mark ALL sibling threads TERMINATED. The
        // caller is the sole EL1 thread, so none can be mid-critical-section.
        // Ownership-guarded like the smp-shared path: a sibling that died before
        // this group kill keeps its recorded `thread_id`, and the slot may have
        // been recycled to an unrelated process by now.
        for (sib_pid, sib_tid) in &siblings {
            if let Some(tid) = sib_tid {
                if grace_kill_should_terminate(*sib_pid, *tid) {
                    crate::threading::mark_thread_terminated(*tid);
                }
            }
        }
    }

    // PHASE 2: Now safe to clean up resources - siblings can't run.
    for (sib_pid, sib_tid) in &siblings {
        if let Some(proc) = lookup_process_shared(*sib_pid) {
            cleanup_process_fds(proc);
        }

        if let Some(tid) = sib_tid {
            // Same staleness rule as the THREAD_PID_MAP eviction below: the per-tid
            // channel registry is re-registered by each slot's new owner at spawn, so
            // once the slot is recycled the entry is the new occupant's channel.
            // Removing it orphans that process's stdout bridge, and stamping
            // `set_exited()` on it FORGES AN EXIT for a live, unrelated process: its
            // parent's `wait4` sees `has_exited()`, reaps it mid-run, and
            // `unregister_process` terminates the running thread — whose abandoned
            // fd teardown then leaks pipe write refcounts, so the reader waiting on
            // that pipe never gets EOF. Measured 2026-08-07 (`-j4` self-host hang):
            // pid 113's group kill stamped exit(0) onto recycled tid 31 (= freshly
            // spawned `ld`, pid 140); collect2 reaped live ld; rustc hung forever in
            // `read()` on the leaked pipe. A slot that is FREE (owner None) is still
            // cleaned up — a dead thread's leftover entry is exactly what this
            // eviction exists to collect.
            let owner = table::pid_for_thread(*tid);
            if owner.is_none_or(|o| o == *sib_pid) {
                if let Some(channel) = remove_channel(*tid) {
                    // Use the GROUP's real exit code, not a hardcoded -9. When a
                    // goroutine calls exit_group(0), the leader is one of these
                    // "siblings" and its channel is the one the shell reads — a
                    // hardcoded -9 here made a clean exit report as "killed by
                    // signal 9" (-9). Never clobber a channel that already recorded
                    // a real code (matches teardown_forked_process_thread_group).
                    if !channel.has_exited() {
                        channel.set_exited(exit_code);
                    }
                }
            } else if KTG_STALE_CH_SKIPS.fetch_add(1, Ordering::Relaxed) < 64 {
                let mut buf = [0u8; 160];
                let mut pos = 0;
                let _ = core::fmt::write(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
                    format_args!(
                        "[KTG-STALE-CH] my_pid={} sib_pid={} tid={} recycled to \
                         pid={:?} — not stamping channel\n",
                        my_pid, sib_pid, tid, owner));
                if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
            }
        }

        // Deliberately do NOT clear `p.thread_id` here, even though `kill_process` and
        // `kill_fork_subtree_recursive` do. Under `kernel_smp_shared` PHASE 1 only
        // *requests* a deferred kill and its grace-wait exits as soon as each sibling
        // has consumed the request — which is before the sibling has actually marked
        // itself TERMINATED. `unregister_process` re-reading this field is the backstop
        // that finishes the job; clearing it here removes that backstop and lets a
        // sibling keep running against a RETIRED `Process` (measured: wedged the box
        // mid-`execve(ld)` at SMP=1, 99% CPU, SSH dead).
        //
        // The staleness hazard that motivated clearing it is handled precisely inside
        // `unregister_process` instead: it terminates the slot when the slot is still
        // this sibling's, and skips only when THREAD_PID_MAP proves the slot has been
        // recycled to an unrelated process. Backstop kept, innocent threads spared.
        // See docs/archive/STALE_THREAD_SLOT_KILL.md §5.

        // CLONE_THREAD siblings are NOT fork children — they don't need wait4
        // to reap them. On Linux, CLONE_THREAD children are auto-reaped.
        // Unregister immediately to prevent zombie accumulation that fills
        // the 256-slot table and causes ps/list_processes to hang.
        // DO NOT clear lazy regions here — they're keyed by the address-space
        // owner PID, and the owner is still alive.
        let _ = table::unregister_process(*sib_pid);

        if let Some(tid) = sib_tid {
            // Same staleness rule as the grace path above: evict the map entry only
            // while it still names THIS sibling. If the slot has been recycled, the
            // entry belongs to its new occupant, and dropping it makes that thread's
            // identity unresolvable — `read_current_pid` returns None, which silently
            // degrades its futex keys into the VA-only `tgid=0` namespace where every
            // process running the same binary collides (see `futex_key_tgid`).
            let owner = with_irqs_disabled(|| THREAD_PID_MAP.lock().get(tid).copied());
            if owner == Some(*sib_pid) {
                with_irqs_disabled(|| {
                    THREAD_PID_MAP.lock().remove(tid);
                });
            }
            // The wake stays unconditional. Already marked terminated in phase 1 (or
            // self-terminated at the boundary under smp-shared); wake in case it is
            // still parked. Waking the slot's new occupant instead is harmless — every
            // park loop in the kernel already re-checks its condition and re-parks on a
            // spurious wake — whereas *not* waking a genuinely parked terminated sibling
            // leaves it queued forever. Only the map eviction above needs the guard.
            crate::threading::get_waker_for_thread(*tid).wake();
        }
    }

    if !siblings.is_empty() {
        log::debug!("[Process] Killed {} sibling thread(s) for PID {}",
            siblings.len(), my_pid);
    }
}

/// Debug: name every ACTIVE, un-exited process that has no live thread.
///
/// Such a process is unschedulable by construction: nothing can run in it, so it
/// can never reach `exit_group`, never publishes an exit code, is never reaped,
/// and its parent's `wait4` blocks forever. It is the terminal state of the
/// "stale thread slot" class (`docs/archive/STALE_THREAD_SLOT_KILL.md`) and it
/// leaves *no* trace in the futex tables — the waiter that is really stuck is in
/// the parent, correctly queued, waiting on a process that cannot move.
///
/// Ownership is resolved through `THREAD_PID_MAP` ([`table::thread_for_pid`]),
/// never `p.thread_id`: a dead process keeps its recorded slot number, so the
/// recorded field would report a thread that belongs to somebody else now.
/// Printed next to `[THR-DUMP]`; silent when the system is healthy.
pub fn dump_orphan_processes() {
    // Snapshot under the table lock, then resolve the map outside it: every other
    // nested taker of both goes table -> map, and this keeps it that way.
    let mut candidates: Vec<(Pid, Pid, Option<usize>, bool)> = Vec::new();
    table::for_each_process(|p| {
        candidates.push((p.pid, p.tgid, p.thread_id, p.exited));
    });
    for (pid, tgid, thread_id, exited) in candidates {
        if exited || table::thread_for_pid(pid).is_some() {
            continue;
        }
        let mut buf = [0u8; 160];
        let mut pos = 0;
        let _ = core::fmt::write(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
            format_args!(
                "[PROC-ORPHAN] pid={} tgid={} no live thread; recorded thread_id={:?} \
                 now owned by pid={:?}\n",
                pid, tgid, thread_id, thread_id.and_then(table::pid_for_thread)));
        if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
    }
}

/// Kill forked children whose `parent_pid` is in `parents`.
///
/// Used by [`kill_child_processes`] (single parent) and
/// [`kill_child_processes_for_thread_group`] (all pthread PIDs in a process).
fn kill_children_whose_parent_in(parents: &BTreeSet<Pid>) {
    // Do not skip `proc.exited`: children may already be Zombie(137) from a
    // prior kill_thread_group without unregister_process; they must still be
    // torn down here or `ps` shows zombies forever.
    let mut children: Vec<(Pid, Pid, Option<usize>, usize, bool, String)> = Vec::new();
    table::for_each_process(|p| {
        if parents.contains(&p.parent_pid) {
            children.push((p.pid, p.parent_pid, p.thread_id, p.address_space.l0_phys(),
                p.exited, p.name.clone()));
        }
    });

    // A child force-reaped here *while still alive* (`already_exited=false`) never
    // got to run its own exit path — no fd flush, no `chmod`, no clean file close.
    // If that child is a linker (`cc`/`collect2`/`ld`/`rust-lld`), this is the
    // mechanism behind the truncated/empty linker-output failure: see
    // docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md §4.
    for (child_pid, parent_pid, _, _, already_exited, name) in &children {
        if ORPHAN_KILL_TRACES.fetch_add(1, Ordering::Relaxed) < 512 {
            let mut buf = [0u8; 176];
            let mut pos = 0;
            let _ = core::fmt::write(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
                format_args!(
                    "[ORPHAN-KILL] parent_pid={} child_pid={} already_exited={} name={}\n",
                    parent_pid, child_pid, already_exited, name));
            if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
        }
    }

    for (child_pid, _, _, l0_phys, _, _) in &children {
        kill_fork_subtree_recursive(*child_pid, *l0_phys);
    }

    if !children.is_empty() {
        log::debug!("[Process] Killed {} forked child process(es) (parent set size {})",
            children.len(), parents.len());
    }
}

/// Tear down one forked process: its pthread group and all `PROCESS_TABLE` rows.
///
/// Without this, entries stay `Zombie(137)` until `return_to_kernel` — compile
/// workers often never get there, so `ps` shows zombies forever.
///
/// Drop order: CLONE_THREAD siblings (`shared == true`) first, address-space
/// owner (`shared == false`) last so `UserAddressSpace::drop` frees L0 once.
fn teardown_forked_process_thread_group(child_pid: Pid, l0_phys: usize) {
    let mut members: Vec<(Pid, Option<usize>, bool)> = Vec::new();
    table::for_each_process(|p| {
        if p.address_space.l0_phys() == l0_phys {
            members.push((p.pid, p.thread_id, p.address_space.is_shared()));
        }
    });
    if members.is_empty() {
        return;
    }
    members.sort_by_key(|(_, _, shared)| if *shared { 0 } else { 1 });

    kill_thread_group(child_pid, l0_phys, 137);

    for (pid, tid, _) in &members {
        if let Some(proc) = lookup_process_shared(*pid) {
            cleanup_process_fds(proc);
        }
        table::with_process(*pid, |p| {
            p.exited = true;
            p.exit_code = 137;
            p.state = ProcessState::Zombie(137);
            p.thread_id = None;
        });
        // Same Arc as register_child_channel / notify_child_channel_exited /
        // return_to_kernel::remove_channel.  If the child already called
        // exit_group / return_to_kernel, `has_exited` is true with the real code
        // (e.g. 0).  Do **not** overwrite with 137 — wait4 would report 137
        // while `[exit_group] … code=0` and confuse `go build` (buildID).
        // `publish_child_exit` keeps that guard AND raises SIGCHLD on the first
        // publish, so a parent tearing down a live subtree is notified.
        publish_child_exit(*pid, 137);
        if let Some(tid) = tid {
            if let Some(ch) = remove_channel(*tid) {
                if !ch.has_exited() {
                    ch.set_exited(137);
                }
            }
            crate::threading::mark_thread_terminated(*tid);
            crate::threading::get_waker_for_thread(*tid).wake();
            with_irqs_disabled(|| {
                THREAD_PID_MAP.lock().remove(tid);
            });
        }
        // NOTE: clear_lazy_regions(*pid) is intentionally NOT called here — the
        // per-process `lazy_regions` field drops inside `Process::drop` on the
        // reclaim path. Calling it from teardown would re-enter the lock an
        // OOM'd mutator frame may still hold (rule-2 hang).
        let _ = table::unregister_process(*pid);
    }
}

/// Depth-first: nested forked children (e.g. `go` → `compile` → sub-tools) are
/// torn down before their parent fork.
fn kill_fork_subtree_recursive(child_pid: Pid, l0_phys: usize) {
    let mut nested: Vec<(Pid, usize)> = Vec::new();
    table::for_each_process(|p| {
        if p.parent_pid == child_pid {
            nested.push((p.pid, p.address_space.l0_phys()));
        }
    });
    for (npid, nl0) in nested {
        kill_fork_subtree_recursive(npid, nl0);
    }

    if lookup_process_shared(child_pid).is_none() {
        return;
    }
    teardown_forked_process_thread_group(child_pid, l0_phys);
}

/// Kill all forked child processes of the given parent PID only.
///
/// `fork_process` sets `parent_pid` to the **forking thread's** PID (each
/// CLONE_THREAD has its own `Process` entry).  Prefer
/// [`kill_child_processes_for_thread_group`] from `return_to_kernel` so
/// children forked by worker threads are not missed.
pub fn kill_child_processes(parent_pid: Pid) {
    let mut s = BTreeSet::new();
    s.insert(parent_pid);
    kill_children_whose_parent_in(&s);
}

/// Kill forked children whose parent is **any** thread in this address space.
///
/// Go and other runtimes call `clone`/`fork` from M-threads; each thread has a
/// distinct PID in `PROCESS_TABLE`, but `fork_process` records that thread's
/// PID as `parent_pid`.  When the main thread (TGID) exits, `kill_child_processes(main_pid)`
/// would miss compiles forked by worker PID 53 because `parent_pid == 53`, not `main_pid`.
/// This collects every PID sharing `l0_phys` (the whole pthread group) and kills
/// children of any of them.
pub fn kill_child_processes_for_thread_group(l0_phys: usize) {
    let parents: BTreeSet<Pid> = table::collect_pids(|p| p.address_space.l0_phys() == l0_phys)
        .into_iter()
        .collect();
    kill_children_whose_parent_in(&parents);
}

/// Exit code is communicated via ProcessChannel for async callers.
#[unsafe(no_mangle)]
pub extern "C" fn return_to_kernel(exit_code: i32) -> ! {
    // A BKL-opted-out syscall (Phase 7f per-syscall opt-out list) runs its whole EL0
    // excursion inside an open dropped-BKL window, and this function never returns to
    // that excursion's exit path — clear the thread's ledger and restore the "EL1
    // holds the BKL" invariant before the teardown below touches shared lifecycle
    // state (mirrors `return_to_kernel_from_fault`). No-op (depth 0) for every
    // non-opted-out caller, i.e. all of them until the opt-out list is populated.
    crate::bkl::reset_dropped_windows();
    // Serialize the entire teardown (channel notify, fd close, AS deactivate,
    // child/box kill, unregister) against concurrent lifecycle ops on peer cores.
    // Released explicitly before the terminal yield loop (this function never
    // returns, so the RAII guard would otherwise hold the lock forever — wedging
    // every future fork/exec/exit on the box). See `process/lifecycle.rs`.
    let lifecycle = LifecycleGuard::acquire();
    let lr: u64;
    #[cfg(target_os = "none")]
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr); }
    #[cfg(not(target_os = "none"))]
    { lr = 0; }
    let tid = crate::threading::current_thread_id();
    log::debug!("[RTK] code={} tid={} LR={:#x}", exit_code, tid, lr);
    
    // Check if this thread was already killed externally (by kill_process or
    // kill_thread_group).
    let already_terminated = crate::threading::is_thread_terminated(tid);
    
    // Always try to resolve the PID so we can unregister the process later.
    // For kill_process: the process is already unregistered, current_process_shared()
    //   returns None → pid = None → cleanup section is skipped.
    // For kill_thread_group: the process is still registered (only marked
    //   zombie), current_process_shared() succeeds → pid = Some → we can unregister
    //   below, preventing the process from leaking in PROCESS_TABLE.
    let pid = if let Some(proc) = current_process_shared() {
        let pid = proc.pid;
        if !already_terminated {
            cleanup_process_fds(proc);
        }
        Some(pid)
    } else {
        None
    };

    // Publish the child exit + raise SIGCHLD BEFORE the thread-channel set_exited
    // below. `publish_child_exit`'s `has_exited` guard means: if a crash path
    // already called `notify_child_channel_exited_pub`, this is a no-op (no
    // duplicate SIGCHLD); if the process fell off the end of `main` without
    // calling `exit_group`, THIS is the first publish and the parent gets its
    // SIGCHLD. Must run first so the guard sees the un-exited channel.
    if let Some(pid) = pid {
        publish_child_exit(pid, exit_code);
    }

    // Set exit code on ProcessChannel if registered for this thread
    // This notifies async callers (SSH shell, etc.) that the process exited
    // Safe to call even if already removed by kill_process - just returns None.
    // Same Arc as the child channel above, so on the fall-off-the-end path this
    // is now a redundant re-set (harmless); on the crash path it is the wake for
    // async SSH pollers.
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
    //
    // Deliberately NOT gated on `!already_terminated`, unlike the robust-list walk
    // below. A thread killed from outside (`kill_thread_group`, a fault-kill, a
    // consumed `PENDING_KILL`) is exactly the case that needs this most: it never ran
    // its own exit epilogue, so the *only* thing that can release what it held is the
    // kernel. musl leans on this directly — `pthread_create` passes
    // `&__thread_list_lock` (a `libc.bss` global) as the `CLONE_CHILD_CLEARTID` word,
    // so the kernel's store+wake here IS how the thread-list lock is released on
    // thread exit. Skipping it leaves that lock owned by a dead tid, and every
    // subsequent `pthread_create`/`pthread_exit` in the process parks in `__tl_lock`
    // forever — one killed thread wedges the whole process, with the futex table
    // showing a perfectly ordinary, correctly-queued waiter.
    if let Some(proc) = lookup_process_shared(pid.unwrap_or(0)) {
        let tid_addr = proc.clear_child_tid;
        if tid_addr != 0 {
            if crate::mmu::is_current_user_page_mapped(tid_addr as usize) {
                unsafe { core::ptr::write(tid_addr as *mut u32, 0); }
            }
            // Wake pthread_join waiters regardless of whether the page was
            // mappable: futex_wake reads the kernel waiter table, not user
            // memory, and a joiner must be woken even if we couldn't zero the
            // word (else join() futex-waits forever).
            (runtime().futex_wake)(proc.tgid, tid_addr as usize, i32::MAX);
        }
    }

    if !already_terminated {
        if let Some(proc) = lookup_process_shared(pid.unwrap_or(0)) {
            // Robust futex list cleanup: walk the list and mark owned futexes
            // with FUTEX_OWNER_DIED so waiters don't deadlock.
            let robust_head = proc.robust_list_head;
            if robust_head != 0 {
                const FUTEX_OWNER_DIED: u32 = 0x40000000;
                const ROBUST_LIST_LIMIT: usize = 2048;
                let my_tid = proc.pid;
                let my_tgid = proc.tgid;
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
                                    (runtime().futex_wake)(my_tgid, futex_addr, 1);
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
                                (runtime().futex_wake)(my_tgid, futex_addr, 1);
                            }
                        }
                    }
                }
            }
        }
    }

    // Deactivate user address space - restore boot TTBR0
    // CRITICAL: This must happen BEFORE we unregister the Process, since TTBR0
    // must never point at page tables that could be freed out from under it.
    crate::mmu::UserAddressSpace::deactivate();

    // Now unregister (retire) the process. `unregister_process` no longer drops
    // it synchronously — see its doc comment — but the ordering constraint above
    // still holds: retiring makes it eligible for `reclaim_retired_processes` to
    // free after a cooldown, and TTBR0 must already be off these page tables by
    // then. When the deferred drop does run, it frees:
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

        // Kill forked child processes (different address spaces) so they
        // don't become orphans.  `fork()` records the **forking thread's** PID
        // as parent_pid, not the TGID — so when the **address-space owner**
        // exits we must scan every pthread PID in the group.
        //
        // **Do not** run that full scan on **pthread** exit: CLONE_VM workers
        // share `l0_phys` with the main thread; `kill_child_processes_for_thread_group`
        // includes *all* thread PIDs in `parents`, so any child whose parent_pid
        // is the main thread would be matched whenever *any* worker exits — killing
        // live `compile` subprocesses still needed by other threads (exit 137).
        let (l0_phys, is_shared) = match lookup_process_shared(pid) {
            Some(p) => (p.address_space.l0_phys(), p.address_space.is_shared()),
            None => (0usize, true),
        };
        if l0_phys != 0 {
            if is_shared {
                kill_child_processes(pid);
            } else {
                kill_child_processes_for_thread_group(l0_phys);
            }
        }

        // If this process owns the address space (not shared), kill all
        // sibling CLONE_VM threads BEFORE dropping. Dropping the owner frees
        // all page tables; siblings still using them would cause EL1 faults.
        if !is_shared && l0_phys != 0 {
            kill_thread_group(pid, l0_phys, exit_code);
        }

        let (start_us, proc_name) = lookup_process_shared(pid)
            .map(|p| (p.start_time_us, p.name.clone()))
            .unwrap_or((0, alloc::string::String::from("?")));
        let elapsed_us = (runtime().uptime_us)().saturating_sub(start_us);
        let secs = elapsed_us / 1_000_000;
        let frac = (elapsed_us % 1_000_000) / 10_000; // centiseconds

        if process_syscall_stats_enabled() {
            if let Some(proc) = lookup_process_shared(pid) {
                proc.syscall_stats.dump(pid, &proc_name, elapsed_us);
            }
        }

        // Read tgid BEFORE unregister (for future signal-based group exit).
        let _tgid = lookup_process_shared(pid).map(|p| p.tgid);

        // NOTE: clear_lazy_regions(pid) is intentionally NOT called here — the
        // per-process `lazy_regions` field drops inside `Process::drop` on the
        // reclaim path. Calling it from teardown would re-enter the lock an
        // OOM'd mutator frame may still hold (rule-2 hang).
        unregister_process(pid);
        log::debug!("[Process] PID {} thread {} exited ({}) [{}.{:02}s]", pid, tid, exit_code, secs, frac);

        // DO NOT kill the thread group leader from a goroutine thread's exit path.
        //
        // Previously this code killed the leader when a goroutine crashed (SIGSEGV etc).
        // This caused SIGSEGV at PC=0x20000000 — the leader's address space was destroyed
        // while its main thread was still running. The race is unfixable because:
        // 1. The crashing goroutine's return_to_kernel runs on one thread
        // 2. The leader's main thread is running user code on the same CPU (preemptive)
        // 3. Killing the leader frees page tables that the main thread's TTBR0 points to
        //
        // On Linux, a thread crash sends SIGSEGV to the process (exit_group), which
        // coordinates shutdown through the signal mechanism. Akuma should do the same
        // (pending future work). For now, the crashed goroutine just exits individually.
        // The leader and other goroutines continue running. If the leader itself crashes,
        // it handles its own cleanup via its own return_to_kernel.
        //
        // Orphaned goroutine zombies are cleaned up by on_thread_cleanup when their
        // thread slots are recycled, or by kill_process when the parent exits.
    } else {
        log::debug!("[Process] Thread {} exited ({})", tid, exit_code);
    }
    
    // Drop this thread's EL0 trap-frame pointer before the slot becomes eligible for
    // recycling. This function never returns to the SVC epilogue that would otherwise
    // clear it (`exceptions.rs`: `clear_current_trap_frame` sits *after* the
    // exited-process check that jumps here), so every syscall exit — i.e. essentially
    // every process — used to leave the entry pointing at its own kernel stack. The
    // recycler clears it too; doing it here closes the window in between, during which
    // the diagnostic readers (`current_trap_frame_elr`, `dump_thread_resume_points`)
    // would report this zombie's dead frame.
    crate::threading::clear_current_trap_frame();

    // Mark thread as terminated so scheduler stops scheduling it
    // Idempotent - safe to call even if already marked by kill_process
    crate::threading::mark_current_terminated();

    // Release the lifecycle lock before entering the terminal yield loop. The thread
    // is now zombie and will never run user code again; holding the lock past this
    // point would deadlock every future fork/exec/exit on the box. Drop is explicit
    // (rather than via the RAII guard's scope-end drop) because this function never
    // returns — the `loop` below is the function's end.
    drop(lifecycle);

    // Yield forever - thread is terminated, scheduler will reclaim it
    // Thread 0's cleanup routine will free the thread slot.
    // Defensive (mirrors return_to_kernel_from_fault): if this exit was reached
    // with IRQs masked, the yield loop could never switch away. Re-enable IRQs so
    // the terminated thread is always schedulable off-CPU.
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("msr daifclr, #2", "isb", options(nomem, nostack));
    }
    // Pressure-driven reclaim of previously RETIRED processes — the first of
    // `process::reclaim`'s vetted drain sites, and the one that matters under an
    // OOM-kill storm (`docs/archive/OOM_KILL_DEFERRED_RECLAIM_GAP.md` §5 candidate 1).
    // Everything that makes this context safe is above us: the lifecycle guard is
    // released (so a multi-thousand-page free cannot trip its 100 ms preemption
    // watchdog, and `Process::drop`'s fd-table close cannot re-enter it), the address
    // space is deactivated, the dropped-window ledger is reset, IRQs are back on, and
    // no drop-path lock is held. Our OWN slot was retired microseconds ago and is
    // still inside its cooldown, so this can never free the `Process` we just left.
    crate::process::reclaim::drain_retired_if_requested();
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
    // This thread's kernel call stack was ABANDONED (ELR redirected here after an EL1
    // fault), so any live BKL-carve-out guard never ran its destructor. Clear the
    // thread's dropped-window ledger and restore the "EL1 holds the BKL" invariant
    // before touching shared teardown state — a stale window would otherwise make the
    // IRQ epilogues release the BKL mid-teardown (and poison the recycled slot).
    // No-op unless smp-shared.
    crate::bkl::reset_dropped_windows();
    // Serialize teardown against concurrent lifecycle ops (mirrors `return_to_kernel`).
    // Released explicitly before the terminal yield loop below — see the comment there.
    let lifecycle = LifecycleGuard::acquire();
    let tid = crate::threading::current_thread_id();
    log::debug!("[RTK-FAULT] code={} tid={}", exit_code, tid);

    let already_terminated = crate::threading::is_thread_terminated(tid);

    let pid = if !already_terminated {
        if let Some(proc) = current_process_shared() {
            let pid = proc.pid;
            cleanup_process_fds(proc);
            Some(pid)
        } else {
            None
        }
    } else {
        None
    };

    // Publish child exit + SIGCHLD before the thread-channel set_exited below —
    // same rationale as `return_to_kernel` (first-publish-wins via the
    // `has_exited` guard; covers the fault-exit fall-off-the-end path).
    if let Some(pid) = pid {
        publish_child_exit(pid, exit_code);
    }

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

        let (l0_phys, is_shared) = match lookup_process_shared(pid) {
            Some(p) => (p.address_space.l0_phys(), p.address_space.is_shared()),
            None => (0usize, true),
        };
        if l0_phys != 0 {
            if is_shared {
                kill_child_processes(pid);
            } else {
                kill_child_processes_for_thread_group(l0_phys);
            }
        }

        if !is_shared && l0_phys != 0 {
            kill_thread_group(pid, l0_phys, exit_code);
        }

        let start_us = lookup_process_shared(pid)
            .map(|p| p.start_time_us)
            .unwrap_or(0);
        let elapsed_us = (runtime().uptime_us)().saturating_sub(start_us);
        let secs = elapsed_us / 1_000_000;
        let frac = (elapsed_us % 1_000_000) / 10_000;

        // NOTE: clear_lazy_regions(pid) intentionally omitted (see return_to_kernel).
        unregister_process(pid);
        log::debug!("[Process] PID {} thread {} faulted ({}) [{}.{:02}s]", pid, tid, exit_code, secs, frac);
    } else {
        log::debug!("[Process] Thread {} faulted ({})", tid, exit_code);
    }

    // Drop the trap-frame pointer before the slot can be recycled — same reasoning as
    // `return_to_kernel`. Doubly needed here: the frame this thread published sits on
    // the kernel stack that the EL1 fault ABANDONED.
    crate::threading::clear_current_trap_frame();

    crate::threading::mark_current_terminated();

    // Release the lifecycle lock before entering the terminal yield loop — same
    // reasoning as `return_to_kernel`. The thread is zombie now; holding the lock
    // would deadlock future fork/exec/exit on the box.
    drop(lifecycle);

    // CRITICAL: this path is entered from the EL1 fault-recovery pad, which ERETs
    // with the *faulting* code's DAIF. If the fault hit a kernel critical section
    // (IRQs masked, SPSR.I=1), the terminal yield loop below can never switch away
    // — `yield_now` triggers the scheduler SGI, but a masked IRQ is never taken, so
    // the now-terminated thread spins forever and wedges the whole VM (observed:
    // an EL1 abort in ssh::server::run left tid=2 spinning in `yield_now with IRQs
    // masked`, killing SSH for the entire box). Re-enable IRQs so the scheduler can
    // preempt this terminated thread and reclaim it — turning a VM-wide hang back
    // into a clean single-process kill.
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("msr daifclr, #2", "isb", options(nomem, nostack));
    }

    // Same vetted drain site as `return_to_kernel` — see the comment there. Placed
    // after the IRQ re-enable above deliberately: this path inherits the *faulting*
    // code's DAIF, and running a multi-thousand-page free with IRQs still masked would
    // starve this core's timer for the duration.
    crate::process::reclaim::drain_retired_if_requested();

    loop {
        crate::threading::yield_now();
    }
}

/// Clean up all file descriptors owned by a process.
///
/// With shared fd tables (CLONE_FILES), only the last **live** user of the
/// table performs actual cleanup; while siblings are alive, their fds must not
/// be closed out from under them (the git sideband-thread bug `sys_exit`'s
/// comment documents).
///
/// "Live" is decided by scanning the process table for other not-yet-exited
/// processes sharing this same `Arc<SharedFdTable>` — NOT by
/// `Arc::strong_count == 1`, which this used to test. Phase 7e's "Free" half
/// defers `Process::drop` (RETIRED slots wait for `reclaim_retired_processes`),
/// so a killed CLONE_THREAD sibling's `Arc` clone now stays alive long after
/// the sibling is dead: under the old count test, an externally-killed
/// multithreaded group (`kill -9`, `kill_process*`) NEVER saw the count reach 1
/// and its pipes/sockets were released only by the deferred collector — which
/// during the synchronous boot self-test phase never runs at all. Same defect
/// class as `sys_exit_group`'s close-after-notify (BKL_PHASE7E §3b): a pipe
/// read end held forever hangs the peer. RETIRED processes are invisible to
/// `for_each_process`, and kill paths mark `exited` (or retire) each member
/// before/without ever re-entering here for it, so the last member's cleanup
/// correctly sees itself as the sole live user and closes. `close_all` stays
/// idempotent, so over-calling on already-emptied tables is harmless.
fn cleanup_process_fds(proc: &Process) {
    let mut live_sharers = 0usize;
    table::for_each_process(|p| {
        if !p.exited && Arc::ptr_eq(&p.fds, &proc.fds) {
            live_sharers += 1;
        }
    });
    // `live_sharers` counts `proc` itself unless it is already marked exited
    // or already retired; in every such caller the table should be closed.
    if live_sharers <= 1 {
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
/// Returns the new PID to the parent.
///
/// **Locks:** `clone_deep_for_fork` and the lazy-region snapshot take
/// `SharedFdTable` / the parent's `Process::lazy_regions` only inside short
/// `with_irqs_disabled` windows. The long eager copies do **not** hold those locks,
/// so fork is not expected to deadlock the fd table or either process's
/// lazy-region map. A pathological
/// huge `brk` can still monopolize CPU for a long time (see `MAX_FORK_BRK_COPY_PAGES`).
pub fn fork_process(child_pid: u32, stack_ptr: u64) -> Result<u32, &'static str> {
    // Serialize lifecycle against preemption under shared-kernel SMP. See
    // `process/lifecycle.rs`. The guard drops on every return path (including `?`
    // early-returns), so the lock is released exactly when the function exits.
    let _lifecycle = LifecycleGuard::acquire();
    lifecycle_trace("[FORK-DBG] fork_process ENTRY\n");
    if (runtime().is_memory_low)() {
        return Err("Kernel memory low, cannot fork");
    }
    let parent = current_process_shared().ok_or("No current process")?;
    let parent_pid = parent.pid;
    // Lazy mmap regions are keyed by *thread-group id* (see `mmap` →
    // `push_lazy_region(proc.tgid, …)`): every thread sharing this address space
    // registers its anonymous mappings (including each pthread's stack) under
    // the one tgid. fork must enumerate lazy regions by tgid, NOT by the forking
    // thread's pid — otherwise a worker-thread fork (pid != tgid) drops every
    // *sibling* thread's stack from the child, and the child's libc `fork()`
    // thread-list fixup faults dereferencing a sibling pthread node that was
    // never replicated (docs/RUST_TOOLCHAIN.md §4). For a single-threaded
    // process pid == tgid, so this is a no-op there.
    let parent_tgid = parent.tgid;

    if lifecycle_trace_on() {
        let mut buf = [0u8; 128];
        let mut pos = 0usize;
        let _ = core::fmt::write(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
            format_args!("[FORK-DBG] parent_pid={} child_pid={} brk=0x{:x} code_end=0x{:x} mmap_regions={} lazy_regs={}\n",
                parent_pid, child_pid, parent.brk, parent.memory.code_end,
                parent.mmap_regions.len(),
                parent.lazy_regions.lock().len()));
        if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
    }

    // 1. Create new address space
    let mut new_address_space = mmu::UserAddressSpace::new().ok_or("Failed to create address space")?;
    mmu::as_trace(format_args!("[AS-NEW] pid={} l0=0x{:x} asid=0x{:x} via=fork parent={}\n",
        child_pid, new_address_space.l0_phys(), new_address_space.asid(), parent_pid));

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
        tgid: child_pid, // fork creates a new thread group
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
        stdin: parent.stdin.clone(), // Share!
        stdout: parent.stdout.clone(), // Share!
        exited: false,
        exit_code: 0,
        dynamic_page_tables: Vec::new(),
        mmap_regions: Vec::new(),
        lazy_regions: Spinlock::new(LazyRegionMap::new()),
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
        fault_mutex: Spinlock::new(BTreeMap::new()),
        vm_lock: Spinlock::new(()),
        as_lock: Spinlock::new(()),
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

    // The `as_lock` that serializes THIS address space's page tables — the very lock
    // the CoW fault handler takes (`AsLockHold::new(&owner.as_lock)` in
    // src/exceptions.rs). The `no-bkl-process` carve-out holds it in bounded chunks
    // around every parent-PTE access below, which is what lets the BKL be dropped for
    // the copy.
    //
    // It is the thread-group LEADER's lock, not necessarily this thread's:
    // `CLONE_THREAD` siblings each get a fresh `Spinlock` in their own `Process` (see
    // the struct literal above) while SHARING one address space, and the fault handler
    // resolves its owner from the live TTBR0 via `address_space_owner_pid_for_fault`.
    // A worker-thread fork (pid != tgid) that took `parent.as_lock` would hold a lock
    // no fault handler ever waits on, and the window would exclude nothing. Inside
    // fork the live TTBR0 *is* the parent's address space, so this resolves to exactly
    // the pid the fault handler would pick.
    let as_lock: &Spinlock<()> = {
        let owner_pid = address_space_owner_pid_for_fault().unwrap_or(parent_pid);
        if owner_pid == parent_pid {
            &parent.as_lock
        } else {
            lookup_process_shared(owner_pid).map_or(&parent.as_lock, |p| &p.as_lock)
        }
    };

    fn copy_range_phys(
        parent_l0: *const u64,
        src_va: usize,
        len: usize,
        dest_as: &mut mmu::UserAddressSpace,
        max_pages: Option<usize>,
        label: &'static str,
    ) -> Result<(), &'static str> {
        let pages = fork_page_count_for_len(len).ok_or("Fork copy page count overflow")?;
        if let Some(cap) = max_pages {
            if pages > cap {
                return Err("Fork brk copy exceeds kernel page cap");
            }
        }
        let mut copied = 0usize;
        for i in 0..pages {
            let page_off = i
                .checked_mul(mmu::PAGE_SIZE)
                .ok_or("Fork copy VA overflow")?;
            let va = src_va
                .checked_add(page_off)
                .ok_or("Fork copy VA overflow")?;
            if i > 0 && (i % FORK_COPY_PROGRESS_INTERVAL_PAGES == 0) {
                if config().syscall_debug_info_enabled {
                    log::debug!(
                        "[fork] {} copy progress: {} / {} pages (va={:#x})",
                        label,
                        i,
                        pages,
                        va
                    );
                }
                // Serial — `log::debug!` often does not appear on QEMU console; brk is the long path.
                if label == "brk" && config().fork_brk_serial_progress {
                    (runtime().print_str)(
                        "[fork] brk copy still running (enable SYSCALL_DEBUG_INFO for page numbers in log)…\n",
                    );
                }
            }
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
            log::debug!(
                "[fork] copy_range WARNING {}: 0x{:x}..0x{:x}: {}/{} pages copied ({} unmapped)",
                label,
                src_va,
                src_va.saturating_add(len),
                copied,
                pages,
                pages.saturating_sub(copied)
            );
        }
        Ok(())
    }

    if config().cow_fork_enabled {
        // ── CoW fork: share physical pages read-only instead of copying ──
        //
        // `no-bkl-process` (docs/archive/BKL_PROCESS_CARVE_OUT.md §9): this pass runs
        // with the Big Kernel Lock DROPPED. Everything it touches is either private to
        // the not-yet-published child, or covered by an inner lock: the parent's page
        // tables by `as_lock` (taken per chunk below — the same lock, and the same
        // discipline, the CoW fault handler already uses BKL-free), the CoW refcounts
        // by `COW_REFCOUNTS`, the frame pool and heap by the PMM/allocator's own locks.
        // Steps 5–8 that follow stay fully BKL-held: they touch `THREAD_CONTEXTS` and
        // the process table, which have no inner lock and where the BKL *is* the lock.
        lifecycle_trace("[FORK-DBG] step4: CoW fork\n");
        let cow_start_us = (runtime().uptime_us)();
        let mut total_shared: usize = 0;


        // code_start is the lowest VA we need to scan for code/data pages:
        //   - Large binaries (code_end >= 256 MB): loaded at or above 0x1000_0000.
        //   - Typical musl/TCC binaries: loaded at 0x400000, scan from 0x400000.
        //   - Go ARM64 binaries: loaded at ~0x40000 (code_end < 0x400000), so
        //     scan from PAGE_SIZE to cover the actual text segment.
        let code_start = if parent.memory.code_end >= 0x1000_0000 {
            0x1000_0000
        } else if parent.memory.code_end < 0x400000 {
            mmu::PAGE_SIZE  // binary loads below 4 MB (e.g. Go ARM64 at ~0x40000)
        } else {
            0x400000
        };
        let interp_base = 0x3000_0000usize;
        let interp_scan_size = 2 * 1024 * 1024;

        // ── Collected BKL-held, BEFORE the dropped window opens ──
        //
        // Sibling-thread EAGER mmap regions. Each thread has its own `Process` with a
        // private `mmap_regions` Vec, and `mmap` pushes eager mappings (e.g. a small
        // pthread stack ≤256 pages) onto the *calling* thread's struct. When a worker
        // thread forks we must replicate every sibling's eager mappings too —
        // otherwise the child's libc `fork()` thread-list fixup faults dereferencing a
        // sibling pthread node whose stack was never copied (docs/RUST_TOOLCHAIN.md
        // §4b′). All threads share one address space (`parent_l0`), so the same CoW
        // share applies. `for_each_process` runs IRQs-disabled and forbids allocation
        // in its callback, so collect (va,len) into a pre-reserved Vec (push within
        // capacity does not allocate), then share afterwards.
        //
        // This scan walks the PROCESS TABLE, which the `no-bkl-process` carve-out does
        // NOT cover — the table hands out `&'static mut Process` under nothing but a
        // local IRQ mask, so the BKL is its only cross-core lock. It therefore has to
        // run before the window opens. The code already collected into a local Vec; the
        // only change is that it now happens up front rather than mid-copy.
        let sibling_ranges: alloc::vec::Vec<(usize, usize)> = {
            let mut ranges: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::with_capacity(2048);
            let mut overflow = false;
            table::for_each_process(|p| {
                if p.tgid == parent_tgid && p.pid != parent_pid {
                    for region in &p.mmap_regions {
                        if region.pages > 0 {
                            if ranges.len() < ranges.capacity() {
                                ranges.push((region.start_va, region.len_bytes()));
                            } else {
                                overflow = true;
                            }
                        }
                    }
                }
            });
            if overflow {
                lifecycle_trace("[FORK-COW] WARNING: sibling mmap region list truncated (>2048 regions)\n");
            }
            ranges
        };

        // Lazy regions, likewise snapshotted BKL-held. We share the pages the parent
        // has *resident* in each lazy range, AND propagate the parent's lazy-region
        // *descriptors* to the child's own tgid — see `propagate_lazy_regions_to_child`'s
        // doc comment for why: `cow_share_and_demote_range` only shares resident pages, so a lazy
        // region the parent registered but hasn't fully touched yet would otherwise
        // vanish for the child (not resident, not registered). Note: this closes a real
        // correctness gap, but is NOT a fix for the specific fork-then-exec SIGSEGV in
        // docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md — that crash's faulting VA
        // was confirmed unregistered in the parent too. That bug remains open.
        //
        // One snapshot serves the share, the demote AND the descriptor
        // propagation — `propagate_lazy_regions_to_child` writes straight into
        // `new_proc`, which is not registered in the process table until the very
        // end of this function, so it must be reached by reference here.
        let parent_lazy_regions: alloc::vec::Vec<crate::process::types::LazyRegion> =
            lazy_regions_snapshot(parent_tgid)
                .map(|m| m.into_values().collect())
                .unwrap_or_default();
        propagate_lazy_regions_to_child(&parent_lazy_regions, &new_proc);

        // Per-chunk PTE snapshot buffer, reserved ONCE here so no `as_lock` hold below
        // ever has to grow it (see `FORK_AS_CHUNK_PAGES`).
        let mut chunk_scratch: alloc::vec::Vec<(usize, usize, u64)> =
            alloc::vec::Vec::with_capacity(FORK_AS_CHUNK_PAGES);

        {
            // Locals drop in reverse declaration order, so on every exit path —
            // including the `?` early-returns below — `_bkl` re-acquires the BKL first
            // and only then `_forking` clears FORK_IN_PROGRESS, BKL-held again.
            let _forking = ForkInProgressGuard::new();
            let _bkl = ProcessBklGuard::new();

            // Share stack
            total_shared += cow_share_and_demote_range(parent_l0, as_lock, stack_start, stack_size,
                &mut new_proc.address_space, &mut chunk_scratch, "stack")?;

            // Share code+brk.
            if parent.brk > code_start {
                let brk_len = parent.brk - code_start;
                total_shared += cow_share_and_demote_range(parent_l0, as_lock, code_start, brk_len,
                    &mut new_proc.address_space, &mut chunk_scratch, "brk")?;
            }

            // Share interpreter region
            if mmu::translate_user_va(parent_l0, interp_base).is_some() {
                total_shared += cow_share_and_demote_range(parent_l0, as_lock, interp_base, interp_scan_size,
                    &mut new_proc.address_space, &mut chunk_scratch, "interp")?;
            }

            // Share mmap regions.
            //
            // Size the share from `region.pages`, NOT `region.frames.len()`: when the
            // parent is itself a CoW-forked child its regions are `inherited` (extent
            // known, no owned frames), and using the frame count would compute a
            // zero-length range and silently skip the region — leaving this child with
            // no mapping at all for a VA its parent has resident and will hand it a
            // live pointer into. That is exactly the deterministic SIGSEGV in
            // docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md: a grandchild of the
            // process that ran `mmap` (`( cmd; cmd ) &` — shell forks a subshell,
            // subshell forks to exec) faulted on musl's first malloc arena.
            for region in &parent.mmap_regions {
                if region.pages > 0 {
                    total_shared += cow_share_and_demote_range(parent_l0, as_lock, region.start_va,
                        region.len_bytes(), &mut new_proc.address_space, &mut chunk_scratch,
                        "mmap")?;
                }
            }

            for (va_start, len) in &sibling_ranges {
                total_shared += cow_share_and_demote_range(parent_l0, as_lock, *va_start, *len,
                    &mut new_proc.address_space, &mut chunk_scratch, "sibling-mmap")?;
            }

            for region in &parent_lazy_regions {
                total_shared += cow_share_and_demote_range(parent_l0, as_lock, region.start_va, region.size,
                    &mut new_proc.address_space, &mut chunk_scratch, "lazy")?;
            }

            // Belt-and-braces global invalidate on top of the per-chunk range flushes.
            // MUST be flush_tlb_all() (tlbi vmalle1): user processes run under their
            // own non-zero ASID (ttbr0 = (asid<<48)|l0), so flush_tlb_asid(0) only
            // invalidates ASID 0 and MISSES the parent's stale RW entries — the parent
            // would then write through to a still-shared CoW page (no fault), clobbering
            // the child's snapshot (e.g. saved return addresses on a shared stack page)
            // and the child later ret's to a garbage/zero LR → SIGSEGV. (Intermittent
            // because the next context switch's activate()/deactivate() flush_tlb_all
            // closes the window; only writes before the parent is preempted corrupt.)
            mmu::flush_tlb_all();
        }
        // ── BKL re-acquired from here ──

        // CoW fork doesn't track per-region frame lists — frames are shared,
        // not owned.  On write fault, new frames are allocated and tracked in
        // user_frames.  We keep each region's VA *and page count* so munmap can
        // size it and so this child's own forks can share it (see `MmapRegion`).
        new_proc.mmap_regions = inherit_mmap_regions_for_cow_child(&parent.mmap_regions);
        // NOTE: `new_proc.lazy_regions` is deliberately NOT cleared here — the
        // parent's descriptors were propagated into it above, and wiping them
        // reinstates the first-touch SIGSEGV of
        // docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md. (Pre-`LazyRegionMap`
        // this line reset a vestigial `Vec` field that nothing read, while the real
        // state lived in the global table.)
        new_proc.memory.next_mmap.store(parent.memory.next_mmap.load(Ordering::Relaxed), Ordering::Relaxed);

        let cow_elapsed_us = (runtime().uptime_us)() - cow_start_us;
        if lifecycle_trace_on() {
            let mut buf = [0u8; 96];
            let mut pos = 0usize;
            let _ = core::fmt::write(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
                format_args!("[FORK-COW] shared {} pages in {}µs\n", total_shared, cow_elapsed_us));
            if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
        }
    } else {
        // ── Eager-copy fork (legacy path) ──
        //
        // Deliberately NOT carved out of the BKL. `COW_FORK_ENABLED` is `true`
        // (src/config.rs), so this branch is unreachable on every shipping build and
        // there is no contention here to relieve — and unlike the CoW path it copies
        // page *contents* out of the parent, which would need `as_lock` held across the
        // 4 KiB copy (not just the PTE read) to be safe against a peer core's CoW break
        // freeing the source frame mid-copy. Carving it would mean auditing that for a
        // path nothing runs; the playbook's "scope the window as narrowly as possible"
        // says leave it.
        lifecycle_trace("[FORK-DBG] step4: copying stack\n");
        copy_range_phys(
            parent_l0,
            stack_start,
            stack_size,
            &mut new_proc.address_space,
            None,
            "stack",
        )?;
        lifecycle_trace("[FORK-DBG] step4: stack done\n");

        let code_start = if parent.memory.code_end >= 0x1000_0000 {
            0x1000_0000
        } else if parent.memory.code_end < 0x400000 {
            mmu::PAGE_SIZE
        } else {
            0x400000
        };
        if parent.brk > code_start {
            let brk_len = parent.brk - code_start;
            if lifecycle_trace_on() {
                let mut buf = [0u8; 96];
                let mut pos = 0usize;
                let _ = core::fmt::write(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
                    format_args!("[FORK-DBG] step4: brk copy 0x{:x}..0x{:x} ({} pages)\n",
                        code_start, parent.brk, brk_len / mmu::PAGE_SIZE));
                if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
            }
            copy_range_phys(
                parent_l0,
                code_start,
                brk_len,
                &mut new_proc.address_space,
                Some(MAX_FORK_BRK_COPY_PAGES),
                "brk",
            )?;
            lifecycle_trace("[FORK-DBG] step4: brk done\n");
        }

        lifecycle_trace("[FORK-DBG] step4: copying interp\n");
        let interp_base = 0x3000_0000usize;
        let interp_scan_size = 2 * 1024 * 1024;
        if mmu::translate_user_va(parent_l0, interp_base).is_some() {
            copy_range_phys(
                parent_l0,
                interp_base,
                interp_scan_size,
                &mut new_proc.address_space,
                None,
                "interp",
            )?;
        }
        lifecycle_trace("[FORK-DBG] step4: interp done\n");

        const MAX_FORK_MMAP_PAGES: usize = 2048;
        let mmap_snapshot: Vec<MmapRegion> = parent.mmap_regions.clone();
        // RAII so a `?` early-return (OOM mid-copy) can't strand the flag set for the
        // rest of the boot, as the bare store/store pair did.
        let _forking = ForkInProgressGuard::new();
        let mut total_copied_pages: usize = 0;
        let mut child_mmap_regions: Vec<MmapRegion> = Vec::new();

        for (region_idx, region) in mmap_snapshot.iter().enumerate() {
            let va_start = &region.start_va;
            let parent_frames = &region.frames;
            if lifecycle_trace_on() {
                let mut buf = [0u8; 128];
                let mut pos = 0usize;
                let _ = core::fmt::write(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
                    format_args!("[FORK-DBG] mmap region {}/{} va=0x{:x} pages={}\n",
                        region_idx, mmap_snapshot.len(), va_start, parent_frames.len()));
                if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
            }
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
                // Reject frames outside usable RAM, or VAs inside the kernel RAM
                // identity map. Both bounds scale with detected RAM (were hardcoded
                // to a 2GB machine, which mis-rejected valid frames/VAs at >2GB).
                if pf.addr < mmu::ram_base() || pf.addr >= mmu::ram_end() {
                    ok = false;
                    break;
                }
                if page_va >= ProcessMemory::KERNEL_VA_START && page_va < mmu::kernel_va_end() {
                    ok = false;
                    break;
                }
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
                    None => {
                        lifecycle_trace("[FORK-PG] OOM in inner loop\n");
                        ok = false; break;
                    }
                }
            }
            if ok {
                total_copied_pages += child_frames.len();
                // Carry the parent's recorded protection: the child's copy of an
                // eagerly-forked region is the same mapping, and losing `flags` here
                // would cost the child the fault handler's eager-region repair path.
                child_mmap_regions.push(
                    MmapRegion::owned_with_flags(*va_start, child_frames, region.flags));
            } else {
                if config().syscall_debug_info_enabled {
                    log::debug!("[fork] OOM copying mmap region 0x{:x}, skipping rest", va_start);
                }
                break;
            }
        }

        new_proc.mmap_regions = child_mmap_regions;
        // `new_proc.lazy_regions` starts empty and is filled by the propagation
        // just below; clearing it here would undo that (see the CoW arm's note).
        new_proc.memory.next_mmap.store(parent.memory.next_mmap.load(Ordering::Relaxed), Ordering::Relaxed);
        lifecycle_trace("[FORK-DBG] step4: mmap done\n");

        {
            const MAX_FORK_LAZY_PAGES: usize = 4096;
            let lazy_start_us = (runtime().uptime_us)();
            // Clone the full LazyRegion descriptors, not just (va, size) — see
            // `propagate_lazy_regions_to_child`'s doc comment (same propagation gap:
            // eagerly copying whatever's *resident* in the parent's lazy ranges is
            // not enough; the child needs its own lazy-region registration too, for
            // whatever wasn't resident yet in the parent at fork time).
            let parent_regions: alloc::vec::Vec<crate::process::types::LazyRegion> =
                lazy_regions_snapshot(parent_tgid)
                    .map(|m| m.into_values().collect())
                    .unwrap_or_default();
            propagate_lazy_regions_to_child(&parent_regions, &new_proc);
            let num_regions = parent_regions.len();
            let mut lazy_pages_copied = 0usize;
            let mut lazy_pages_scanned = 0usize;
            if lifecycle_trace_on() {
                let mut buf = [0u8; 64];
                let mut pos = 0usize;
                let _ = core::fmt::Write::write_fmt(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
                    format_args!("[FORK-DBG] lazy: {} regions\n", num_regions));
                if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
            }
            'lazy_copy: for (region_idx, region) in parent_regions.into_iter().enumerate() {
                let (va, size) = (region.start_va, region.size);
                let pages = match fork_page_count_for_len(size) {
                    Some(p) => p,
                    None => continue,
                };
                let mapped_pages = mmu::collect_mapped_pages_sparse(parent_l0, va, pages);
                lazy_pages_scanned += pages;
                for (page_va, src_phys) in mapped_pages {
                    if lazy_pages_copied >= MAX_FORK_LAZY_PAGES {
                        break 'lazy_copy;
                    }
                    if let Ok(frame) = new_proc.address_space.alloc_and_map(page_va, mmu::user_flags::RW) {
                        unsafe {
                            let src = mmu::phys_to_virt(src_phys & !0xFFF) as *const u8;
                            let dst = mmu::phys_to_virt(frame.addr);
                            core::ptr::copy_nonoverlapping(src, dst, mmu::PAGE_SIZE);
                        }
                        lazy_pages_copied += 1;
                    }
                }
                if lifecycle_trace_on() && (region_idx % 4 == 3 || region_idx == num_regions - 1) {
                    let mut buf = [0u8; 96];
                    let mut pos = 0usize;
                    let _ = core::fmt::Write::write_fmt(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
                        format_args!("[FORK-DBG] lazy {}/{} copied={} scanned={}\n",
                            region_idx + 1, num_regions, lazy_pages_copied, lazy_pages_scanned));
                    if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
                }
            }
            let lazy_elapsed_us = (runtime().uptime_us)() - lazy_start_us;
            if lifecycle_trace_on() {
                let mut buf = [0u8; 96];
                let mut pos = 0usize;
                let _ = core::fmt::Write::write_fmt(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
                    format_args!("[FORK-DBG] lazy: {} pages copied, {} scanned in {}µs\n",
                        lazy_pages_copied, lazy_pages_scanned, lazy_elapsed_us));
                if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
            }
        }

        lifecycle_trace("[FORK-DBG] step4: lazy done\n");
    }

    lifecycle_trace("[FORK-DBG] step4: done, entering step5\n");

    // 5. Write ProcessInfo to child's process info page.
    //
    // CRITICAL: Re-map PROCESS_INFO_ADDR AFTER the CoW fork.  For Go ARM64
    // binaries, code_start = PAGE_SIZE = 0x1000 = PROCESS_INFO_ADDR.
    // cow_share_and_demote_range copies the parent's PTE for 0x1000 into the child,
    // overwriting the child's process info mapping.  Without this re-map,
    // the child reads the PARENT's PID from PROCESS_INFO_ADDR, causing
    // current_process_shared() / read_current_pid() to return the wrong PID.
    // This broke vfork_complete (wrong child PID → parent never unblocked)
    // and the CoW fault handler (resolved pages in the wrong address space).
    lifecycle_trace("[FORK-DBG] step5a: re-mapping PROCESS_INFO_ADDR\n");
    let map_result = new_proc.address_space.map_page(
        PROCESS_INFO_ADDR,
        new_proc.process_info_phys,
        mmu::user_flags::RO | mmu::flags::UXN | mmu::flags::PXN,
    );
    if map_result.is_err() {
        lifecycle_trace("[FORK-DBG] step5a: map_page FAILED\n");
    }
    lifecycle_trace("[FORK-DBG] step5b: writing ProcessInfo\n");
    unsafe {
        let info_ptr = mmu::phys_to_virt(new_proc.process_info_phys) as *mut ProcessInfo;
        let info = ProcessInfo::new(child_pid, parent_pid, new_proc.box_id);
        core::ptr::write(info_ptr, info);
    }

    lifecycle_trace("[FORK-DBG] step5: done, entering step6\n");

    // 6. Capture parent's user context and create child context
    let parent_tid = crate::threading::current_thread_id();
    lifecycle_trace("[FORK-DBG] step6a: getting context\n");
    let parent_ctx = crate::threading::get_saved_user_context(parent_tid).ok_or("No saved context")?;
    lifecycle_trace("[FORK-DBG] step6b: context captured\n");
    
    let mut child_ctx = parent_ctx;
    child_ctx.x0 = 0;    // fork returns 0 to child
    child_ctx.spsr = 0;  // Clean EL0t with interrupts enabled
    if stack_ptr != 0 {
        child_ctx.sp = stack_ptr;
    }
    // Override the (possibly stale) inherited ttbr0 with the child's *own* address
    // space ttbr0 — the same fix as clone_thread (line ~2146). parent_ctx was read
    // from `THREAD_CONTEXTS[parent_tid].ttbr0`, which is only refreshed when the SGI
    // context-switch code switches *away* from the parent. A parent that execve'd or
    // mmap'd since its last switch-out has a stale value there; loading it on the
    // child's first scheduling wedge the CPU (TLB flush → instruction fetch against a
    // garbage page table → ec=0x20 with IRQs masked → silent VM hang). The child
    // gets a fresh, independent address space from this fork, so its ttbr0 is
    // `new_proc.address_space.ttbr0()`, captured here before `new_proc` is consumed.
    child_ctx.ttbr0 = new_proc.address_space.ttbr0();

    // Store context in the Process struct (entry_point_trampoline uses proc.context)
    new_proc.context = child_ctx;

    lifecycle_trace("[FORK-DBG] step7: spawning child thread\n");
    // 7. Allocate thread but keep it INITIALIZING
    let tid = crate::threading::spawn_user_thread_initializing(
        entry_point_trampoline as extern "C" fn() -> !,
        core::ptr::null_mut(),
        false
    )?;

    new_proc.thread_id = Some(tid);

    // Register in THREAD_PID_MAP so current_process_shared() returns child PID for this thread.
    // Without this, current_process_shared() falls back to reading the parent's PROCESS_INFO_ADDR
    // (not yet updated) and returns the parent PID, causing vfork_complete to fire on the
    // wrong child PID and leaving the parent permanently blocked.
    with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().insert(tid, child_pid);
    });

    // Copy sigaltstack from parent thread to child thread
    let (parent_sp, parent_size, parent_flags) = crate::threading::get_sigaltstack(parent_tid);
    crate::threading::set_sigaltstack(tid, parent_sp, parent_size, parent_flags);

    crate::threading::update_thread_context(tid, &child_ctx);

    // 8. Create a ProcessChannel for exit notification only.
    // The child keeps parent.channel (set in struct init above) for I/O so its
    // stdout writes are visible on the same SSH stream as the parent.
    // The exit-tracking channel is separate to avoid contaminating the I/O channel.
    let exit_channel = Arc::new(ProcessChannel::new());
    register_channel(tid, exit_channel.clone());
    register_child_channel(child_pid, exit_channel, parent_pid);

    // Register process BEFORE marking thread READY
    lifecycle_trace("[FORK-DBG] step8: registering process\n");
    register_process(child_pid, new_proc);
    // No `clone_lazy_regions(parent_pid, child_pid)` here: both fork arms above
    // already propagated the parent *thread-group leader*'s descriptors into
    // `new_proc` before registration. Re-cloning from `parent_pid` would replace
    // that with the forking thread's own (per-thread, possibly stale) map — lazy
    // regions are tgid-keyed (`sys_mmap` uses `proc.tgid`), so the leader's map is
    // the authoritative one.
    
    // Now safe to start the thread
    lifecycle_trace("[FORK-DBG] step8: marking child READY\n");
    // POSIX: a fork()/vfork() child inherits the parent's signal mask, and it must be
    // in place BEFORE the child can run — the slot claim scrubs the mask to 0, and the
    // syscall-layer seed lands after this point, so without this the child is briefly
    // runnable with everything UNBLOCKED. `Command::spawn` blocks all signals right
    // before forking precisely to keep the pre-exec child from taking one.
    crate::threading::seed_thread_signal_mask(tid, crate::threading::thread_signal_mask());
    crate::threading::mark_thread_ready(tid);
    lifecycle_trace("[FORK-DBG] fork_process EXIT ok\n");

    Ok(child_pid)
}

/// vfork fast-path (docs/COW_OPTIMIZATIONS.md Fix B).
///
/// Creates a child that **shares** the parent's address space (same L0 page
/// tables, via `new_shared`) instead of replicating it.  This is sound only for
/// the `CLONE_VFORK` contract — the parent is suspended until the child execs or
/// `_exit`s, so they never run concurrently, and the child runs on a
/// caller-provided stack and must not disturb the parent's live memory.
///
/// Compared to `fork_process` this skips the entire CoW share, the RW→RO demote
/// of the parent, the parent TLB flush, and the later teardown.  Because the
/// parent is never demoted, it takes **zero CoW faults** when it resumes.  On
/// `exec`, `replace_image` installs a fresh address space and drops this shared
/// view (refcount--), leaving the parent's pages untouched.
///
/// Identity: the child shares the parent's `PROCESS_INFO` page, so it is
/// resolved via `THREAD_PID_MAP` (see `read_current_pid`, which prefers the map
/// → tgid when the fast-path is enabled).  We deliberately do **not** remap
/// `PROCESS_INFO_ADDR` (that would corrupt the parent's pid in the shared L0)
/// and do **not** clone lazy regions (the child shares the parent's already
/// mapped pages; faulting new memory before exec would violate the vfork
/// contract and mutate the shared L0 — a stray fault instead fails safely).
pub fn vfork_process(child_pid: u32, stack_ptr: u64) -> Result<u32, &'static str> {
    // Serialize lifecycle against preemption under shared-kernel SMP — see
    // `process/lifecycle.rs`. vfork's state mutations (shared-AS clone, parent
    // suspension, child registration) must not be exposed mid-flight.
    let _lifecycle = LifecycleGuard::acquire();
    if (runtime().is_memory_low)() {
        return Err("Kernel memory low, cannot vfork");
    }
    let parent = current_process_shared().ok_or("No current process")?;
    let parent_pid = parent.pid;

    // Share the parent's L0 page table (same mechanism as CLONE_THREAD).
    let parent_l0_phys = parent.address_space.l0_phys();
    let new_address_space = mmu::UserAddressSpace::new_shared(parent_l0_phys)
        .ok_or("Failed to create shared address space")?;
    mmu::as_trace(format_args!("[AS-NEW] pid={} l0=0x{:x} asid=0x{:x} via=vfork parent={}\n",
        child_pid, parent_l0_phys, new_address_space.asid(), parent_pid));

    let mut new_proc = Box::try_new(Process {
        pid: child_pid,
        pgid: parent.pgid,
        tgid: child_pid, // a new process (own thread group), not a thread
        name: parent.name.clone(),
        parent_pid,
        state: ProcessState::Ready,
        context: UserContext::default(), // set below
        address_space: new_address_space,
        entry_point: parent.entry_point,
        brk: parent.brk,
        initial_brk: parent.initial_brk,
        memory: parent.memory.clone(),
        // Shares the parent's ProcessInfo page; identity comes from
        // THREAD_PID_MAP.  exec installs a fresh page.
        process_info_phys: parent.process_info_phys,
        args: parent.args.clone(),
        cwd: parent.cwd.clone(),
        stdin: parent.stdin.clone(),
        stdout: parent.stdout.clone(),
        exited: false,
        exit_code: 0,
        dynamic_page_tables: Vec::new(),
        mmap_regions: Vec::new(),  // shares parent's via the shared L0
        lazy_regions: Spinlock::new(LazyRegionMap::new()),
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
        signal_actions: Arc::new(SharedSignalTable::new()),
        signal_mask: parent.signal_mask,
        fault_mutex: Spinlock::new(BTreeMap::new()),
        vm_lock: Spinlock::new(()),
        as_lock: Spinlock::new(()),
        sigaltstack_sp: parent.sigaltstack_sp,
        sigaltstack_flags: parent.sigaltstack_flags,
        sigaltstack_size: parent.sigaltstack_size,
        start_time_us: (runtime().uptime_us)(),
        current_syscall: core::sync::atomic::AtomicU64::new(!0),
        last_syscall: core::sync::atomic::AtomicU64::new(0),
        syscall_stats: ProcessSyscallStats::new(),
    }).map_err(|_| "Failed to allocate Process struct (ENOMEM)")?;

    // Child context: inherit the parent's, return 0, clean EL0t, optional new SP.
    let parent_tid = crate::threading::current_thread_id();
    let parent_ctx = crate::threading::get_saved_user_context(parent_tid).ok_or("No saved context")?;
    let mut child_ctx = parent_ctx;
    child_ctx.x0 = 0;
    child_ctx.spsr = 0;
    if stack_ptr != 0 {
        child_ctx.sp = stack_ptr;
    }
    // Same stale-ttbr0 bug as clone_thread/fork_process: parent_ctx.ttbr0 comes
    // from THREAD_CONTEXTS[parent_tid], only refreshed on context-switch-out, so
    // it can be stale if the parent execve'd/mmap'd since its last switch-out.
    // vfork's child shares the parent's L0 table under a *new* ASID (new_shared
    // above), so new_proc.address_space.ttbr0() is the live, canonical value —
    // use that instead of the possibly-stale inherited one.
    child_ctx.ttbr0 = new_proc.address_space.ttbr0();
    new_proc.context = child_ctx;

    let tid = crate::threading::spawn_user_thread_initializing(
        entry_point_trampoline as extern "C" fn() -> !,
        core::ptr::null_mut(),
        false,
    )?;
    new_proc.thread_id = Some(tid);

    // Register tid→pid BEFORE the thread runs so read_current_pid/current_process
    // resolve the child's identity from its first instruction (the shared
    // ProcessInfo page shows the parent's pid).
    with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().insert(tid, child_pid);
    });

    let (parent_sp, parent_size, parent_flags) = crate::threading::get_sigaltstack(parent_tid);
    crate::threading::set_sigaltstack(tid, parent_sp, parent_size, parent_flags);
    crate::threading::update_thread_context(tid, &child_ctx);

    let exit_channel = Arc::new(ProcessChannel::new());
    register_channel(tid, exit_channel.clone());
    register_child_channel(child_pid, exit_channel, parent_pid);

    // register_process must complete before mark_thread_ready so the child's
    // first read_current_pid → THREAD_PID_MAP → with_process(child_pid) resolves.
    register_process(child_pid, new_proc);

    // POSIX: a fork()/vfork() child inherits the parent's signal mask, and it must be
    // in place BEFORE the child can run — the slot claim scrubs the mask to 0, and the
    // syscall-layer seed lands after this point, so without this the child is briefly
    // runnable with everything UNBLOCKED. `Command::spawn` blocks all signals right
    // before forking precisely to keep the pre-exec child from taking one.
    crate::threading::seed_thread_signal_mask(tid, crate::threading::thread_signal_mask());
    crate::threading::mark_thread_ready(tid);
    Ok(child_pid)
}

// ============================================================================
// CLONE_THREAD hand-off snapshot (thread-spawn SIGSEGV diagnostics)
// ============================================================================
//
// `docs/runbooks/debug-thread-spawn-segv.md`: a freshly cloned thread dies at
// `Thread::new::thread_start`'s first instruction pair — `ldr x20,[x0]` then an
// atomic fetch-add at `[x20]` — because `[x0]` is not a pointer. Every theory in
// that runbook turns on one question the fault dump cannot answer: **was the
// argument the child ran with the one its parent handed over?**
//
// musl's `__clone` stores the child's entry and argument at the top of the new
// stack (`stp x0,x3,[x1,#-16]!`) and the child pops them (`ldp x1,x0,[sp],#16`),
// so `stack` — the value the kernel is handed — *is* the address of that pair.
// Snapshot it in the parent's context here, re-read it at the fatal fault, and
// the three candidates separate cleanly:
//
//   - words changed between clone and fault  ⇒ the child's stack page is not the
//     page the parent wrote (aliasing / stale TLB / double-allocated demand page)
//   - words identical, `[arg]` is a string    ⇒ true use-after-free of the packet
//   - `ttbr0_live != ttbr0_proc` at the fault ⇒ wrong address space entirely
//
// Cost is three stores and one 16-byte user read per `pthread_create`; the read
// side runs only on the fatal-SIGSEGV path.
static CLONE_SNAP_STACK: [AtomicU64; crate::threading::MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; crate::threading::MAX_THREADS]
};
static CLONE_SNAP_FN: [AtomicU64; crate::threading::MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; crate::threading::MAX_THREADS]
};
static CLONE_SNAP_ARG: [AtomicU64; crate::threading::MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; crate::threading::MAX_THREADS]
};
/// Packs the creating thread's identity: `(parent_pid as u64) << 32 | parent_tid`.
static CLONE_SNAP_PARENT: [AtomicU64; crate::threading::MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; crate::threading::MAX_THREADS]
};
/// The TTBR0 the child was created to run under.
static CLONE_SNAP_TTBR0: [AtomicU64; crate::threading::MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; crate::threading::MAX_THREADS]
};

/// What `clone_thread` recorded for thread slot `tid`; see [`clone_snapshot`].
#[derive(Clone, Copy, Debug)]
pub struct CloneSnapshot {
    /// The `stack` argument to `clone(2)` — also the address of musl's
    /// `[entry, arg]` pair, since `__clone` pushes them before the `svc`.
    pub stack: u64,
    /// `*(stack)` as the parent left it: musl's `start`/`start_c11`.
    pub entry: u64,
    /// `*(stack + 8)` as the parent left it: musl's `struct start_args *`,
    /// whose `start_arg` field is the Rust thread packet.
    pub arg: u64,
    pub parent_pid: Pid,
    pub parent_tid: u32,
    pub ttbr0: u64,
}

/// Record the hand-off for a slot that `clone_thread` has just claimed. Runs in
/// the *parent's* address space, which is the only context where the words the
/// parent wrote are guaranteed to be the ones under `stack`.
fn record_clone_snapshot(tid: usize, stack: u64, parent_pid: Pid, parent_tid: usize, ttbr0: u64) {
    if tid >= crate::threading::MAX_THREADS { return; }
    let mut words = [0u64; 2];
    // Fault-safe: `stack` is user-supplied and a bogus one must not take down
    // the kernel on a diagnostic read.
    let ok = unsafe {
        crate::mmu::user_access::copy_from_user_safe(
            words.as_mut_ptr().cast::<u8>(), stack as *const u8, 16,
        ).is_ok()
    };
    CLONE_SNAP_STACK[tid].store(stack, Ordering::Release);
    CLONE_SNAP_FN[tid].store(if ok { words[0] } else { u64::MAX }, Ordering::Release);
    CLONE_SNAP_ARG[tid].store(if ok { words[1] } else { u64::MAX }, Ordering::Release);
    CLONE_SNAP_PARENT[tid].store(((parent_pid as u64) << 32) | (parent_tid as u64 & 0xFFFF_FFFF), Ordering::Release);
    CLONE_SNAP_TTBR0[tid].store(ttbr0, Ordering::Release);
}

/// The `clone_thread` hand-off recorded for slot `tid`, or `None` if this slot's
/// current occupant did not arrive through `clone_thread`.
///
/// `clone_thread` is the only writer and it writes before the slot is marked
/// READY, so a value read here belongs to the slot's current occupant unless the
/// slot has since been recycled to a non-clone thread — which `stack == 0`
/// (scrubbed on FREE) does not catch. Treat it as diagnostic, not as an
/// invariant: cross-check `parent_pid` against the faulting process's `tgid`.
pub fn clone_snapshot(tid: usize) -> Option<CloneSnapshot> {
    if tid >= crate::threading::MAX_THREADS { return None; }
    let stack = CLONE_SNAP_STACK[tid].load(Ordering::Acquire);
    if stack == 0 { return None; }
    let parent = CLONE_SNAP_PARENT[tid].load(Ordering::Acquire);
    Some(CloneSnapshot {
        stack,
        entry: CLONE_SNAP_FN[tid].load(Ordering::Acquire),
        arg: CLONE_SNAP_ARG[tid].load(Ordering::Acquire),
        parent_pid: (parent >> 32) as Pid,
        parent_tid: (parent & 0xFFFF_FFFF) as u32,
        ttbr0: CLONE_SNAP_TTBR0[tid].load(Ordering::Acquire),
    })
}

/// Re-read the `[entry, arg]` pair a snapshot points at, in the *current*
/// address space. Returns `None` if the stack page is no longer readable.
pub fn reread_clone_handoff(snap: &CloneSnapshot) -> Option<(u64, u64)> {
    let mut words = [0u64; 2];
    unsafe {
        crate::mmu::user_access::copy_from_user_safe(
            words.as_mut_ptr().cast::<u8>(), snap.stack as *const u8, 16,
        ).ok()?;
    }
    Some(words.into())
}

/// Clone a thread within the same process (CLONE_THREAD | CLONE_VM).
/// The child shares the parent's address space and file descriptors.
///
/// Returns the child's **TID** (kernel thread slot) — the same namespace as
/// `gettid()`, `tkill`, and every per-thread array. Not the child's PID: the
/// thread's process-table entry has its own id, and callers that need it must
/// resolve it through `THREAD_PID_MAP`.
///
/// `flags` is the raw clone(2) flag word. It is load-bearing: `parent_tid_ptr`
/// and `child_tid_ptr` are only written when the caller asked for them
/// (`CLONE_PARENT_SETTID` / `CLONE_CHILD_SETTID`) — see the tid-publication
/// block below for why writing an unrequested `child_tid_ptr` corrupts musl.
pub fn clone_thread(stack: u64, tls: u64, parent_tid_ptr: u64, child_tid_ptr: u64, flags: u64) -> Result<u32, &'static str> {
    // Serialize lifecycle against preemption under shared-kernel SMP — see
    // `process/lifecycle.rs`. CLONE_THREAD creates a new thread + Process and
    // registers it; the half-built child must not be observable by a peer core's
    // EL1 code (e.g. a signal-delivery scan) between allocate and mark-ready.
    let _lifecycle = LifecycleGuard::acquire();
    const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
    const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
    const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
    // Reject stack=0: a thread with SP=0 will immediately crash at a near-zero
    // address (e.g. FAR=0x28) when it tries to access stack variables.  This
    // happens when Go's vfork child leaks -ENOSYS into clone flags, causing
    // clone_thread to be entered with garbage arguments.
    if stack == 0 {
        return Err("clone_thread: stack must be non-zero");
    }
    if (runtime().is_memory_low)() {
        return Err("Kernel memory low, cannot clone thread");
    }
    let parent = current_process_shared().ok_or("No current process")?;
    let parent_pid = parent.pid;
    let child_pid = allocate_pid();

    let parent_l0_phys = parent.address_space.ttbr0() & 0x0000_FFFF_FFFF_F000;
    let shared_as = mmu::UserAddressSpace::new_shared(parent_l0_phys as usize)
        .ok_or("Failed to create shared address space")?;
    mmu::as_trace(format_args!("[AS-NEW] pid={} l0=0x{:x} asid=0x{:x} via=clone parent={}\n",
        child_pid, parent_l0_phys, shared_as.asid(), parent_pid));
    // CLONE_VM: the child shares the parent's address space, so its kernel
    // context ttbr0 MUST be the parent's *actual* top-of-page-table physical
    // base. `get_saved_user_context(parent)` below reads the stale
    // `THREAD_CONTEXTS[parent].ttbr0`, which for a thread that activated a new
    // address space (execve/mmap) since its last context-switch-out can hold a
    // bogus value — loading that on the child's first switch froze the kernel
    // (user space unmapped → fault on ERET → silent hang). Capture the real,
    // canonical ttbr0 here, straight off the still-live address space, before
    // `shared_as` is moved into the child Process.
    let shared_ttbr0 = parent.address_space.ttbr0();

    let parent_tgid = parent.tgid; // inherit thread group leader
    let mut new_proc = Box::try_new(Process {
        pid: child_pid,
        pgid: parent.pgid,
        tgid: parent_tgid, // same thread group as parent
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
        stdin: parent.stdin.clone(), // Share!
        stdout: parent.stdout.clone(), // Share!
        exited: false,
        exit_code: 0,
        dynamic_page_tables: Vec::new(),
        mmap_regions: Vec::new(),
        lazy_regions: Spinlock::new(LazyRegionMap::new()), // per-process; dropped in Process::drop
        fds: parent.fds.clone(), // Arc::clone — shared fd table (CLONE_FILES)
        thread_id: None,
        spawner_pid: parent.spawner_pid,
        terminal_state: parent.terminal_state.clone(),
        box_id: parent.box_id,
        namespace: parent.namespace.clone(),
        channel: parent.channel.clone(),
        delegate_pid: None,
        // Only a CLONE_CHILD_CLEARTID caller gets the zero-and-wake at exit.
        clear_child_tid: if flags & CLONE_CHILD_CLEARTID != 0 { child_tid_ptr } else { 0 },
        robust_list_head: 0,
        robust_list_len: 0,
        signal_actions: parent.signal_actions.clone(), // Shared table (Arc clone)
        signal_mask: parent.signal_mask,
        fault_mutex: Spinlock::new(BTreeMap::new()),
        vm_lock: Spinlock::new(()),
        as_lock: Spinlock::new(()),
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
    // Override the (possibly stale) inherited ttbr0 with the live, canonical
    // shared address-space ttbr0 — see the comment where `shared_ttbr0` is captured.
    child_ctx.ttbr0 = shared_ttbr0;

    new_proc.context = child_ctx;

    let tid = crate::threading::spawn_user_thread_initializing(
        entry_point_trampoline as extern "C" fn() -> !,
        core::ptr::null_mut(),
        false
    )?;

    new_proc.thread_id = Some(tid);

    // Snapshot musl's `[entry, arg]` pair while we are still in the parent's
    // address space — the thread-spawn SIGSEGV diagnostic; see
    // `record_clone_snapshot` and docs/runbooks/debug-thread-spawn-segv.md.
    record_clone_snapshot(tid, stack, parent_pid, parent_tid, shared_ttbr0);

    // DO NOT copy sigaltstack from parent thread to child thread.
    // Each Go M-thread must set up its own sigaltstack during mstart1.
    // If we copy the parent's sigaltstack, the SIGURG guard (alt_sp == 0 check)
    // will think the child is ready for signal delivery, but it actually isn't -
    // Go's M-thread initialization hasn't completed and signal handlers would
    // corrupt the thread's state. Linux also doesn't inherit sigaltstack on clone.
    
    // Verify sigaltstack is clean (should be 0 from thread slot reuse cleanup)
    let (alt_sp, _, _) = crate::threading::get_sigaltstack(tid);
    if alt_sp != 0 {
        // If not clean, force-clear it
        crate::threading::set_sigaltstack(tid, 0, 0, 2); // SS_DISABLE
    }

    crate::threading::update_thread_context(tid, &child_ctx);

    let exit_channel = Arc::new(ProcessChannel::new());
    register_channel(tid, exit_channel);
    // CLONE_THREAD threads are NOT visible to waitpid on Linux — they belong to the same
    // thread group and are never reaped by the parent. Registering them in CHILD_CHANNELS
    // caused wait4(-1) to block forever on git's sideband demux pthread, which never exited
    // because it was waiting for data from a pipe whose write-end git itself held open.

    // Register in THREAD_PID_MAP so current_process_shared() works for this thread
    with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().insert(tid, child_pid);
    });

    register_process(child_pid, new_proc);
    clone_lazy_regions(parent_pid, child_pid);

    // Publish the child's TID — but ONLY where the caller asked for it.
    //
    // Linux keeps three tid flags strictly separate, and the difference is
    // load-bearing for musl:
    //   CLONE_PARENT_SETTID  write child tid to `parent_tid_ptr` at clone
    //   CLONE_CHILD_SETTID   write child tid to `child_tid_ptr`  at clone
    //   CLONE_CHILD_CLEARTID write *zero* to `child_tid_ptr` at **exit**, + futex wake
    //
    // Akuma used to write `child_tid_ptr` unconditionally, i.e. it treated
    // CLEARTID as if it also implied SETTID. musl's `pthread_create` passes
    // CLEARTID *without* SETTID, and the pointer it passes is
    // `&__thread_list_lock` — a global mutex word, not a tid slot. So every
    // thread spawn stamped the new thread's own tid into the thread-list lock.
    //
    // That is worse than garbage, because of musl's `__tl_lock`:
    //     int val = __thread_list_lock;
    //     if (val == tid) { tl_lock_count++; return; }   // "already mine"
    // The value we wrote is *exactly* the child's tid, so the child's very
    // first `__tl_lock()` — the one at the top of `__pthread_exit` — took the
    // recursive fast path and returned **without the lock**, while the parent
    // still held it and was mid-way through linking the child into the thread
    // list. The child then ran the unlink against `self->prev == NULL`:
    //     ldp x0, x1, [x19, #8]   ;  str x0, [x1, #8]
    // and died writing to address 0x8. Second-order damage: the bogus
    // `tl_lock_count++` is never undone, so the parent's `__tl_unlock` only
    // decrements the count and never releases — every later pthread operation
    // in that process blocks forever. Repro:
    // userspace/forktest/c_stress/spawnalias.c;
    // diagnosis in docs/runbooks/debug-thread-spawn-segv.md.
    //
    // Go's `newosproc` passes 0 for both pointers, so it is unaffected either way.
    //
    // This MUST be the kernel thread slot `tid`, not `child_pid`. The slot is
    // Akuma's thread-id namespace: `gettid()` returns `current_thread_id()`, and
    // every per-thread array — pending signals, signal masks, sigaltstacks,
    // wakers — is indexed by it. `child_pid` is a *process-table* id from a
    // different counter, and the two only coincide by accident.
    //
    // Publishing `child_pid` here made musl cache the wrong value in
    // `pthread_self()->tid`, so every later `tkill(self->tid, …)` addressed an
    // unrelated slot: `abort()` on a spawned thread pended SIGABRT on some other
    // process's thread (observed landing on sshd, which then spun forever with a
    // stuck pending signal) while the aborting thread saw nothing and fell
    // through to musl's `a_crash()`. Repro: userspace/forktest/c_stress/abortsig.c.
    //
    // Plain EL1 store is safe here: the bits-32+ guard in sys_clone_pidfd
    // prevents garbage flags from entering clone_thread, so the caller is
    // always a legitimate CLONE_THREAD|CLONE_VM request with writable pages.
    // copy_to_user_safe was tried here but its byte-by-byte strb through
    // the fault-handler mechanism silently returned EFAULT on some pages,
    // leaving mp.procid=0 and crashing the Go runtime.
    let child_tid = tid as u32;
    if parent_tid_ptr != 0 && flags & CLONE_PARENT_SETTID != 0 {
        unsafe { core::ptr::write(parent_tid_ptr as *mut u32, child_tid); }
    }
    if child_tid_ptr != 0 && flags & CLONE_CHILD_SETTID != 0 {
        unsafe { core::ptr::write(child_tid_ptr as *mut u32, child_tid); }
    }

    // POSIX: the new thread inherits the CREATING thread's signal mask — and it must be
    // in place BEFORE the thread can run. `sys_clone` also seeds it on the way out, but
    // that lands *after* `mark_thread_ready` below, so on SMP the child can already be
    // executing with a zeroed mask (slot claim scrubs it) and take a signal its creator
    // had deliberately blocked. Seeding here closes that window; the caller's seed then
    // becomes a harmless idempotent repeat. We are running in the parent's context, so
    // `thread_signal_mask()` is the creating thread's.
    crate::threading::seed_thread_signal_mask(tid, crate::threading::thread_signal_mask());

    crate::threading::mark_thread_ready(tid);

    if config().syscall_debug_info_enabled {
        log::debug!("[syscall] clone_thread: PID {} -> thread PID {} (tid {})", parent_pid, child_pid, tid);
    }

    // Linux returns the child's TID from clone(2), and it must agree with what
    // `gettid()` and CLONE_PARENT_SETTID report — see the TID note above.
    Ok(child_tid)
}

/// Allocate a new unique PID (uses the same global counter as Process::from_elf)
pub fn allocate_pid() -> Pid {
    NEXT_PID.fetch_add(1, Ordering::SeqCst)
}

/// Trampoline for new process threads
/// Called by threading::spawn_user_thread
/// Which process owns thread slot `tid` — `THREAD_PID_MAP` first, table scan second.
///
/// The `p.thread_id == Some(tid)` scan is the historical path and is not safe on its
/// own. `thread_id` is a *recorded* slot number that several teardown paths
/// deliberately leave set (`kill_thread_group` PHASE 2 says so in as many words), and
/// [`table::find_process`] returns the **first ACTIVE slot** that matches — so a stale
/// process at a lower slot index wins. A thread that resolves to that process then
/// runs it: [`Process::run`] activates *its* address space and erets to *its*
/// `Process.context`, which `replace_image` left as `UserContext::new(entry, sp)`,
/// i.e. the image's entry point. For a dynamically linked stale process that entry
/// point is ld-musl's `_dlstart`, so the thread re-runs musl's RELR `*slot += base`
/// loop over an interpreter data page that address space already relocated — one
/// `+= base` per occurrence on one physical word, then an indirect branch through it.
/// That is the `N × INTERP_BASE + 0x6c964` class, and its live signature is exactly
/// this disagreement: `ttbr0_live` (the stale process's, because `run()` activated it)
/// != `ttbr0_proc` (`THREAD_PID_MAP`'s). See
/// `docs/runbooks/debug-thread-spawn-segv.md` §2h.
///
/// `THREAD_PID_MAP` is authoritative: `fork_process`, `vfork_process`, `clone_thread`
/// and both `spawn` paths publish it before the child can be scheduled, and
/// `current_process_shared` already trusts it. A *missing* entry is not evidence of
/// anything, so the scan stays as the fallback; a *disagreeing* entry is, and is
/// logged.
pub fn resolve_thread_process(tid: usize) -> Option<Pid> {
    let map_pid = table::pid_for_thread(tid);
    let scan_pid = table::find_process(|p| {
        if p.thread_id == Some(tid) { Some(p.pid) } else { None }
    });
    if let (Some(m), Some(s)) = (map_pid, scan_pid)
        && m != s
    {
        let mut buf = [0u8; 128];
        let mut pos = 0usize;
        let _ = core::fmt::write(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
            format_args!(
                "[TRAMP-MISMATCH] tid={} THREAD_PID_MAP={} but table scan found {} — using {}\n",
                tid, m, s, m));
        if let Ok(st) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(st); }
    }
    map_pid.or(scan_pid)
}

pub extern "C" fn entry_point_trampoline() -> ! {
    let tid = crate::threading::current_thread_id();
    if lifecycle_trace_on() {
        let mut buf = [0u8; 64];
        let mut pos = 0usize;
        let _ = core::fmt::write(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
            format_args!("[FORK-DBG] trampoline ENTRY tid={}\n", tid));
        if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
    }
    let proc_ptr: *mut Process = resolve_thread_process(tid)
        .and_then(table::get_process_ptr)
        .unwrap_or(core::ptr::null_mut());

    if proc_ptr.is_null() || crate::threading::is_thread_terminated(tid) {
        if proc_ptr.is_null() {
            log::debug!("[process] FATAL: No process found for thread {}", tid);
        }
        crate::threading::mark_current_terminated();
        loop { crate::threading::yield_now(); }
    }
    
    // SIGURG guard: Clear any pending SIGURG if sigaltstack isn't configured.
    // Go's runtime sends SIGURG to newly created M-threads for goroutine preemption,
    // but the thread hasn't finished mstart1 initialization yet (sigaltstack not set).
    // Unlike syscall return paths, this is the first entry to userspace for a new
    // thread, so we handle it here before calling run().
    let (alt_sp, _, _) = crate::threading::get_sigaltstack(tid);
    if lifecycle_trace_on() {
        let mut buf = [0u8; 96];
        let mut pos = 0usize;
        let _ = core::fmt::write(&mut FmtBuf { buf: &mut buf, pos: &mut pos },
            format_args!("[TRAMP] tid={} alt_sp={:#x}\n", tid, alt_sp));
        if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }
    }
    if alt_sp == 0 {
        // Thread's sigaltstack not configured - it's not ready for signal handling.
        // Any SIGURG pended now would corrupt Go's M-thread state.
        // Clear SIGURG (signal 23) if pending - it will be re-sent by Go later.
        let pending = crate::threading::peek_pending_signal(tid);
        if pending == 23 {
            lifecycle_trace("[TRAMP] clearing pending SIGURG\n");
            crate::threading::clear_pending_signal(tid, 23);
        }
    }
    
    unsafe {
        let proc = &*proc_ptr;
        if proc.exited {
            crate::threading::mark_current_terminated();
            loop { crate::threading::yield_now(); }
        }
        (*proc_ptr).run();
    }
}




#[cfg(test)]
mod grace_kill_tests {
    //! Regression tests for the grace-expired hard kill in [`kill_thread_group`].
    //!
    //! The bug: when the 2 s grace expired, PHASE 1 terminated *every* recorded
    //! sibling tid unconditionally. Two thirds of those were not stragglers at all,
    //! and any whose slot had been recycled during the window killed an unrelated
    //! process's thread — leaving that process registered with no thread, so it could
    //! never exit and its parent's `wait4` never returned. Reproduced in-VM as
    //! `[TERM] ... pending_kill=false at=process/mod.rs` followed by a permanent
    //! `[PROC-ORPHAN]` in cargo's thread group.
    //!
    //! Both halves matter: the naive fix (never force anything) would reinstate the
    //! hang the grace path exists to break, so a real straggler on its own slot must
    //! still be terminated.
    use super::*;
    use crate::process::table::THREAD_PID_MAP;
    use crate::threading::{request_thread_kill, take_kill_request_via_tid};

    /// Slots well clear of the low ids the other host tests drive, so parallel
    /// `cargo test` execution can't have two tests arming the same flag.
    const OWN_TID: usize = 121;
    const STOLEN_TID: usize = 122;
    const QUIET_TID: usize = 123;

    fn map_insert(tid: usize, pid: Pid) {
        with_irqs_disabled(|| { THREAD_PID_MAP.lock().insert(tid, pid); });
    }
    fn map_remove(tid: usize) {
        with_irqs_disabled(|| { THREAD_PID_MAP.lock().remove(&tid); });
    }

    #[test]
    fn grace_kill_forces_a_real_straggler_but_spares_recycled_and_quiet_slots() {
        let sib_pid: Pid = 63_200;
        let unrelated_pid: Pid = 63_201;

        // 1. A genuine straggler: request still pending, slot still ours.
        map_insert(OWN_TID, sib_pid);
        request_thread_kill(OWN_TID);

        // 2. Recycled: the request is still armed on the slot, but THREAD_PID_MAP
        //    proves the slot now belongs to an unrelated process. This is the shape
        //    that killed an innocent thread and stranded its process.
        map_insert(STOLEN_TID, unrelated_pid);
        request_thread_kill(STOLEN_TID);

        // 3. Not a straggler: still ours, but the request was already consumed.
        map_insert(QUIET_TID, sib_pid);

        let forced = grace_kill_should_terminate(sib_pid, OWN_TID);
        let spared_recycled = !grace_kill_should_terminate(sib_pid, STOLEN_TID);
        // Still ours, request already consumed, still alive after the 2 s grace.
        // This must ALSO be forced — see below.
        let forced_quiet = grace_kill_should_terminate(sib_pid, QUIET_TID);

        // Cleanup before asserting, so a failure doesn't poison sibling tests.
        take_kill_request_via_tid(OWN_TID);
        take_kill_request_via_tid(STOLEN_TID);
        map_remove(OWN_TID);
        map_remove(STOLEN_TID);
        map_remove(QUIET_TID);

        assert!(forced,
            "a straggler that still owns its slot must be hard-terminated — without \
             this the grace path cannot break the hang it exists for");
        assert!(spared_recycled,
            "a slot recycled to an unrelated process must NOT be terminated: that kills \
             its thread and strands the process with no thread at all");
        // This assertion used to read the other way — "a sibling with no pending kill
        // request is not a straggler and must be left to self-terminate at its own
        // boundary". That rationale holds only if the thread will *reach* a boundary.
        // The request is consumed at the EL1→EL0 return, and a thread parked in an
        // untimed FUTEX_WAIT never gets there: it is woken, re-checks its futex, and
        // re-parks. After the 2 s grace, "still ours, still alive, flag gone" is not
        // evidence of an orderly exit in progress — it is the hang itself, observed as
        // a normal rustc exit whose sibling was still parked 557 s later.
        assert!(forced_quiet,
            "a sibling that still owns its slot must be terminated even with no pending \
             request: the flag is consumed at the EL0 boundary an untimed FUTEX_WAIT \
             never reaches, so sparing it leaves the thread parked forever and its \
             parent's wait4 blocked");
    }
}
