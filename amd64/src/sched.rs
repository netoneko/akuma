//! Context switching and a round-robin scheduler — across every core.
//!
//! Stage E. Stage D produced a timer tick and nothing that used it; this is what
//! uses it. Stage U made it run on more than one core.
//!
//! # What this is, and what it is not
//!
//! This is a **real context switch**: separate stacks, callee-saved registers
//! preserved across the switch, locals surviving a round trip through another
//! task. It is scheduled round-robin and paced by the LAPIC tick, and since
//! Stage U any core may pick any unpinned task: a task suspended on one core
//! resumes on whichever core's `yield_now` finds it first.
//!
//! **Ring 3 and the idle loop are preempted; kernel tasks are not.** The tick
//! sets a per-core flag ([`need_resched`]) which the next `yield_now` on that
//! core consumes; when the tick lands in user code or in a core's idle loop the
//! handler yields on the interrupted task's behalf, inside the interrupt, so
//! `iretq` returns onto a different task's stack. A kernel task — the boot
//! task, the netpoll daemon — switches only where it calls `yield_now` itself.
//! It used to be preemptible too, and that was a latent deadlock rather than a
//! feature: a kernel task preempted inside the heap allocator leaves the heap's
//! spinlock held, and the next task to allocate — a syscall, with interrupts
//! off — spins on it forever. Every kernel task here yields on every lap, so
//! nothing was lost by making the rule explicit.
//!
//! # The Big Kernel Lock, and what a switch does with it
//!
//! Every task in this table is in kernel code when it is not running — that is
//! what "not running" means — and kernel code holds the BKL (`smp.rs`). So a
//! switch never releases the lock: the core keeps it, saves the outgoing task's
//! hold depth into its `Task`, and installs the incoming task's. The incoming
//! task then leaves the kernel however it was going to (a `sysret`, the idle
//! loop's `hlt`) and that is where the lock is actually let go. The one place
//! this module drops the lock itself is when there is nothing to switch *to*:
//! a task spinning in `yield_now` waiting for another core to do something
//! (write a pipe, exit) opens a `bkl_drop_window` so that core can.
//!
//! # Relationship to proposal item 4
//!
//! `akuma-exec-core`'s `Context` is a `#[repr(C)]` struct of named AArch64
//! registers (`x19`..`x30`, `spsr`, `elr`, `ttbr0`) with **public mutable
//! fields**, which item 4 wants replaced by constructors and accessors. The
//! [`Context`] here is what that argument looks like taken seriously: it holds
//! **one** field, `rsp`, and is built only by [`Context::for_task`].
//!
//! Everything else lives on the task's own stack, pushed by the switch routine.
//! That is not an x86 trick — the same structure works on AArch64 — and it is
//! why item 4's "make the register block private" is the right instinct: once
//! the only way to build a context is a constructor, the register set stops
//! being part of the interface at all, and the crate that owns fork and exec
//! stops needing to know what a callee-saved register is.

use akuma_selftest::Suite;

use crate::lapic;
use crate::paging;
use crate::smp::{self, NO_CPU};
use crate::usermode::UserCtx;
use alloc::vec;
use core::sync::atomic::{AtomicU64, Ordering};

/// Per-task kernel stack. Generous: these are `Vec` allocations from a 64 MiB
/// heap, and a stack overflow here has no guard page to catch it.
const STACK_SIZE: usize = 32 * 1024;

