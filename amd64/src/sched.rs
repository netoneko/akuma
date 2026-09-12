//! The **machine half** of the scheduler on amd64.
//!
//! Stage E built a whole scheduler here — task table, states, context switch,
//! picker, park/wake. As of 2026-09-07 it does not have one: `akuma-threading`
//! is the scheduler, on both architectures, and what is left in this file is
//! everything that scheduler is not allowed to know.
//!
//! # What moved, and why it is not a rewrite
//!
//! `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` calls this A1, and it is the
//! trunk the rest of the amd64 self-hosting tree hangs off. The argument for it
//! is not tidiness — it is that this file had grown a **second, weaker copy** of
//! a state machine `akuma-threading` already had, hardened by two years of
//! AArch64 incidents:
//!
//! | this file had | the crate has | the difference |
//! |---|---|---|
//! | `State::Blocked` | `thread_state::WAITING` | — |
//! | `Task::wake_pending` | `WOKEN_STATES` | — |
//! | `Task::wake_at_us` | `WAKE_TIMES` | — |
//! | `wake(slot)` | [`ThreadWaker`]/[`WakeHandle`] | **slot generations**: a wake held across a slot's death is *refused*, not spent on the next occupant |
//! | `state = Runnable` on wake | a `WAITING → READY` **CAS** | a plain store overwrites a concurrent `TERMINATED`, resurrecting a killed thread onto freed page tables |
//!
//! The last two rows are the whole argument. Both are real, documented AArch64
//! failures (`ThreadWaker::wake`'s own comment; `docs/archive/THREAD_STATES_
//! CHECKSTORE_RACES.md`), both are silent, and this file's version had neither
//! defence. Keeping two implementations meant the target most likely to hit
//! them — the one aiming at `cargo -j4` — was running the version that had
//! never been debugged.
//!
//! # The seam
//!
//! The crate schedules; this file performs. Everything the scheduler cannot
//! know about is registered once as [`akuma_threading::X86ArchHooks`] and lives
//! in [`Machine`], a per-slot side table indexed by the crate's own thread id:
//!
//! - `CR3`, and the rule that a kernel thread runs in [`KERNEL_ROOT`];
//! - the ring-3 trap stack the TSS points at;
//! - `IA32_FS_BASE`/`IA32_GS_BASE`, userspace's TLS;
//! - the `fxsave` area, because the kernel is soft-float and a thread's SSE
//!   registers would otherwise follow whichever core it last ran on;
//! - the Big Kernel Lock's recursion depth, which is per-thread while the lock
//!   itself stays with the core across a switch;
//! - which slot each core is running, and which slot idles it.
//!
//! That list is the same shape as the seams table in
//! `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` § "What stays different forever":
//! not work items, pinned differences.
//!
//! # What did **not** move
//!
//! Preemption. [`preempt_if_needed`] still runs from this target's own timer
//! vector, and still switches only ring 3 and the idle loop — a kernel thread
//! preempted inside the heap allocator leaves the heap's spinlock held, and the
//! next thread to allocate spins on it forever with interrupts off. AArch64
//! reaches its scheduler through an SGI and a fake IRQ-return frame; x86_64
//! reaches it through a plain call. Both are in the crate; only one is used
//! here.

#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;
use akuma_threading as threading;

// The scheduler itself does not touch the LAPIC — the timer is started and
// stopped around `smoke_test`/`block_smoke_test` by `boot::self_tests`, and
// only those two reach it from here.
#[cfg(not(feature = "no-tests"))]
use crate::lapic;
use crate::paging;
use crate::smp::{self, NO_CPU};
use crate::usermode::UserCtx;
// The shared ring-3 register file. `akuma-exec-core` rather than `akuma-exec`
// because that is where the type lives and this file needs nothing else from
// the bigger crate; the two paths name one struct.
use akuma_exec_core::process::UserContext;
use alloc::vec;
use core::sync::atomic::{AtomicU64, Ordering};

/// Per-thread kernel stack. Generous: these are `Vec` allocations from a large
/// heap, and a stack overflow here has no guard page to catch it.
pub const STACK_SIZE: usize = 32 * 1024;

/// Maximum threads, including the boot thread in slot 0.
///
/// **Not a constant of this file any more.** It is
/// `akuma_primitives::preempt::MAX_THREADS`, which carries an `x86_64` arm of
/// 512 for exactly this target's reason: on the bare-metal reference box a real
/// session runs dozens of commands, every command is a slot and `fork` takes a
/// second, so the old 96 was a few minutes of work before `spawn` returned
/// `None` and the shell reported `Out of memory` with 1.5 GB free.
///
/// Re-exported under the old name so the ~dozen call sites outside this module
/// did not move, and so the two can never disagree — they were independent
/// literals once, on the AArch64 side, and raising only one silently did
/// nothing.
pub const MAX_TASKS: usize = akuma_primitives::preempt::MAX_THREADS;

/// One thread's x87/SSE register file, in `fxsave` layout.
///
/// The kernel is soft-float and never touches these registers, so a thread's
/// SSE state used to survive a preemption by accident: whatever was in the xmm
/// registers when the tick landed was still there when it resumed — *on the
/// same core*. A thread that resumes on another core finds that core's
/// registers instead, and a thread interleaved with a second SSE-using process
/// on one core never had that luck. Saved and restored around every switch, 512
/// bytes per thread, 16-byte aligned as the instruction requires.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct FxArea([u8; 512]);

impl FxArea {
    /// The reset state a program expects: x87 control word `0x37F`, `MXCSR`
    /// `0x1F80` (every SSE exception masked). Not all-zero: an all-zero `MXCSR`
    /// unmasks every exception, and the first inexact result in ring 3 would
    /// raise `#XM` — an "unhandled vector" halt, from a program that did
    /// nothing wrong.
    const fn initial() -> Self {
        let mut a = [0u8; 512];
        a[0] = 0x7F;
        a[1] = 0x03;
        a[24] = 0x80;
        a[25] = 0x1F;
        Self(a)
    }
}

/// The machine state `akuma-threading` deliberately knows nothing about, one
/// entry per crate thread slot.
///
/// Everything the *scheduler* needs — state, context, wake flags, deadlines,
/// generation, `ON_CPU` — is in the crate. This is only what a switch has to
/// install on the CPU, plus the three scheduling *facts* the crate asks about
/// through [`akuma_threading::X86ArchHooks::can_run`].
struct Machine {
    /// Page-table root to install when this thread runs. `0` means "the
    /// kernel's", which is what every kernel thread uses.
    space_root: u64,
    /// Where the syscall path saves this thread's stacks.
    uctx: UserCtx,
    /// Stack the CPU switches to when this thread traps from ring 3.
    trap_stack_top: u64,
    /// Base of this slot's kernel stack, and of its trap stack, or `0` if it has
    /// never been given one.
    ///
    /// Kept so a **recycled** slot reuses the stacks it already owns. Without
    /// these, reclaiming a slot would `leak()` two fresh 32 KiB stacks every
    /// time and turn a slot leak into a memory leak — strictly worse than the
    /// exhaustion it was meant to fix.
    stack_base: usize,
    trap_base: usize,
    /// This thread's SSE/x87 registers while it is not running.
    fx: FxArea,
    /// The Big Kernel Lock hold depth this thread was suspended at; reinstalled
    /// on the core that resumes it. The lock itself stays with the core.
    /// Runs for the life of the kernel and never finishes — the netpoll loop,
    /// and every core's idle thread. [`all_user_tasks_finished`] ignores these
    /// so the boot's drive loop still ends when the *shell* exits rather than
    /// spinning against a thread that is alive on purpose.
    daemon: bool,
    /// The thread that runs a core when nothing else will. Never chosen by the
    /// round-robin scan; switched to explicitly, by its own core.
    idle: bool,
    /// The only core allowed to run this thread, or [`NO_CPU`]. The boot thread
    /// is pinned to the boot core — it drives the self-tests against that core's
    /// LAPIC — and every idle thread to its own.
    pinned: u32,
}

impl Machine {
    const fn empty() -> Self {
        Self {
            space_root: 0,
            uctx: UserCtx::new(),
            trap_stack_top: 0,
            stack_base: 0,
            trap_base: 0,
            fx: FxArea::initial(),
            daemon: false,
            idle: false,
            pinned: NO_CPU,
        }
    }
}