/// Maximum tasks, including the boot task in slot 0.
///
/// Slots are never recycled — `spawn` looks for `State::Unused`, and a finished
/// task stays `Finished` so its stack is not handed to someone else. The table
/// therefore has to hold every task the *whole boot* ever creates, not every
/// task alive at once: three scheduler workers, two cooperative processes, two
/// preempted ones, and one loaded from an ELF image. Eight of those plus the
/// boot task is exactly nine, which is why this stopped being 8 when the ELF
/// stage landed — the symptom was `spawn` returning `None` after every earlier
/// test had passed.
///
/// Stage Q added the netpoll daemon (one slot, whole run); Stage R added
/// `sys_spawn`, and each `sshd` session that runs a command takes another slot.
/// Stage T added `fork` — an interactive shell takes a **fresh** slot for every
/// external command it runs, on top of the one for the shell itself.
/// `sys_waitpid` recycles the child's `PROCS` slot and its frames, but **not**
/// its scheduler task slot — so this is the cap on how many commands one `sshd`
/// boot can serve. Stage U added one idle task per secondary core and four SMP
/// self-test workers.
///
/// The right fix is recycling, and it is deliberately not this: a slot cannot
/// be reused until its two 32 KiB stacks can be, and reclaiming those needs the
/// scheduler to know a task's stack is no longer in use by any frame — which is
/// a different stage. Growing the table is honest about being a bound on the
/// boot's total task count; 96 slots × (2 × 32 KiB stacks, leaked lazily on
/// first use) is ~6 MiB against the 64 MiB heap, and buys an interactive shell
/// dozens of commands before the ceiling bites.
const MAX_TASKS: usize = 96;

core::arch::global_asm!(
    r#"
    /* `.section .text` is load-bearing, not boilerplate.
     *
     * Module-level `global_asm!` blocks from every module are concatenated into
     * one object file, and the assembler carries its "current section" across
     * that boundary. `boot.s` ends with `.section .bss` (the boot stack), so
     * without this directive these instructions are emitted into .bss — which is
     * NOLOAD — and the link fails with:
     *
     *   error: BSS section '.bss' cannot have non-zero bytes
     *
     * The error names the section but not the cause, and the cause is in a
     * different file that looks unrelated. Every asm block in this crate should
     * open by naming its section. */
    .section .text
.global switch_context
switch_context:
    /* System V: rdi = &mut old.rsp, rsi = &new.rsp.
     *
     * The entire register state lives on the outgoing stack rather than in the
     * Context struct — push rflags and the six callee-saved registers, swap the
     * stack pointer, pop them back on the incoming stack. The return address is
     * already on the stack from the `call` that got here, so `ret` resumes the
     * incoming task exactly where its own switch_context left off.
     *
     * rflags is saved and restored so that `IF` belongs to the task, not to
     * whoever last ran on this core: a syscall (interrupts off) that yields to
     * a kernel task (interrupts on) must come back with them off, and it did
     * not until Stage U — the resumer's state leaked into the resumed.
     *
     * Caller-saved registers need no handling: this is a normal C call, so the
     * compiler has already spilled anything it cares about. */
    pushfq
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, [rsi]
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    popfq
    ret
"#
);

unsafe extern "C" {
    /// Save the current register state, switch stacks, restore the other.
    ///
    /// # Safety
    /// `new` must describe a stack built by [`Context::for_task`] or saved by a
    /// previous call to this function, and that stack must still be live.
    fn switch_context(old: *mut Context, new: *const Context);
}

/// A saved task, which on x86_64 is exactly one stack pointer.
///
/// See the module header: the register block is not part of this type, so there
/// is no way to hand a task a malformed one. Compare `akuma-exec-core`'s
/// `Context`, whose 20 public mutable fields are what proposal item 4 is about.
#[repr(C)]
pub struct Context {
    rsp: u64,
}

/// The `rflags` a fresh task starts with: `IF` set, bit 1 (always 1). A kernel
/// task runs with interrupts on so its core keeps ticking; a task that enters
/// ring 3 hands the CPU `sysret`'s own `r11` anyway.
const INITIAL_RFLAGS: u64 = 0x202;

impl Context {
    /// An empty context, filled in by the first switch *away* from this task.
    const fn empty() -> Self {
        Self { rsp: 0 }
    }

    /// Build a stack that `switch_context` can resume into `entry`.
    ///
    /// Lays out the frame that routine expects to pop: six callee-saved
    /// registers, then `rflags`, then a return address. `ret` then jumps to
    /// `entry`.
    ///
    /// The 16-byte gap below `entry` is ABI, not padding. System V requires
    /// `rsp + 8` to be 16-byte aligned at function entry — the state after a
    /// `call` has pushed its 8-byte return address. Placing `entry` at a
    /// 16-aligned address leaves `rsp ≡ 8 (mod 16)` once `ret` pops it, which is
    /// what the callee expects. Get this wrong and everything works until the
    /// first SSE spill.
    fn for_task(stack_top: usize, entry: extern "C" fn() -> !) -> Self {
        let entry_slot = (stack_top - 16) & !0xf;
        // SAFETY: `entry_slot` is inside the freshly allocated stack, and the
        // eight words written below it are too.
        unsafe {
            let p = entry_slot as *mut u64;
            p.write(entry as usize as u64);
            p.sub(1).write(INITIAL_RFLAGS);
            for i in 2..=7 {
                p.sub(i).write(0);
            }
        }
        Self {
            rsp: (entry_slot - 56) as u64,
        }
    }
}