/// `static mut`, reached only through raw pointers, **under the BKL**. Every
/// writer is kernel code, kernel code holds the lock, and a switch keeps it
/// held — so the table sees one core at a time.
static mut MACHINE: [Machine; MAX_TASKS] = [const { Machine::empty() }; MAX_TASKS];

/// Raw pointer to the machine table.
///
/// The `&raw mut` lives behind a function for one reason: writing
/// `(*(&raw mut MACHINE))[i]` inline trips `clippy::deref_addrof`, whose
/// suggested fix — index `MACHINE` directly — reintroduces the `static_mut_refs`
/// violation the raw pointer exists to avoid.
fn machines() -> *mut [Machine; MAX_TASKS] {
    &raw mut MACHINE
}

/// The kernel's own page-table root, captured at [`init`].
///
/// A thread with `space_root == 0` runs in this. Recorded rather than re-read
/// from `CR3` at switch time, because by then `CR3` holds whatever the
/// *outgoing* thread was using.
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);

/// The kernel's page-table root. What every core other than the one that built
/// it needs in order to leave its boot tables.
#[must_use]
pub fn kernel_root() -> u64 {
    KERNEL_ROOT.load(Ordering::Relaxed)
}

fn current() -> usize {
    smp::current_task()
}

// ---------------------------------------------------------------------------
// The hooks: everything `akuma-threading` cannot do for itself on this target
// ---------------------------------------------------------------------------

/// Install `to`'s machine state on this core, with `from` still current and
/// before the stack moves.
///
/// The order inside is the contract, and every line of it was a bug once:
///
/// - `CR3` **before** the switch — every address space shares the kernel's
///   upper-half mappings, so the stack and the code stay mapped across the
///   write. Written unconditionally for a process root even when `CR3` already
///   holds that value: the write is what flushes the TLB, and "same value" does
///   not mean "same address space" — a freed root frame can be the next
///   process's root (see [`finish`]). Only kernel-root to kernel-root skips it.
/// - `%fs` restored only when set. `IA32_FS_BASE` is one per-core register and
///   `arch_prctl` is its only writer, so a thread that set it keeps its value
///   only as long as nothing else runs. A shell and the child it forked each
///   have their own TLS; without this the child's `execve` leaves the parent on
///   the child's base.
/// - `%gs` restored **unconditionally**, because the kernel's own `%gs` is the
///   other half of the pair and a stale user value would follow a thread that
///   never set one onto another core.
/// - `fxsave` of `from` before `fxrstor` of `to`, for the reason [`FxArea`]
///   gives.
fn hook_switch_to(from: usize, to: usize) {
    // SAFETY: raw-pointer access to the machine table under the BKL. `from` and
    // `to` are crate slot indices, which are in range by construction.
    unsafe {
        let m = machines();

        let want = match (*m)[to].space_root {
            0 => KERNEL_ROOT.load(Ordering::Relaxed),
            root => root,
        };
        if (*m)[to].space_root != 0 || want != paging::active_root() {
            paging::activate(want);
        }

        // Where a ring-3 trap by the incoming thread will land, on this core.
        crate::gdt::set_kernel_stack((*m)[to].trap_stack_top);

        // Repoint the per-thread syscall context: the incoming thread may
        // resume inside its own syscall and read it on the way back to ring 3.
        smp::set_current_uctx(&raw mut (*m)[to].uctx);

        let fs = (*m)[to].uctx.fs_base;
        if fs != 0 {
            crate::usermode::set_fs_base(fs);
        }
        crate::usermode::set_user_gs_base((*m)[to].uctx.gs_base);

        let fx_out = &raw mut (*m)[from].fx;
        let fx_in = &raw const (*m)[to].fx;
        core::arch::asm!(
            "fxsave64 [{out}]",
            "fxrstor64 [{inp}]",
            out = in(reg) fx_out,
            inp = in(reg) fx_in,
            options(nostack, preserves_flags)
        );
    }
}

/// Hand the kernel lock's recursion depth from the outgoing thread to the
/// incoming one. The lock itself stays with this core.
///
/// A no-op since 5b: the BKL is `akuma_bkl`'s, reentrant by owner core, so
/// there is no depth to transfer — the lock stays with the core across the
/// switch and the bookkeeping this hook used to move does not exist. The hook
/// is kept because `akuma-threading`'s `X86ArchHooks` requires it.
fn hook_transfer_lock_depth(_from: usize, _to: usize) {}

/// May this core run `slot`?
///
/// Idle threads are excluded outright rather than pinned-out: they are reached
/// only through [`akuma_threading::X86ArchHooks::idle_slot`], never by the
/// round-robin scan, or a core with work to do would hand itself to its own
/// idle thread.
fn hook_can_run(slot: usize) -> bool {
    let cpu = smp::cpu_index() as u32;
    // SAFETY: raw-pointer read under the BKL.
    unsafe {
        let m = &(*machines())[slot];
        !m.idle && (m.pinned == NO_CPU || m.pinned == cpu)
    }
}

/// Register this target's machine effects with the scheduler. Called once, from
/// [`init`], before any thread but the boot thread exists.
fn register_hooks() {
    threading::register_x86_arch_hooks(threading::X86ArchHooks {
        switch_to: hook_switch_to,
        current_slot: smp::current_task,
        set_current_slot: smp::set_current_task,
        idle_slot: smp::idle_task,
        can_run: hook_can_run,
        transfer_lock_depth: hook_transfer_lock_depth,
        allow_tick,
        write_user_context,
        read_user_context,
        prepare_task_slot,
    });

    // The scheduler also wants a clock and a console. The other four fields are
    // the AArch64 GIC's vocabulary, and on this target they are **no-ops with a
    // reason**, not stubs:
    //
    // `trigger_sgi` asks a core to enter its scheduler. AArch64's scheduler
    // *is* an interrupt handler, so a wake has to raise one; x86_64's is a plain
    // function call, so a woken thread simply becomes visible to the next
    // `x86_yield_now` pick and there is nothing to raise. `wake_core` is the
    // cross-core version of the same request and answers the same way;
    // `wake_remote_idle` reports that there was no idle core to nudge; and
    // `end_of_interrupt` has no interrupt to end.
    //
    // These were `unreachable!()` for exactly one boot, on the theory that a
    // field the x86 path never reaches should say so loudly. It reaches
    // `trigger_sgi` on the very first wake: `ThreadWaker::wake` raises one
    // unconditionally after a successful `WAITING → READY` CAS, because on
    // AArch64 that is how the woken thread gets looked at. The self-test caught
    // it — `[PANIC] sched.rs` immediately after "a parked task is never picked"
    // — which is the argument for keeping the park checks in the boot suite
    // rather than trusting a green build.
    threading::register(
        threading::ThreadRuntime {
            uptime_us: crate::net::uptime_us,
            trigger_sgi: |_| {},
            wake_core: |_| {},
            wake_remote_idle: || false,
            end_of_interrupt: |_| {},
            print_str: |s| crate::serial::puts(s),
        },
        threading::ThreadConfig {
            reserved_threads: 0,
            kernel_stack_size: STACK_SIZE,
            system_thread_stack_size: STACK_SIZE,
            user_thread_stack_size: STACK_SIZE,
            boot_stack_base: 0,
            boot_stack_top: 0,
            enable_stack_canaries: false,
            stack_canary: 0,
            canary_words: 0,
            network_thread_ratio: 0,
            prioritize_never_scheduled: false,
            deferred_thread_cleanup: false,
            thread_cleanup_cooldown_us: 0,
            syscall_debug_info_enabled: false,
            enable_sgi_debug_prints: false,
        },
    );

    // The scheduler's eight questions for the process layer, answered by
    // `akuma-exec` — **the same table the AArch64 kernel registers**, from the
    // same function, so the two cannot drift.
    //
    // This was a hand-written literal until 2026-09-11, five of whose eight rows
    // were `|_| false` / `|_| None` under two reasons that had both expired:
    // "this target has no signal delivery at all" (it has since
    // `AKUMA_AMD64_SIGNAL_DELIVERY.md`) and "`akuma-exec`'s process table is not
    // built here" (it has been since 5b slice 1). Two of the five were doing
    // real damage while they read as deliberate:
    //
    // * `is_current_interrupted` is **the one hook the x86 park loop reads**
    //   (`akuma_threading`'s `schedule_blocking`). Answering `false` meant a
    //   thread parked in this target's scheduler was never interrupted out of
    //   its wait, which is the shape of "`^C` is raised and the sleeping job
    //   does not die".
    // * `clear_draining` / `drain_in_flight` guard
    //   `akuma_exec::process::reclaim::drain_retired`, which this target calls
    //   from seven places. As no-ops the reaper could free a stack out from
    //   under a live sweep, and a thread killed mid-sweep left the
    //   re-entrancy flag set forever on a slot its next occupant inherits.
    //
    // The other three are diagnostics: `find_pid_by_thread` gates the `[kill]`
    // cross-thread-kill tracer (which therefore printed nothing here),
    // `pid_for_thread` the `[TERM]` lifecycle trace, and `proc_dump_info` /
    // `dump_orphan_processes` feed `dump_thread_resume_points`, which is a stub
    // on x86_64 — those last two have no reader on this target and are wired for
    // uniformity, not effect. `docs/archive/AKUMA_AMD64_STALE_FALSE_HOOKS.md`.
    akuma_exec::register_process_hooks();
}

// ---------------------------------------------------------------------------
// Preemption — this target's own timer vector, deliberately not the crate's
// ---------------------------------------------------------------------------

/// Called from the timer interrupt: switch threads if the tick asked for it and
/// the interrupted code may be switched away from.
///
/// Runs **inside an interrupt handler**, which is what makes this preemption
/// rather than a cooperative yield. The suspended thread is left sitting on its
/// own trap stack with its interrupt frame intact; when it is scheduled again it
/// returns from here, the handler returns, and `iretq` resumes whatever it was
/// doing.
///
/// Only ring 3 (`from_user`) and the idle loop are switched away from here. A
/// kernel thread keeps running and consumes the flag at its next `yield_now` —
/// see the module header for the deadlock that ring-0 preemption was.
pub fn preempt_if_needed(from_user: bool) {
    if !smp::need_resched() {
        return;
    }
    // SAFETY: raw-pointer read of the machine table; the BKL is not needed to
    // read a slot this core itself is running.
    let idle = unsafe { (*machines())[current()].idle };
    if from_user || idle {
        // Ring 3 does not hold the BKL and the idle loop dropped it for `hlt`;
        // the switch needs it. Recursive on the boot core's own kernel threads,
        // harmless there too.
        smp::bkl_enter();
        PREEMPTIONS.fetch_add(1, Ordering::Relaxed);
        yield_now();
        smp::bkl_leave();
    }
}

/// Switches performed from the timer interrupt rather than from a `yield_now`.
static PREEMPTIONS: AtomicU64 = AtomicU64::new(0);

/// How many times the timer has taken a thread off the CPU.
#[must_use]
pub fn preemptions() -> u64 {
    PREEMPTIONS.load(Ordering::Relaxed)
}

/// Yields that found the tick's reschedule request set — the kernel-thread
/// counterpart of a preemption: the timer asked, the thread obliged.
static TICK_YIELDS: AtomicU64 = AtomicU64::new(0);

/// Have all threads except the boot thread and any daemons finished?
///
/// Daemon slots (the netpoll loop, the idle threads) are alive for the whole run
/// by design, so they are skipped — otherwise `run_init`'s drive loop would
/// never see the shell exit.
///
/// Asked as "is this slot **alive**" rather than "is it not runnable". The two
/// are the same question only while a parked state does not exist; it does, and
/// a thread parked on a pipe read is as live as one spinning on it.
#[must_use]
pub fn all_user_tasks_finished() -> bool {
    // SAFETY: raw-pointer read of the machine table; under the BKL.
    unsafe { (1..MAX_TASKS).all(|s| (*machines())[s].daemon || !threading::x86_slot_is_live(s)) }
}

// ---------------------------------------------------------------------------
// Blocking — thin wrappers over the crate's park/wake
// ---------------------------------------------------------------------------

/// How long a park with no deadline of its own waits before the scheduler makes
/// it runnable anyway.
///
/// **This is a tripwire, not a design.** A correct wait has a wake path: the
/// pipe that gains a byte, the futex that is signalled, the child that exits. If
/// that path is missing, a park with no deadline is an unrecoverable hang with
/// no output — the failure mode `thread::drain`'s own comment calls the one that
/// costs the most to diagnose and says the least. With a backstop the same bug
/// degrades to what this kernel did before parking existed: a poll, at 1 Hz
/// instead of at scheduler frequency. The machine stays usable and
/// [`backstop_wakes`] says how often it happened.
///
/// It is a **per-target policy** and only this target sets it. AArch64 parks
/// untimed all the time and has an interrupt-driven scheduler to recover; this
/// target reaches its scheduler only by being called, so a lost wake is
/// terminal here in a way it is not there.
///
/// # It lives in the crate now (4b batch 3a)
///
/// The number is still this file's; the *mechanism* is
/// [`akuma_threading::park_indefinitely`], registered at boot through
/// [`install_untimed_park_backstop`]. It had to move for the `read`/`write`
/// fold: an untimed park spelled here could only cover the four waits this
/// kernel wrote itself, and every blocking arm in `akuma-syscalls-glue` — 22 of
/// them, including the pipe read and write this batch folded — parked
/// `u64::MAX`. Folding onto those arms while the backstop was local would have
/// traded a 1 Hz degradation for a silent, unrecoverable hang, on the exact
/// paths a shell pipeline runs through.
const BACKSTOP_US: u64 = 1_000_000;

/// Threads parked by [`block_current`]/[`block_until_deadline`].
static BLOCKS: AtomicU64 = AtomicU64::new(0);
/// Parked threads released by [`wake`].
static WAKES: AtomicU64 = AtomicU64::new(0);
/// Install [`BACKSTOP_US`] as the deadline every untimed park in the tree gets.
///
/// Called from `boot::install_shared_sinks`, i.e. on **both** boot protocols —
/// the drift that C1 step 3's first arm found the hard way.
pub fn install_untimed_park_backstop() {
    threading::set_untimed_park_backstop_us(BACKSTOP_US);
}

/// How many times a thread has parked.
#[must_use]
pub fn blocks() -> u64 {
    BLOCKS.load(Ordering::Relaxed)
}

/// How many parked threads a [`wake`] has released.
#[must_use]
pub fn wakes() -> u64 {
    WAKES.load(Ordering::Relaxed)
}

/// How many untimed parks the [`BACKSTOP_US`] tripwire has released. Nonzero
/// means a wait somewhere is not being woken; see that constant.
///
/// Counted by the crate since 4b batch 3a, so this number now covers every
/// untimed park in the tree — `akuma-syscalls-glue`'s 22 included — and not
/// just the ones spelled in this file.
#[must_use]
pub fn backstop_wakes() -> u64 {
    threading::untimed_park_backstop_wakes()
}

/// Park the running thread until [`wake`] names it.
///
/// Returns when the thread is runnable again, which is **not** a promise that
/// the condition it waited for holds: a caller must re-test in a loop. A wake
/// can be spurious (the pipe table wakes every registered waiter on every event,
/// each to re-test its own condition), and [`BACKSTOP_US`] can release the park
/// with nothing having happened at all.
///
/// # There is no `prepare_block`
///
/// There was, before the fold, and it was this file's own answer to the
/// lost-wakeup window between registering as a waiter and parking. The crate
/// closes that window without the caller's help: a waker's first act is a sticky
/// `WOKEN_STATES` flag, which [`akuma_threading::schedule_blocking`] tests on
/// entry **and again atomically with publishing `WAITING`**
/// (`publish_waiting_and_take_pending_wake`). So the correct shape here is
/// simply
///
/// ```text
///   if !pipe::check_set_reader(id) {   // test AND register, in one step
///       sched::block_current();        // park
///   }
/// ```
///
/// and there is no third step to forget.
///
/// # Rules for callers
///
/// - **Hold the BKL and nothing else.** This switches away, and the thread that
///   runs next may want any lock this one is holding. The pipe shim releases
///   `PIPES` before it parks, which is why `akuma_pipes` returns its wakes
///   rather than firing them.
/// - **Not from an interrupt handler, and not from an idle thread.** An idle
///   thread is the fallback the switch itself uses; parking one has nowhere to
///   go.
pub fn block_current() {
    BLOCKS.fetch_add(1, Ordering::Relaxed);
    // The deadline arithmetic and the "was it the backstop" accounting are the
    // crate's since 4b batch 3a — see [`BACKSTOP_US`] for why they had to be.
    // This function keeps only [`BLOCKS`], which counts *this kernel's own*
    // parks and is what `sched: threads parked` reports.
    threading::park_indefinitely();
}