/// One task's x87/SSE register file, in `fxsave` layout.
///
/// The kernel is soft-float and never touches these registers, so a task's
/// SSE state used to survive a preemption by accident: whatever was in the
/// xmm registers when the tick landed was still there when the task resumed —
/// *on the same core*. A task that resumes on another core finds that core's
/// registers instead, and a task interleaved with a second SSE-using process
/// on one core never had that luck. Saved and restored around every switch,
/// 512 bytes per task, 16-byte aligned as the instruction requires.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct FxArea([u8; 512]);

impl FxArea {
    /// The reset state a program expects: x87 control word `0x37F`, `MXCSR`
    /// `0x1F80` (every SSE exception masked). Not all-zero: an all-zero
    /// `MXCSR` unmasks every exception, and the first inexact result in ring 3
    /// would raise `#XM` — an "unhandled vector" halt, from a program that did
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

#[derive(Copy, Clone, PartialEq, Eq)]
enum State {
    Unused,
    /// Allocated but not yet schedulable: the caller is still filling it in
    /// (`spawn_in_space_unpublished` → `seed_forked_task` → `publish_task`).
    /// Invisible to the picker, and not free to `spawn`'s scan either.
    Reserved,
    Runnable,
    Finished,
}

struct Task {
    ctx: Context,
    state: State,
    /// A daemon runs for the life of the kernel and never finishes — the netpoll
    /// loop, and every core's idle task. [`all_user_tasks_finished`] ignores
    /// these slots, so `run_init`'s drive loop still ends when the *shell* exits
    /// rather than spinning against a task that is Runnable on purpose.
    daemon: bool,
    /// The task that runs a core when nothing else will. Never chosen by the
    /// round-robin scan; switched to explicitly, by its own core, as the
    /// fallback when the running task cannot continue.
    idle: bool,
    /// Executing on some core right now. A Runnable task that is `on_cpu` is
    /// not a candidate: two cores must never resume one stack.
    on_cpu: bool,
    /// The only core allowed to run this task, or [`NO_CPU`]. The boot task is
    /// pinned to the BSP — it drives the self-tests against that core's LAPIC —
    /// and every idle task to its own core.
    pinned: u32,
    /// The Big Kernel Lock hold depth this task was suspended at; reinstalled
    /// on the core that resumes it. See the module header.
    bkl_depth: u32,
    /// Page-table root to install when this task runs. `0` means "the kernel's",
    /// which is what every kernel task uses.
    space_root: u64,
    /// Where the syscall path saves this task's stacks. Per-task since Stage I;
    /// see `usermode::UserCtx`.
    uctx: UserCtx,
    /// Stack the CPU switches to when this task traps from ring 3. Per-task
    /// since Stage J; see `gdt::set_kernel_stack`.
    trap_stack_top: u64,
    /// This task's SSE/x87 registers while it is not running.
    fx: FxArea,
}

impl Task {
    const fn empty() -> Self {
        Self {
            fx: FxArea::initial(),
            ctx: Context::empty(),
            state: State::Unused,
            daemon: false,
            idle: false,
            on_cpu: false,
            pinned: NO_CPU,
            bkl_depth: 1,
            space_root: 0,
            uctx: UserCtx::new(),
            trap_stack_top: 0,
        }
    }
}

/// The kernel's own page-table root, captured at [`init`].
///
/// A task with `space_root == 0` runs in this. Recorded rather than re-read from
/// `CR3` at switch time, because by then `CR3` holds whatever the *outgoing*
/// task was using.
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);

/// The kernel's page-table root. What every core other than the one that built
/// it needs in order to leave its boot tables.
#[must_use]
pub fn kernel_root() -> u64 {
    KERNEL_ROOT.load(Ordering::Relaxed)
}

/// The task table. Slot 0 is the boot thread.
///
/// `static mut`, reached only through raw pointers, **under the BKL**. Every
/// writer is kernel code, kernel code holds the lock, and a switch keeps it
/// held — so the table sees one core at a time, which is the property the old
/// "single core" comments were actually relying on.
static mut TASKS: [Task; MAX_TASKS] = [const { Task::empty() }; MAX_TASKS];

/// Raw pointer to the task table.
///
/// The `&raw mut` lives behind a function for one reason: writing
/// `(*(&raw mut TASKS))[i]` inline trips `clippy::deref_addrof`, whose suggested
/// fix — index `TASKS` directly — reintroduces the `static_mut_refs` violation
/// the raw pointer exists to avoid. Naming the pointer once resolves both, and
/// puts the `static mut` behind a single door.
fn tasks() -> *mut [Task; MAX_TASKS] {
    &raw mut TASKS
}

fn current() -> usize {
    smp::current_task()
}

/// Called from the timer interrupt: switch tasks if the tick asked for it and
/// the interrupted code may be switched away from.
///
/// Runs **inside an interrupt handler**, which is what makes this preemption
/// rather than the cooperative yield of Stage I. The suspended task is left
/// sitting on its own trap stack with its interrupt frame intact; when it is
/// scheduled again it returns from here, the handler returns, and `iretq`
/// resumes whatever it was doing.
///
/// Only ring 3 (`from_user`) and the idle loop are switched away from here. A
/// kernel task keeps running and consumes the flag at its next `yield_now` —
/// see the module header for the deadlock that ring-0 preemption was.
///
/// Safe only because every task has its **own** trap stack
/// (`gdt::set_kernel_stack`), because interrupts are masked for the duration
/// (the IDT uses interrupt gates, not trap gates), so this cannot nest, and
/// because the handler took the BKL on the way in for the two cases it acts on.
pub fn preempt_if_needed(from_user: bool) {
    if !smp::need_resched() {
        return;
    }
    // SAFETY: raw-pointer read of the table; the BKL is not needed to read a
    // slot this core itself is running.
    let idle = unsafe { (*tasks())[current()].idle };
    if from_user || idle {
        // Ring 3 does not hold the BKL and the idle loop dropped it for `hlt`;
        // the switch needs it. Recursive on the BSP's own kernel tasks, harmless
        // there too.
        smp::bkl_enter();
        PREEMPTIONS.fetch_add(1, Ordering::Relaxed);
        yield_now();
        smp::bkl_leave();
    }
}

/// Switches performed from the timer interrupt rather than from a `yield_now`.
static PREEMPTIONS: AtomicU64 = AtomicU64::new(0);

/// How many times the timer has taken a task off the CPU.
#[must_use]
pub fn preemptions() -> u64 {
    PREEMPTIONS.load(Ordering::Relaxed)
}

/// Yields that found the tick's reschedule request set — the kernel-task
/// counterpart of a preemption: the timer asked, the task obliged.
static TICK_YIELDS: AtomicU64 = AtomicU64::new(0);

/// Have all tasks except the boot task and any daemons finished?
///
/// Daemon slots (the netpoll loop, the idle tasks) are Runnable for the whole
/// run by design, so they are skipped — otherwise `run_init`'s drive loop would
/// never see the shell exit.
#[must_use]
pub fn all_user_tasks_finished() -> bool {
    // SAFETY: raw-pointer read of the table; under the BKL.
    unsafe {
        (1..MAX_TASKS).all(|s| {
            let t = &(*tasks())[s];
            t.daemon || t.state != State::Runnable
        })
    }
}

/// The running task's slot on this core.
#[must_use]
pub fn current_task() -> usize {
    current()
}

/// A raw pointer to slot `slot`'s `UserCtx`, for `smp` to seed a core's
/// `current_uctx` with before that core runs.
#[must_use]
pub fn uctx_ptr(slot: usize) -> *mut UserCtx {
    // SAFETY: the table is a `static`; the pointer is into it.
    unsafe { &raw mut (*tasks())[slot].uctx }
}