/// Park the running thread until [`wake`] names it or `deadline_us` passes.
///
/// `deadline_us` is absolute, on the same clock as `net::uptime_us` — which is
/// the clock `akuma_threading`'s own wake-pass compares against, so the two
/// cannot drift. Resolution is the LAPIC tick (10 ms), so a shorter timeout
/// rounds up to one tick rather than returning instantly.
pub fn block_until_deadline(deadline_us: u64) {
    BLOCKS.fetch_add(1, Ordering::Relaxed);
    threading::schedule_blocking(deadline_us);
}

/// Make thread `slot` runnable if it is parked. Returns whether it was.
///
/// Safe to call for a thread that is not parked, and that case is **not** a
/// no-op: it records the crate's sticky wake flag, which is what stops a wake
/// being lost to a thread that has registered as a waiter but not yet parked.
///
/// Goes through a [`akuma_threading::WakeHandle`], so a wake naming a slot whose
/// thread has since exited and been recycled is *refused* rather than spent on
/// the new occupant — the protection this file's own `wake` did not have.
pub fn wake(slot: usize) -> bool {
    let was_parked = threading::x86_slot_is_waiting(slot);
    threading::wake_by_handle(threading::wake_handle_for_thread(slot));
    if was_parked {
        WAKES.fetch_add(1, Ordering::Relaxed);
    }
    was_parked
}

/// Is thread `slot` parked? For the self-tests and for diagnostics.
#[must_use]
pub fn is_blocked(slot: usize) -> bool {
    threading::x86_slot_is_waiting(slot)
}

// ---------------------------------------------------------------------------
// Identity and per-thread state
// ---------------------------------------------------------------------------

/// The running thread's slot on this core.
#[must_use]
pub fn current_task() -> usize {
    current()
}

/// A raw pointer to slot `slot`'s `UserCtx`, for `smp` to seed a core's
/// `current_uctx` with before that core runs.
#[must_use]
pub fn uctx_ptr(slot: usize) -> *mut UserCtx {
    // SAFETY: the table is a `static`; the pointer is into it.
    unsafe { &raw mut (*machines())[slot].uctx }
}

/// Called from the LAPIC timer handler, on the core whose timer fired.
pub fn set_need_resched() {
    smp::set_need_resched();
}

/// Register the currently-executing thread as slot 0, pinned to the boot core.
pub fn init() {
    KERNEL_ROOT.store(paging::active_root(), Ordering::Relaxed);
    register_hooks();
    // SAFETY: raw-pointer access to the machine table; under the BKL, before
    // any switch.
    unsafe {
        let m = &mut (*machines())[0];
        m.pinned = 0;
        // The boot thread needs a live user context too: it is what the very
        // first `enter_user_mode` publishes its kernel stack into.
        smp::set_current_uctx(&raw mut m.uctx);
    }
    smp::set_current_task(0);
    threading::x86_adopt_running_thread(0);
}

/// Allocate the idle thread for core `cpu` — the context that core is already
/// executing when it arrives in `ap_entry64`, so the slot gets an empty context
/// that the first switch away from it fills in.
///
/// Pinned to its core, a daemon (so it does not hold the boot up), running from
/// the start (its core is about to be executing it), and `idle` so the picker
/// leaves it alone. Called by the boot core, under the BKL, before the core is
/// started.
pub fn register_idle_task(cpu: usize) -> Option<usize> {
    let slot = threading::x86_claim_slot()?;
    // SAFETY: raw-pointer access under the BKL; the slot is INITIALIZING, so
    // nothing can schedule it.
    unsafe {
        let m = &mut (*machines())[slot];
        m.daemon = true;
        m.idle = true;
        m.pinned = cpu as u32;
        m.space_root = 0;
        m.trap_stack_top = 0;
    }
    // Adopted rather than published: this is a context that already exists and
    // is about to be running, not one the scheduler will enter through a seeded
    // stack frame. `x86_adopt_running_thread` latches `ON_CPU` for it, which is
    // what stops a peer core picking up a stack its own core is executing.
    threading::x86_adopt_running_thread(slot);
    Some(slot)
}

/// What a secondary core runs forever: hand the core to any runnable thread, and
/// sleep until the next tick when there is none.
///
/// Entered holding the BKL at depth 1. The lock is released across `hlt` — that
/// is the whole point of the idle loop under a BKL: a core that has nothing to
/// do must not hold the one lock everyone else needs — and taken back before the
/// loop looks at the thread table again. `sti; hlt` is atomic with respect to
/// the tick: `sti` takes effect after the following instruction, so a tick
/// cannot slip between them and leave the core asleep past it.
pub fn idle_loop() -> ! {
    loop {
        // BKL-hold attribution: the idle thread never passes a syscall entry,
        // so without this its (dropped-window, reclaim) holds read `tag=511`.
        akuma_bkl::sync::set_holder_tag(
            crate::smp::cpu_index_u32(),
            akuma_bkl::sync::HOLD_TAG_IDLE,
        );
        // 5b slice 1: the idle loop is reclaim site 2 (`process::reclaim`'s
        // vetted list). Every exit's terminal drain (`run_process`) and the
        // boot drive loop's `yield_now` keep the RETIRED set near-empty while
        // processes die; this is the collector that runs when nothing else
        // does — the regime where the cooldown has always elapsed.
        akuma_exec::process::reclaim::drain_retired_if_requested();
        if !threading::x86_yield() {
            smp::bkl_leave();
            // SAFETY: interrupts on for exactly the `hlt`, then off again. The
            // timer vector is installed and its handler takes the BKL itself.
            unsafe {
                core::arch::asm!("sti", "hlt", "cli", options(nomem, nostack));
            }
            smp::bkl_enter();
        }
    }
}

/// Create a thread that runs in a page table of its own, left **unpublished**
/// so the caller can finish initialising it before anything can schedule it.
/// Finish with [`publish_task`].
///
/// `space_root` is installed in `CR3` whenever this thread is scheduled. The
/// address space must share the kernel's mappings — see
/// `akuma_mmu::UserAddressSpace::SHARED_PML4_SLOTS` — or the switch faults on the
/// instruction after `mov cr3`.
///
/// **There is no publish-immediately variant, on purpose.** There was one until
/// 2026-09-06, and the gap it leaves has bitten this target twice for the same
/// reason — a thread became schedulable before the caller had finished
/// describing it:
///
/// - `space_root` was once written *after* the spawn, and a LAPIC tick landing
///   between the two scheduled the thread with `space_root` still 0 — kernel
///   `CR3` — so `run_init`'s `sysret` into ring 3 fetched sshd's entry point
///   unmapped. Measured 2026-09-04, intermittent by timer phase: one boot served
///   ssh, the next died on the first user instruction.
/// - Every process thread now needs [`seed_proc_slot`] before it runs, because
///   `usermode::proc_entry` reads its process index out of its own `UserCtx`.
///
/// With a second core the window is not a tick away but zero instructions away.
pub fn spawn_in_space_unpublished(entry: extern "C" fn() -> !, space_root: u64) -> Option<usize> {
    spawn_unpublished(entry, space_root, false)
}

/// Make a [`spawn_in_space_unpublished`] slot schedulable.
pub fn publish_task(task_slot: usize) {
    threading::x86_publish(task_slot);
}

/// Repoint the **running** thread's address space, and install it in `CR3` now.
///
/// `execve` rebuilds a spawned child's process in a fresh address space without
/// a `fork`; the thread keeps running but must switch page tables. The switch is
/// done here rather than left to the next `yield_now` because the caller
/// (`usermode::run_process`) re-enters ring 3 at a VA that only the new space
/// maps — a stale `CR3` would `#PF` on the first user instruction.
///
/// Safe to switch mid-flight: every space shares the kernel's upper-half
/// mappings, so the kernel stack this runs on stays mapped across `mov cr3`.
pub fn set_current_space_root(space_root: u64) {
    // SAFETY: raw-pointer access under the BKL.
    unsafe {
        (*machines())[current()].space_root = space_root;
    }
    let want = if space_root == 0 {
        KERNEL_ROOT.load(Ordering::Relaxed)
    } else {
        space_root
    };
    if want != paging::active_root() {
        // SAFETY: `want` is either a live `UserAddressSpace` root (from `execve`'s
        // freshly-loaded image) or the kernel's own; both share the upper-half
        // mappings, so the kernel stack and code stay mapped across the write.
        unsafe {
            paging::activate(want);
        }
    }
}

/// Seed a not-yet-running `fork`/`vfork`/`clone(CLONE_VM)` child with the
/// ring-3 register file it must resume on, so it comes back as a true copy of
/// the parent's context (see `usermode::enter_user_mode_forked`). The child
/// inherits `%fs` because musl's post-fork fixups are `%fs`-relative, and the
/// register set because a C compiler assumes r12-r15/rbx survive the `syscall`.
///
/// `forked` is set in the same call rather than by a separate one, and that is
/// the point of it being here: the flag says "use the values above", so a path
/// that seeded the registers and forgot the flag — or set the flag with no
/// registers behind it — cannot be written. Before 5b slice 4 the flag was a
/// field on this target's own `Process`, one table away from the registers it
/// refers to.
///
/// # This is `akuma_threading::update_thread_context`'s x86_64 arm
///
/// It took a `(fs_base, gs_base, &[u64; 12])` triple until 2026-09-10 and was
/// called `seed_forked_task`. It takes the shared [`UserContext`] instead
/// because that type **is** this triple — `akuma-exec-core`'s x86_64 arm is
/// exactly what `syscall_entry` saves — and because the shared child-spawn
/// path (`akuma_exec::process::spawn_child_thread_and_publish`) describes a
/// child that way. One writer, reached from two spellings, is what stops the
/// tree's `fork` and this target's own from seeding a child differently;
/// registered as [`threading::X86ArchHooks::write_user_context`] and called
/// directly by `usermode::sys_fork` until that folds (slice 3).
///
/// # `pc`/`sp` are deliberately not copied here
///
/// A `UserContext` carries them and this writes neither. `UserCtx::user_rip` /
/// `user_rsp` are the *syscall entry* capture — written by the assembly on
/// every `syscall`, read by `sys_fork` to find where its own caller resumes —
/// and the authority for where a task (re-)enters ring 3 is
/// `ProcessImage::context`, which `usermode::enter_ring3` is handed and reads.
/// Writing them here would put a second copy of that in a second structure,
/// which is the staleness bug `UserContext::set_address_space_root`'s doc
/// describes, one field along.
pub fn write_user_context(task_slot: usize, ctx: &UserContext) {
    // `rax` has no home in `UserCtx`: both ring-3 entry points hard-code the
    // value ring 3 resumes with (`enter_user_mode_forked` does `xor eax, eax`,
    // `enter_user_mode` takes `entry_rax` and every caller passes 0), which
    // matches every context shared code builds — `set_child_return_zero` is
    // the only writer of the field. Say so if that ever stops being true,
    // rather than dropping a value silently.
    if ctx.rax != 0 {
        crate::serial::puts("  [sched] write_user_context: non-zero rax dropped\n");
    }
    // SAFETY: raw-pointer access; under the BKL, and the slot is unpublished so
    // no core can be running it.
    unsafe {
        if let Some(m) = (*machines()).get_mut(task_slot) {
            m.uctx.fs_base = ctx.fs_base;
            m.uctx.gs_base = ctx.gs_base;
            m.uctx.saved_regs = ctx.regs;
            m.uctx.forked = 1;
        }
    }
}

/// Read a task slot's ring-3 register file back out as the shared
/// [`UserContext`] — **`akuma_threading::get_saved_user_context`'s x86_64
/// arm**, and the exact mirror of [`write_user_context`] above.
///
/// `None` for a slot that has never trapped in from ring 3. `user_rip == 0` is
/// what says so: the `syscall_entry` assembly writes it on every `syscall`, so
/// a zero there means the assembly has never run for this task — a kernel
/// thread, or a process task that was published moments ago and has not
/// reached its first instruction. A child built from that slot would resume at
/// address 0 with a zero stack, which is exactly the silent birth the AArch64
/// side refuses when it finds no live EL0 trap frame. `usermode::sys_fork`
/// made the same check by hand, one field at a time, before this existed.
///
/// `user_rsp` is checked too and for the same reason, and both are checked
/// rather than one: `enter_user_mode_forked` consumes the pair, and a child
/// with a good `rip` and a zero `rsp` faults on its first `push` instead of
/// its first fetch — a different-looking crash with the same cause.
///
/// `rax` is `0` and cannot be anything else: the assembly does not save it (it
/// carries the syscall number in and the return value out), so there is no
/// parent value to read. That is also the value a `fork` child wants, which
/// callers state for themselves with `UserContext::set_child_return_zero`
/// rather than lean on here.
pub fn read_user_context(task_slot: usize) -> Option<UserContext> {
    // SAFETY: raw-pointer read under the BKL. Reading the *running* slot is
    // sound and is the common case — `fork` reads its own caller's capture,
    // which the entry assembly wrote before this Rust ran and which nothing
    // rewrites until the `sysret`.
    let uctx = unsafe { (*machines()).get(task_slot).map(|m| m.uctx)? };
    if uctx.user_rip == 0 || uctx.user_rsp == 0 {
        return None;
    }
    Some(UserContext {
        regs: uctx.saved_regs,
        sp: uctx.user_rsp,
        pc: uctx.user_rip,
        fs_base: uctx.fs_base,
        gs_base: uctx.gs_base,
        rax: 0,
    })
}

/// Give a freshly claimed slot its two stacks and its default machine state,
/// and answer the top of its kernel stack — **the stack half of a spawn**,
/// registered as [`threading::X86ArchHooks::prepare_task_slot`].
///
/// It is a hook because stacks are this target's on this architecture: a slot
/// leaks its pair on first use and a recycled slot reuses the pair it already
/// owns, where AArch64 takes stacks from `akuma-threading`'s own PMM-backed
/// pool. So the crate can claim a slot and seed a context into it, but it
/// cannot make one somewhere to stand.
///
/// [`spawn_unpublished`] is the other caller and calls it for the same reason,
/// which is the point of the split: this target's own spawn path and the
/// shared `spawn_user_closure_initializing` give a slot **one** initial
/// machine state, so a field reset in one and forgotten in the other cannot
/// exist.
///
/// Every field the picker or the switch reads is written here, not just the
/// two stacks — `space_root`, `pinned`, `daemon`, `idle`, the saved `UserCtx`
/// and the FPU area. A recycled slot must not inherit the previous occupant's:
/// a stale `space_root` puts the new task in a freed address space, a stale
/// `pinned` strands it on a core, and a stale `uctx` hands it a dead process's
/// `proc_slot`. The caller overrides `space_root`/`daemon` afterwards, while
/// the slot is still INITIALIZING.
pub fn prepare_task_slot(slot: usize) -> Option<usize> {
    // SAFETY: raw-pointer read; the slot is INITIALIZING and not running.
    let (have_stack, have_trap) = unsafe {
        let m = (*machines()).get(slot)?;
        (m.stack_base, m.trap_base)
    };

    // Leaked deliberately on first use: a thread's stack must outlive the frame
    // that made it. Bounded by `MAX_TASKS`, not by how many processes ever ran,
    // because a recycled slot arrives here with its pair already set.
    let stack_base = if have_stack == 0 {
        vec![0u8; STACK_SIZE].leak().as_ptr() as usize
    } else {
        have_stack
    };
    let stack_top = stack_base + STACK_SIZE;

    // A second stack, for traps taken while this thread is in ring 3. Separate
    // from its kernel stack because a preempted thread is suspended on the
    // interrupt frame, and the two must not overlap.
    let trap_base = if have_trap == 0 {
        vec![0u8; STACK_SIZE].leak().as_ptr() as usize
    } else {
        have_trap
    };
    let trap_top = (trap_base + STACK_SIZE) & !0xf;

    // SAFETY: raw-pointer access under the BKL; the slot is INITIALIZING, so
    // nothing runs on it.
    unsafe {
        let m = &mut (*machines())[slot];
        m.trap_stack_top = trap_top as u64;
        m.stack_base = stack_base;
        m.trap_base = trap_base;
        m.space_root = 0;
        m.daemon = false;
        m.idle = false;
        m.pinned = NO_CPU;
        m.uctx = UserCtx::new();
        m.fx = FxArea::initial();
    }
    Some(stack_top)
}