/// Called from the LAPIC timer handler, on the core whose timer fired.
pub fn set_need_resched() {
    smp::set_need_resched();
}

/// Register the currently-executing thread as task 0, pinned to the BSP.
pub fn init() {
    KERNEL_ROOT.store(paging::active_root(), Ordering::Relaxed);
    // SAFETY: raw-pointer access to the table; under the BKL, before any switch.
    unsafe {
        let t0 = &mut (*tasks())[0];
        t0.state = State::Runnable;
        t0.on_cpu = true;
        t0.pinned = 0;
        t0.bkl_depth = smp::bkl_depth();
        // The boot task needs a live user context too: it is what the very first
        // `enter_user_mode` publishes its kernel stack into.
        smp::set_current_uctx(&raw mut t0.uctx);
    }
    smp::set_current_task(0);
}

/// Allocate the idle task for core `cpu` — the context that core is already
/// executing when it arrives in `ap_entry64`, so the slot gets an empty
/// [`Context`] that the first switch away from it fills in.
///
/// Pinned to its core, a daemon (so it does not hold the boot up), `on_cpu`
/// from the start (its core is about to be running it), and `idle` so the
/// picker leaves it alone. Called by the BSP, under the BKL, before the core is
/// started.
pub fn register_idle_task(cpu: usize) -> Option<usize> {
    // SAFETY: raw-pointer access to the table; under the BKL.
    unsafe {
        let t = tasks();
        for slot in 1..MAX_TASKS {
            if (*t)[slot].state == State::Unused {
                let task = &mut (*t)[slot];
                task.ctx = Context::empty();
                task.daemon = true;
                task.idle = true;
                task.on_cpu = true;
                task.pinned = cpu as u32;
                task.bkl_depth = 1;
                task.space_root = 0;
                task.trap_stack_top = 0;
                task.state = State::Runnable;
                return Some(slot);
            }
        }
    }
    None
}

/// What a secondary core runs forever: hand the core to any runnable task, and
/// sleep until the next tick when there is none.
///
/// Entered holding the BKL at depth 1. The lock is released across `hlt` —
/// that is the whole point of the idle loop under a BKL: a core that has
/// nothing to do must not hold the one lock everyone else needs — and taken
/// back before the loop looks at the task table again. `sti; hlt` is atomic
/// with respect to the tick: `sti` takes effect after the following
/// instruction, so a tick cannot slip between them and leave the core asleep
/// past it.
pub fn idle_loop() -> ! {
    loop {
        if !try_switch() {
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

/// Create a task that runs in a page table of its own.
///
/// `space_root` is installed in `CR3` whenever this task is scheduled. The
/// address space must share the kernel's mappings — see
/// `paging::AddressSpace` — or the switch faults on the instruction after
/// `mov cr3`.
///
/// The slot is published [`State::Runnable`] only after `space_root` is in
/// place: this used to be `spawn()` followed by a separate write, and a LAPIC
/// tick landing between the two scheduled the task with `space_root` still 0 —
/// kernel CR3 — so `run_init`'s sysret into ring 3 fetched sshd's entry point
/// unmapped (`#PF`, `cr2 == rip == 0x2045d0`, `err=0x14`). Measured 2026-09-04,
/// intermittent by timer phase: one boot served ssh, the next died on the first
/// user instruction. With a second core the same gap is not a tick away but
/// zero instructions away, which is why [`State::Reserved`] exists.
pub fn spawn_in_space(entry: extern "C" fn() -> !, space_root: u64) -> Option<usize> {
    spawn_ready(entry, space_root, false)
}

/// Like [`spawn_in_space`], but the slot is left unpublished
/// ([`State::Reserved`]) so the caller can finish initialising it —
/// [`seed_forked_task`]'s register and TLS snapshot — before anything can
/// schedule it. Finish with [`publish_task`].
pub fn spawn_in_space_unpublished(entry: extern "C" fn() -> !, space_root: u64) -> Option<usize> {
    spawn_unpublished(entry, space_root, false)
}

/// Make an [`spawn_in_space_unpublished`] slot schedulable.
pub fn publish_task(task_slot: usize) {
    // SAFETY: raw-pointer access to the table; under the BKL.
    unsafe {
        if let Some(task) = (*tasks()).get_mut(task_slot) {
            task.state = State::Runnable;
        }
    }
}

/// Repoint the **running** task's address space, and install it in `CR3` now.
///
/// `execve` (Stage T) rebuilds a spawned child's process in a fresh address
/// space without a `fork`; the task keeps running but must switch page tables.
/// The switch is done here rather than left to the next `yield_now` because the
/// caller (`usermode::run_process`) re-enters ring 3 at a VA that only the new
/// space maps — a stale `CR3` would `#PF` on the first user instruction.
///
/// Safe to switch mid-flight: every space shares the kernel's upper-half
/// mappings, so the kernel stack this runs on stays mapped across `mov cr3`.
pub fn set_current_space_root(space_root: u64) {
    // SAFETY: raw-pointer access to the table; under the BKL.
    unsafe {
        (*tasks())[current()].space_root = space_root;
    }
    let want = if space_root == 0 {
        KERNEL_ROOT.load(Ordering::Relaxed)
    } else {
        space_root
    };
    if want != paging::active_root() {
        // SAFETY: `want` is either a live `AddressSpace` root (from `execve`'s
        // freshly-loaded image) or the kernel's own; both share the upper-half
        // mappings, so the kernel stack and code stay mapped across the write.
        unsafe {
            paging::activate(want);
        }
    }
}

/// Seed a not-yet-running `vfork` child task with the parent's TLS base and its
/// full register snapshot, so it resumes as a true copy of the parent's context
/// (see `usermode::enter_user_mode_forked`). The child inherits `%fs` because
/// musl's post-fork fixups are `%fs`-relative, and the register set because a C
/// compiler assumes r12-r15/rbx survive the `syscall`.
pub fn seed_forked_task(task_slot: usize, fs_base: u64, gs_base: u64, saved_regs: &[u64; 12]) {
    // SAFETY: raw-pointer access; under the BKL, and the task is Reserved so
    // no core can be running it.
    unsafe {
        if let Some(task) = (*tasks()).get_mut(task_slot) {
            task.uctx.fs_base = fs_base;
            task.uctx.gs_base = gs_base;
            task.uctx.saved_regs = *saved_regs;
        }
    }
}

/// Create a daemon task: one that runs for the life of the kernel and is not
/// counted by [`all_user_tasks_finished`]. The netpoll loop is the only caller.
///
/// The `daemon` flag is written before publication, not after: the flag's whole
/// job is to keep this task out of [`all_user_tasks_finished`]'s tally, and a
/// tick landing between `spawn()` and the flag write would publish a not-yet-
/// daemon task — the boot drive loop would then wait for a `Finished` that a
/// never-exiting daemon never reaches.
pub fn spawn_daemon(entry: extern "C" fn() -> !) -> Option<usize> {
    spawn_ready(entry, 0, true)
}

/// [`spawn`] with the space root and daemon flag set before the slot becomes
/// schedulable. Every field the scheduler or the picker reads is in place
/// before `state = Runnable` publishes the slot — that ordering is the whole
/// point; see [`spawn_in_space`].
fn spawn_ready(entry: extern "C" fn() -> !, space_root: u64, daemon: bool) -> Option<usize> {
    let slot = spawn_unpublished(entry, space_root, daemon)?;
    publish_task(slot);
    Some(slot)
}

/// Create a task in the kernel's own address space (`space_root` 0).
pub fn spawn(entry: extern "C" fn() -> !) -> Option<usize> {
    spawn_ready(entry, 0, false)
}

/// Allocate and initialise a task slot without making it schedulable.
fn spawn_unpublished(
    entry: extern "C" fn() -> !,
    space_root: u64,
    daemon: bool,
) -> Option<usize> {
    // Leaked deliberately: a task's stack must outlive the frame that made it,
    // and nothing here ever reaps a task.
    let stack = vec![0u8; STACK_SIZE].leak();
    let stack_top = stack.as_ptr() as usize + STACK_SIZE;

    // A second stack, for traps taken while this task is in ring 3. Separate
    // from the task's own kernel stack because a preempted task is suspended on
    // the interrupt frame, and the two must not overlap.
    let trap = vec![0u8; STACK_SIZE].leak();
    let trap_top = (trap.as_ptr() as usize + STACK_SIZE) & !0xf;

    // SAFETY: raw-pointer access to the table; under the BKL.
    unsafe {
        let t = tasks();
        for slot in 1..MAX_TASKS {
            if (*t)[slot].state == State::Unused {
                let task = &mut (*t)[slot];
                task.ctx = Context::for_task(stack_top, entry);
                task.trap_stack_top = trap_top as u64;
                task.space_root = space_root;
                task.daemon = daemon;
                task.idle = false;
                task.on_cpu = false;
                task.pinned = NO_CPU;
                // A fresh task begins in kernel code, and kernel code holds the
                // lock: it is born at depth 1, as if it had just entered.
                task.bkl_depth = 1;
                task.uctx = UserCtx::new();
                task.fx = FxArea::initial();
                task.state = State::Reserved;
                return Some(slot);
            }
        }
    }
    None
}

/// Mark the running task finished and switch away for good.
///
/// The task's address space is somebody else's to free from here on — the
/// parent's `waitpid`, a self-test's teardown — and they may do so the moment
/// `yield_now` drops the BKL, which is before this core has switched. So this
/// core leaves the space *first*: with `CR3` on the kernel root, a freed and
/// reused PML4 frame is nothing this core will ever walk again. Measured
/// 2026-09-05 (`SMP=4`): without this, a reaped root frame came back as the
/// next spawn's root, the same `CR3` value skipped the flush, and the new
/// process ran on the dead one's TLB entries — `hello` read a garbage `argc`,
/// busybox `#GP`'d walking `argv`.
pub fn finish() -> ! {
    // SAFETY: raw-pointer access; under the BKL.
    unsafe {
        (*tasks())[current()].state = State::Finished;
        (*tasks())[current()].space_root = 0;
        // SAFETY: the kernel root maps this stack and everything below.
        paging::activate(KERNEL_ROOT.load(Ordering::Relaxed));
    }
    loop {
        yield_now();
    }
}

/// Switch to the next runnable task, round-robin.
///
/// A no-op when nothing else is runnable — deliberately, so a lone task calling
/// this in a loop makes progress rather than deadlocking against itself. Every
/// call, switch or not, first opens a `bkl_drop_window`: see [`try_switch`].
pub fn yield_now() {
    try_switch();
}

/// Pick the next runnable task for this core and switch to it. Returns whether
/// a switch happened.
///
/// **Every yield lets the other cores in first.** The lock is dropped and
/// retaken before the table is even looked at — and the ticket lock is FIFO, so
/// a core already spinning for it gets it now, not after this core's next
/// idea. The first version dropped it only when there was nothing to switch
/// to, and that was a livelock, measured 2026-09-05 with `SMP=4 STRACE=1`: a
/// shell and the boot task took turns on the BSP, each yield finding the other
/// runnable, while the shell's forked child sat on core 2 spinning in
/// `syscall_handler`'s `bkl_enter` for the `execve` it never got to make. Two
/// tasks on one core is enough to keep a lock that is only released "when
/// idle" held forever.
///
/// Candidates are Runnable, not on any core, not an idle task, and not pinned
/// to another core. When there is none and the running task is itself
/// Runnable, it keeps the core. When there is none and it is not — it finished,
/// or it is blocked on something only another task will do — this core's idle
/// task takes over; the boot task is the BSP's.
fn try_switch() -> bool {
    smp::bkl_drop_window();
    let cpu = smp::cpu_index() as u32;
    if smp::take_need_resched() {
        TICK_YIELDS.fetch_add(1, Ordering::Relaxed);
    }

    // SAFETY: raw-pointer access to the table under the BKL, and the contexts
    // handed to `switch_context` are either built by `Context::for_task` or
    // saved by a previous switch. `on_cpu` is what keeps two cores off one
    // stack: it is cleared for the outgoing task before the switch, but no
    // other core can observe that until this core releases the BKL, which the
    // incoming task does only after the switch has completed.
    unsafe {
        let t = tasks();
        let cur = current();

        let mut next = None;
        for step in 1..=MAX_TASKS {
            let cand = (cur + step) % MAX_TASKS;
            let c = &(*t)[cand];
            if c.state == State::Runnable
                && !c.on_cpu
                && !c.idle
                && (c.pinned == NO_CPU || c.pinned == cpu)
            {
                next = Some(cand);
                break;
            }
        }

        let next = match next {
            Some(n) if n != cur => n,
            // Nothing to switch to and this task can carry on.
            _ if (*t)[cur].state == State::Runnable => return false,
            _ => {
                // This task cannot continue and there is nothing else: idle.
                let idle = smp::idle_task();
                if idle == cur {
                    return false;
                }
                idle
            }
        };

        // Hand the lock's depth from the outgoing task to the incoming one.
        // The lock itself stays with this core.
        (*t)[cur].bkl_depth = smp::bkl_depth();
        (*t)[cur].on_cpu = false;
        (*t)[next].on_cpu = true;
        smp::set_bkl_depth((*t)[next].bkl_depth);
        smp::set_current_task(next);

        // Repoint the per-task syscall context *before* the switch: the
        // incoming task may resume inside its own syscall and immediately read
        // it on the way back to ring 3.
        smp::set_current_uctx(&raw mut (*t)[next].uctx);

        // Install the incoming task's address space. Every space shares the
        // kernel's mappings, so the instructions between here and the switch —
        // and the switch itself — remain mapped throughout. An idle task's is
        // the kernel's: a core parked in `hlt` must never hold a process's root
        // in `CR3`, because that process may be torn down from another core
        // while it sleeps.
        // Written unconditionally for a process root, even when `CR3` already
        // holds that value: the write is what flushes the TLB, and "same value"
        // does not mean "same address space" — a freed root frame can be the
        // next process's root (see `finish`). Only kernel-root to kernel-root
        // skips it.
        let want = match (*t)[next].space_root {
            0 => KERNEL_ROOT.load(Ordering::Relaxed),
            root => root,
        };
        if (*t)[next].space_root != 0 || want != paging::active_root() {
            paging::activate(want);
        }

        // Where a ring-3 trap by the incoming task will land, on this core.
        crate::gdt::set_kernel_stack((*t)[next].trap_stack_top);

        // Restore the incoming task's `%fs` base. `IA32_FS_BASE` is one
        // per-core register and `arch_prctl` is the only writer, so a task
        // that set it keeps its value only as long as nothing else runs. A
        // shell and the child it forked each have their own TLS — without this,
        // the child's `execve` (which re-`arch_prctl`s) leaves the parent on
        // the child's base. `0` means "never set"; leave the MSR alone then.
        let fs = (*t)[next].uctx.fs_base;
        if fs != 0 {
            crate::usermode::set_fs_base(fs);
        }
        // And its user `%gs` base — unconditionally, because the kernel's own
        // `%gs` is the other half of the pair and a stale user value here would
        // follow a task that never set one onto another core.
        crate::usermode::set_user_gs_base((*t)[next].uctx.gs_base);

        // The SSE/x87 registers belong to the task, not the core (see `FxArea`).
        // Saved here rather than at interrupt entry because nothing between the
        // two touches them: the kernel is soft-float.
        let fx_out = &raw mut (*t)[cur].fx;
        let fx_in = &raw const (*t)[next].fx;
        core::arch::asm!(
            "fxsave64 [{out}]",
            "fxrstor64 [{inp}]",
            out = in(reg) fx_out,
            inp = in(reg) fx_in,
            options(nostack, preserves_flags)
        );

        let old = &raw mut (*t)[cur].ctx;
        let new = &raw const (*t)[next].ctx;
        switch_context(old, new);
    }
    true
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

/// Spawn three tasks, run them to completion, verify.
pub fn smoke_test(t: &mut Suite) {
    init();
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
        // SAFETY: raw-pointer read of the table; under the BKL.
        let done = unsafe { (1..=WORKERS).all(|s| (*tasks())[s].state == State::Finished) };
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