/// Point an **unpublished** slot's page-table root at `root`.
///
/// The slot-indexed counterpart of [`set_current_space_root`], and deliberately
/// without its `mov cr3`: this task is not running, so the switch that picks it
/// up installs the root from here. Writing `CR3` on its behalf would install a
/// foreign address space on the core doing the spawning.
///
/// A task the shared child-spawn path created arrives with `space_root` 0 —
/// kernel `CR3` — because [`prepare_task_slot`] has no `Process` to read one
/// from. This is what supplies it, and forgetting it is not subtle: the first
/// switch into the child runs ring-3 code with the kernel's page tables.
pub fn set_task_space_root(task_slot: usize, root: u64) {
    // SAFETY: raw-pointer access under the BKL; the slot is unpublished, so no
    // core can be running it.
    unsafe {
        if let Some(m) = (*machines()).get_mut(task_slot) {
            m.space_root = root;
        }
    }
}

/// Seed a not-yet-running **clone child** with the two identities its entry
/// path reads out of its own `UserCtx`: which process it belongs to, and which
/// `thread::THREADS` row is its.
///
/// The `seed_thread_task` half that this is *not*: no `fs_base`, no `gs_base`,
/// no register snapshot. The shared child-spawn path already wrote all three
/// through `akuma_threading::update_thread_context`
/// ([`write_user_context`]) — including the `CLONE_SETTLS` base, which reaches
/// it as `UserContext::set_tls_base` -> `fs_base`. Writing them again here
/// would be a second authority for the same fields, and the `fs_base` one is
/// the field where the two primitives genuinely disagree (a `fork` child
/// inherits the parent's; a thread must not), so it belongs with the context
/// that states which primitive it is.
pub fn seed_thread_slots(task_slot: usize, proc_slot: usize, thread_slot: usize) {
    // SAFETY: raw-pointer access under the BKL; the slot is unpublished, so no
    // core can be running it.
    unsafe {
        if let Some(m) = (*machines()).get_mut(task_slot) {
            m.uctx.proc_slot = proc_slot;
            m.uctx.thread_slot = thread_slot;
        }
    }
}

/// Seed a not-yet-running **process** thread with the process slot it serves.
///
/// The counterpart of [`seed_thread_task`]'s `proc_slot`/`thread_slot` pair, and
/// the reason `usermode` needs only one process entry function. Until 2026-09-06
/// a process index had nowhere to live but the `fn` pointer itself, so
/// `usermode::proc_entry_for` baked one into each of sixteen hand-written
/// trampolines — and since only nine of them were ever handed out, the machine
/// could not run more than nine processes at once. `cargo -j4` is cargo plus
/// four `rustc`s plus their children before anything interesting happens.
///
/// Deliberately not folded into [`write_user_context`]: a spawned process needs
/// this and *not* a register snapshot (it starts at a fresh entry point, not at
/// a copy of its parent's context), so one function taking both would make two
/// unrelated requirements look like one call.
pub fn seed_proc_slot(task_slot: usize, proc_slot: usize) {
    // SAFETY: raw-pointer access under the BKL; the slot is unpublished, so no
    // core can be running it.
    unsafe {
        if let Some(m) = (*machines()).get_mut(task_slot) {
            m.uctx.proc_slot = proc_slot;
        }
    }
}

/// Release a [`spawn_in_space_unpublished`] slot that will never be published.
///
/// The crate marks it `TERMINATED` rather than `FREE`, which is the same
/// distinction this file used to draw with `Finished`: the slot owns two leaked
/// 32 KiB stacks and the next occupant reuses them, so handing it back as
/// pristine would leak the pair.
pub fn abandon_unpublished(task_slot: usize) {
    // SAFETY: raw-pointer access under the BKL; never published, so no core can
    // be running it.
    unsafe {
        if let Some(m) = (*machines()).get_mut(task_slot) {
            m.space_root = 0;
            m.uctx = UserCtx::new();
        }
    }
    threading::x86_abandon(task_slot);
}

/// Create a daemon thread: one that runs for the life of the kernel and is not
/// counted by [`all_user_tasks_finished`]. The netpoll loop is the only caller.
///
/// The `daemon` flag is written before publication, not after: the flag's whole
/// job is to keep this thread out of that tally, and a tick landing between the
/// spawn and the flag write would publish a not-yet-daemon thread — the boot
/// drive loop would then wait for a death a never-exiting daemon never reaches.
pub fn spawn_daemon(entry: extern "C" fn() -> !) -> Option<usize> {
    spawn_ready(entry, 0, true)
}

/// [`spawn`] with the space root and daemon flag set before the slot becomes
/// schedulable. Every field the scheduler or the picker reads is in place before
/// publication — that ordering is the whole point; see
/// [`spawn_in_space_unpublished`].
fn spawn_ready(entry: extern "C" fn() -> !, space_root: u64, daemon: bool) -> Option<usize> {
    let slot = spawn_unpublished(entry, space_root, daemon)?;
    publish_task(slot);
    Some(slot)
}

/// Create a thread in the kernel's own address space (`space_root` 0).
pub fn spawn(entry: extern "C" fn() -> !) -> Option<usize> {
    spawn_ready(entry, 0, false)
}

/// Claim a slot, give it stacks, seed its entry context and fill in its machine
/// state — everything but publication.
///
/// Slot **recycling** is the crate's now (`x86_claim_slot` takes a `TERMINATED`
/// slot no core is executing), but the stacks are still this file's: a recycled
/// slot reuses the pair it already owns, so the leak is bounded by
/// [`MAX_TASKS`] rather than by how many processes ever ran.
///
/// Before recycling existed, nothing ever released a slot: the table was a
/// ceiling on **total processes for the life of the boot**, and a shell hit it
/// at ~500 `fork`s with 1.5 GB free and the process table almost empty. `sh`
/// reported `can't fork: Out of memory`, naming the one resource that was not
/// exhausted.
fn spawn_unpublished(
    entry: extern "C" fn() -> !,
    space_root: u64,
    daemon: bool,
) -> Option<usize> {
    let slot = threading::x86_claim_slot()?;

    // The stacks and every default the picker reads — shared with the crate's
    // own `spawn_user_closure_initializing`, so this target's spawn path and
    // the tree's cannot describe a fresh slot two different ways.
    let Some(stack_top) = prepare_task_slot(slot) else {
        threading::x86_abandon(slot);
        return None;
    };

    threading::x86_seed_entry(slot, stack_top, entry);

    // The two `prepare_task_slot` cannot know: the caller's address space and
    // whether this thread counts towards the boot's live-task tally. Written
    // while the slot is still INITIALIZING, which is the whole reason there is
    // no publish-immediately variant.
    // SAFETY: raw-pointer access under the BKL; the slot is INITIALIZING, so
    // nothing runs on it.
    unsafe {
        let m = &mut (*machines())[slot];
        m.space_root = space_root;
        m.daemon = daemon;
    }
    Some(slot)
}

/// Mark the running thread finished and switch away for good.
///
/// The thread's address space is somebody else's to free from here on — the
/// parent's `waitpid`, a self-test's teardown — and they may do so the moment
/// the switch drops the BKL, which is before this core has moved. So this core
/// leaves the space *first*: with `CR3` on the kernel root, a freed and reused
/// PML4 frame is nothing this core will ever walk again. Measured 2026-09-05
/// (`SMP=4`): without this, a reaped root frame came back as the next spawn's
/// root, the same `CR3` value skipped the flush, and the new process ran on the
/// dead one's TLB entries — `hello` read a garbage `argc`, busybox `#GP`'d
/// walking `argv`.
pub fn finish() -> ! {
    // SAFETY: raw-pointer access; under the BKL.
    unsafe {
        (*machines())[current()].space_root = 0;
        // SAFETY: the kernel root maps this stack and everything below.
        paging::activate(KERNEL_ROOT.load(Ordering::Relaxed));
    }
    threading::x86_finish_current()
}

/// Let a pending timer tick be delivered, then mask again.
///
/// # The bug this exists for
///
/// `net::uptime_us()` is the LAPIC tick counter, and it only advances when the
/// timer vector runs. A syscall runs with `IF` clear (`IA32_FMASK`), and the
/// **only** place the scheduler re-enables it is [`idle_loop`]'s `sti; hlt; cli`
/// — which the picker reaches only when nothing else is runnable. So a
/// kernel-side drive loop that spins while *any* other thread is also spinning
/// in the kernel sees a **frozen clock**: two threads bounce off each other
/// through `yield_now` forever, the idle thread is never picked, and every
/// deadline computed against `uptime_us` is unreachable.
///
/// Measured 2026-09-06 by `scripts/futex_suite.py`'s `futexops`: a `FUTEX_WAIT`
/// with a 400 ms timeout never returned, because the only other runnable thread
/// was in `nanosleep` — which on this target is `yield_now`. The probe reported
/// it as a requeue bug; the requeue was fine and the clock was stopped.
///
/// # Why this is safe with the BKL held
///
/// `timer_dispatch` takes **no lock**: `lapic::on_tick` is two atomic increments,
/// and [`preempt_if_needed`] returns immediately unless the tick landed in ring
/// 3 or in the idle loop — neither of which is true of a caller here. So the tick
/// advances the clock and switches nothing. Interrupt gates mask `IF` for the
/// handler's duration, so it cannot nest, and every thread has its own trap
/// stack.
///
/// Deliberately **not** folded into [`yield_now`]: that is also called from
/// inside the timer handler ([`preempt_if_needed`]), where opening an interrupt
/// window would let the vector nest, which the gate type exists to prevent.
///
/// `sti` takes effect only after the following instruction, so the `nop` is the
/// window and a pending tick is recognised at the boundary before `cli`.
pub fn allow_tick() {
    // SAFETY: interrupts on for exactly one instruction. The timer vector is
    // installed, takes no lock, and will not switch away from kernel code.
    unsafe {
        core::arch::asm!("sti", "nop", "cli", options(nomem, nostack));
    }
}

/// Switch to the next runnable thread, round-robin.
///
/// A no-op when nothing else is runnable — deliberately, so a lone thread
/// calling this in a loop makes progress rather than deadlocking against itself.
///
/// **Every yield lets the other cores in first.** The lock is dropped and
/// retaken before the thread table is even looked at — and the ticket lock is
/// FIFO, so a core already spinning for it gets it now, not after this core's
/// next idea. The first version dropped it only when there was nothing to switch
/// to, and that was a livelock, measured 2026-09-05 with `SMP=4 STRACE=1`: a
/// shell and the boot thread took turns on the boot core, each yield finding the
/// other runnable, while the shell's forked child sat on core 2 spinning in
/// `syscall_handler`'s `bkl_enter` for the `execve` it never got to make. Two
/// threads on one core is enough to keep a lock that is only released "when
/// idle" held forever.
///
/// The drop window is here rather than inside the crate's switch because it is
/// this target's lock: `akuma-threading` has no idea the BKL exists beyond the
/// depth it is handed.
pub fn yield_now() {
    smp::bkl_drop_window();
    if smp::take_need_resched() {
        TICK_YIELDS.fetch_add(1, Ordering::Relaxed);
    }
    threading::yield_now();
}

// ---------------------------------------------------------------------------
// Smoke test
// ---------------------------------------------------------------------------

const ROUNDS: u64 = 4;
const WORKERS: usize = 3;

/// Per-worker round counters.
static mut COUNTERS: [u64; WORKERS] = [0; WORKERS];
/// Per-worker checksums, accumulated in a *local* across yields.
static mut CHECKSUMS: [u64; WORKERS] = [0; WORKERS];

/// Body shared by the three workers.
///
/// The accumulator is a **local**. It is read and written across a `yield_now`,
/// so if the switch failed to preserve this task's stack or its callee-saved
/// registers, the checksum comes out wrong — which is the property that
/// distinguishes a real context switch from a function call that happens to
/// return.
///
/// Each round yields explicitly. An earlier version *waited* for the timer to
/// request a reschedule before yielding, which stopped working the moment
/// preemption existed: the flag was consumed inside the interrupt handler, so a
/// worker polling for it in ring 0 could never observe it and spun out its
/// whole budget. That the tick drives scheduling is now measured by
/// `TICK_YIELDS` instead — yields that found the request set — which counts the
/// thing directly rather than inferring it from a flag two parties race for.
fn worker_body(id: usize) -> ! {
    let mut acc: u64 = 0;
    for round in 0..ROUNDS {
        acc = acc.wrapping_mul(31).wrapping_add(round + id as u64);

        // SAFETY: under the BKL, one writer per index.
        unsafe {
            (*(&raw mut COUNTERS).cast::<[u64; WORKERS]>())[id] += 1;
        }

        yield_now();
    }
    // SAFETY: as above.
    unsafe {
        (*(&raw mut CHECKSUMS).cast::<[u64; WORKERS]>())[id] = acc;
    }
    finish();
}

extern "C" fn worker0() -> ! {
    worker_body(0);
}
extern "C" fn worker1() -> ! {
    worker_body(1);
}
extern "C" fn worker2() -> ! {
    worker_body(2);
}

/// What `worker_body` should produce for `id`, computed independently.
fn expected_checksum(id: u64) -> u64 {
    let mut acc: u64 = 0;
    for round in 0..ROUNDS {
        acc = acc.wrapping_mul(31).wrapping_add(round + id);
    }
    acc
}

#[cfg(not(feature = "no-tests"))]
/// Spawn three tasks, run them to completion, verify.
pub fn smoke_test(t: &mut Suite) {
    // `init()` is **not** called here any more — `kmain` does it, once, long
    // before this runs. Registration with `akuma-threading` is once-only, and
    // the network bring-up yields before the self-tests start.
    let spawned = [worker0 as extern "C" fn() -> !, worker1, worker2]
        .into_iter()
        .filter(|&f| spawn(f).is_some())
        .count();

    if !t.check_eq("sched: tasks spawned", spawned as u64, WORKERS as u64) {
        return;
    }

    // Enable interrupts so the tick runs while the workers do, then drive the
    // round-robin from the boot task until every worker has finished.
    // SAFETY: IDT loaded, PICs masked, timer vector installed.
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }

    let tick_yields_before = TICK_YIELDS.load(Ordering::Relaxed);
    let mut switches = 0u64;
    loop {
        let done = (1..=WORKERS).all(|s| !threading::x86_slot_is_live(s));
        if done || switches > 10_000 {
            break;
        }
        switches += 1;
        yield_now();
    }

    // SAFETY: masking interrupts is the conservative direction.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }

    // SAFETY: workers have finished; nothing else writes these.
    let (counters, checksums) = unsafe {
        (
            *(&raw const COUNTERS).cast::<[u64; WORKERS]>(),
            *(&raw const CHECKSUMS).cast::<[u64; WORKERS]>(),
        )
    };

    t.check(
        "sched: every worker ran every round",
        counters.iter().all(|&c| c == ROUNDS),
    );
    // The property that distinguishes a real context switch from a call that
    // returns: the accumulator is a local, read and written across a yield.
    for (i, &sum) in checksums.iter().enumerate() {
        t.check_eq("sched: locals survive the switch", sum, expected_checksum(i as u64));
    }
    t.note("sched: switches", switches);
    t.note("sched: ticks", lapic::ticks());
    // Kernel tasks are not preempted (module header), so what the tick can be
    // seen to do here is *request*: a yield that found the flag set is the tick
    // driving the schedule.
    t.check(
        "sched: the tick requested a reschedule at least once",
        TICK_YIELDS.load(Ordering::Relaxed) > tick_yields_before,
    );
    t.note("sched: tick-driven yields", TICK_YIELDS.load(Ordering::Relaxed) - tick_yields_before);
    t.note("sched: preemptions", preemptions());
}

// ---------------------------------------------------------------------------
// Blocking smoke test
// ---------------------------------------------------------------------------

/// The park worker reached its `block_current`.
static BLOCK_ARMED: AtomicU64 = AtomicU64::new(0);
/// The park worker ran again after being woken.
static BLOCK_RESUMED: AtomicU64 = AtomicU64::new(0);
/// The timeout worker reached its `block_until_deadline`.
static TIMEOUT_ARMED: AtomicU64 = AtomicU64::new(0);
/// The timeout worker's deadline released it.
static TIMEOUT_DONE: AtomicU64 = AtomicU64::new(0);

/// How long the timeout worker asks to wait. Five LAPIC ticks: long enough that
/// it cannot be satisfied by the tick already in flight, short enough that the
/// boot pays 50 ms for the check.
const TEST_TIMEOUT_US: u64 = 50_000;

/// How many drive-loop laps a step gets before the test calls it a failure.
///
/// Bounded, like every other drive loop here. An unbounded wait would turn a
/// missing wake into a boot that hangs with no output; a bounded one leaves a
/// named `[FAIL]` and the rest of the suite still runs.
const DRIVE_LAPS: u64 = 2_000_000;

extern "C" fn park_worker() -> ! {
    // Publish that we are about to park, then park. No arming step: the
    // crate's sticky `WOKEN_STATES` flag is tested on entry to
    // `schedule_blocking` and again atomically with publishing `WAITING`, so a
    // wake landing in this window is recorded, not lost. See `block_current`.
    BLOCK_ARMED.store(1, Ordering::Release);
    block_current();
    BLOCK_RESUMED.store(1, Ordering::Release);
    finish();
}

extern "C" fn timeout_worker() -> ! {
    let deadline = crate::net::uptime_us().saturating_add(TEST_TIMEOUT_US);
    TIMEOUT_ARMED.store(1, Ordering::Release);
    block_until_deadline(deadline);
    TIMEOUT_DONE.store(1, Ordering::Release);
    finish();
}

/// Drive the round-robin until `done`, or until [`DRIVE_LAPS`] laps.
///
/// `allow_tick` on every lap because the deadline half of this test needs the
/// clock to move: a syscall — and this drive loop — runs with `IF` clear, and
/// the only other place the scheduler re-enables it is the idle loop, which is
/// never reached while a worker is runnable. That is the measurement
/// `allow_tick`'s own comment records.
fn drive_until(done: &AtomicU64) -> bool {
    let mut laps = 0;
    while done.load(Ordering::Acquire) == 0 && laps < DRIVE_LAPS {
        laps += 1;
        yield_now();
        allow_tick();
    }
    done.load(Ordering::Acquire) != 0
}

#[cfg(not(feature = "no-tests"))]
/// Prove a task can actually **park** — not spin — and that both ways out of a
/// park work.
///
/// This is the property the whole blocking change exists for, and it is not
/// observable from any of the tests above: before it, `State` was
/// `Unused | Reserved | Runnable | Finished`, so a waiter was always runnable
/// and every wait in this kernel was a poll. What is checked:
///
/// 1. A parked task is **invisible to the picker**. Two hundred yields go by
///    and it does not run. That is the difference between a park and a yield.
/// 2. It still counts as live to [`all_user_tasks_finished`], or the boot's own
///    drive loop would declare a waiting shell finished.
/// 3. [`wake`] releases it, exactly once.
/// 4. A deadline releases it with **no** wake at all — the path every timed
///    `futex` wait and every `poll` timeout takes.
pub fn block_smoke_test(t: &mut Suite) {
    let Some(worker) = spawn(park_worker) else {
        t.check("block: park worker spawned", false);
        return;
    };

    // SAFETY: IDT loaded, timer vector installed — the same window
    // `smoke_test` opens, and for the same reason.
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }

    if !t.check("block: the worker reached its park", drive_until(&BLOCK_ARMED)) {
        // SAFETY: masking is the conservative direction.
        unsafe { core::arch::asm!("cli", options(nomem, nostack)) };
        return;
    }
    // The worker stores `BLOCK_ARMED` and parks under one BKL hold, so by the
    // time this core runs again the park has happened. No race to wait out.
    t.check("block: the worker is parked", is_blocked(worker));
    t.check("block: a parked task is still live to the drive loop", !all_user_tasks_finished());

    // The property that distinguishes a park from a yield: the picker never
    // chooses it, however long the round-robin runs.
    for _ in 0..200 {
        yield_now();
    }
    t.check(
        "block: a parked task is never picked",
        BLOCK_RESUMED.load(Ordering::Acquire) == 0 && is_blocked(worker),
    );

    let wakes_before = wakes();
    t.check("block: wake reports it released a parked task", wake(worker));
    t.check("block: waking a task that is not parked reports false", !wake(0));
    t.check("block: the woken task ran again", drive_until(&BLOCK_RESUMED));
    t.check_eq("block: exactly one park was released", wakes() - wakes_before, 1);

    // The other way out: a deadline, with nothing ever calling `wake`.
    let wakes_before = wakes();
    if let Some(_tw) = spawn(timeout_worker) {
        t.check("block: the timeout worker reached its park", drive_until(&TIMEOUT_ARMED));
        t.check("block: a deadline releases a park", drive_until(&TIMEOUT_DONE));
        t.check_eq("block: and no wake was fired for it", wakes() - wakes_before, 0);
    } else {
        t.check("block: timeout worker spawned", false);
    }

    // SAFETY: masking interrupts is the conservative direction.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }

    t.note("block: parks", blocks());
    t.note("block: wakes", wakes());
    t.note("block: backstop releases", backstop_wakes());
}

/// [`write_user_context`] does what `akuma-threading` asks of it, on a real
/// slot, through the shared entry point.
///
/// # Why this is worth a boot test and the rest of the seam is not
///
/// The other half of the ring-3 entry seam —
/// `usermode::enter_ring3` — is exercised by every ring-3 self-test that
/// follows and by every process the kernel ever starts, so a mistake in it is
/// impossible to miss. This half is the opposite: `update_thread_context` is
/// called by the **shared** `spawn_child_thread_and_publish`, which this
/// target does not reach until `sys_fork` folds (slice 3). Wired but
/// unreachable is exactly the shape that rots — and the failure it would rot
/// into is a child resuming on a register file that is subtly not its
/// parent's, which reads as a userspace bug a long way from here.
///
/// So it is called the way the shared code will call it: by thread id, through
/// `akuma_threading::update_thread_context`, with a `UserContext` whose every
/// field is distinguishable. The last check is the one that pins a *decision*
/// rather than a mapping — `pc`/`sp` are deliberately not copied into
/// `UserCtx`, because `ProcessImage::context` is the authority for where a
/// task enters ring 3 and a second copy of it here is a staleness bug waiting
/// to happen.
#[cfg(not(feature = "no-tests"))]
pub fn user_context_smoke_test(t: &mut Suite) {
    /// Never runs: the slot is abandoned before it is published.
    extern "C" fn unreachable_entry() -> ! {
        finish();
    }

    let Some(slot) = spawn_in_space_unpublished(unreachable_entry, 0) else {
        t.check("uctx: unpublished slot claimed", false);
        return;
    };

    // Every field distinct, and none of them zero: a writer that copied the
    // wrong field, or none, cannot pass by accident.
    let ctx = UserContext {
        regs: [0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab],
        sp: 0x7fff_0000,
        pc: 0x40_1000,
        fs_base: 0x7f00_0000,
        gs_base: 0x7e00_0000,
        rax: 0,
    };
    threading::update_thread_context(slot, &ctx);

    // SAFETY: raw-pointer read under the BKL; the slot is unpublished, so
    // nothing else can be touching it.
    let (regs, fs_base, gs_base, forked, user_rip, user_rsp) = unsafe {
        let m = &(*machines())[slot];
        (
            m.uctx.saved_regs,
            m.uctx.fs_base,
            m.uctx.gs_base,
            m.uctx.forked,
            m.uctx.user_rip,
            m.uctx.user_rsp,
        )
    };
    t.check("uctx: the register snapshot is the parent's", regs == ctx.regs);
    t.check_eq("uctx: fs_base (the TLS musl fixes up)", fs_base, ctx.fs_base);
    t.check_eq("uctx: gs_base", gs_base, ctx.gs_base);
    t.check_eq("uctx: the forked flag says to use them", forked, 1);
    t.check(
        "uctx: pc/sp stay with the process image, not the slot",
        user_rip == 0 && user_rsp == 0,
    );

    abandon_unpublished(slot);
}
