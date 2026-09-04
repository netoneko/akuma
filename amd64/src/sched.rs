//! Context switching and a round-robin scheduler.
//!
//! Stage E. Stage D produced a timer tick and nothing that used it; this is what
//! uses it.
//!
//! # What this is, and what it is not
//!
//! This is a **real context switch**: separate stacks, callee-saved registers
//! preserved across the switch, locals surviving a round trip through another
//! task. It is scheduled round-robin and paced by the LAPIC tick.
//!
//! It is **not preemption**. A task that never calls [`yield_now`] runs forever;
//! the tick sets a flag ([`need_resched`]) which `yield_now` consumes, so the
//! switch happens at a point the task chooses. True preemption means switching *inside* the
//! interrupt handler, so `iretq` returns onto a different task's stack — which
//! needs each task's interrupt frame to live on its own stack, and needs a TSS
//! once ring 3 exists. The honest description of what is here is
//! "timer-driven cooperative scheduling", and calling it anything else would
//! misrepresent what the test below proves.
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
use crate::usermode::{CURRENT_UCTX, UserCtx};
use alloc::vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Per-task kernel stack. Generous: these are `Vec` allocations from a 16 MiB
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
/// `sys_waitpid` recycles the child's `PROCS` slot and its frames, but **not**
/// its scheduler task slot — so this is now the cap on how many commands one
/// `sshd` boot can serve (~13 after the self-tests). Recycling task slots is
/// the fix and is deliberately still not here.
///
/// The right fix is recycling, and it is deliberately not this: a slot cannot
/// be reused until its two 32 KiB stacks can be, and reclaiming those needs the
/// scheduler to know a task's stack is no longer in use by any frame — which is
/// a different stage. Growing the table is honest about being a bound on the
/// boot's total task count.
const MAX_TASKS: usize = 24;

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
     * Context struct — push the six callee-saved registers, swap the stack
     * pointer, pop them back on the incoming stack. The return address is
     * already on the stack from the `call` that got here, so `ret` resumes the
     * incoming task exactly where its own switch_context left off.
     *
     * Caller-saved registers need no handling: this is a normal C call, so the
     * compiler has already spilled anything it cares about. */
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

impl Context {
    /// An empty context, filled in by the first switch *away* from this task.
    const fn empty() -> Self {
        Self { rsp: 0 }
    }

    /// Build a stack that `switch_context` can resume into `entry`.
    ///
    /// Lays out the frame that routine expects to pop: six callee-saved
    /// registers, then a return address. `ret` then jumps to `entry`.
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
        // seven words written below it are too.
        unsafe {
            let p = entry_slot as *mut u64;
            p.write(entry as usize as u64);
            for i in 1..=6 {
                p.sub(i).write(0);
            }
        }
        Self {
            rsp: (entry_slot - 48) as u64,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum State {
    Unused,
    Runnable,
    Finished,
}

struct Task {
    ctx: Context,
    state: State,
    /// A daemon runs for the life of the kernel and never finishes — the netpoll
    /// loop is the only one so far. [`all_user_tasks_finished`] ignores these
    /// slots, so `run_init`'s drive loop still ends when the *shell* exits rather
    /// than spinning against a task that is Runnable on purpose.
    daemon: bool,
    /// Page-table root to install when this task runs. `0` means "the kernel's",
    /// which is what every kernel task uses.
    space_root: u64,
    /// Where the syscall path saves this task's stacks. Per-task since Stage I;
    /// see `usermode::UserCtx`.
    uctx: UserCtx,
    /// Stack the CPU switches to when this task traps from ring 3. Per-task
    /// since Stage J; see `gdt::set_kernel_stack`.
    trap_stack_top: u64,
}

impl Task {
    const fn empty() -> Self {
        Self {
            ctx: Context::empty(),
            state: State::Unused,
            daemon: false,
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

/// The task table. Slot 0 is the boot thread.
///
/// `static mut`, reached only through raw pointers, on one core. A real
/// scheduler needs a lock here; this one cannot race because the only writer
/// runs with the switch and there is no second core.
static mut TASKS: [Task; MAX_TASKS] = [const { Task::empty() }; MAX_TASKS];
static mut CURRENT: usize = 0;

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
    // SAFETY: single core; `CURRENT` is only written by `yield_now` and `init`.
    unsafe { *(&raw const CURRENT).cast::<usize>() }
}

/// Called from the timer interrupt: switch tasks if the tick asked for it.
///
/// Runs **inside an interrupt handler**, which is what makes this preemption
/// rather than the cooperative yield of Stage I. The suspended task is left
/// sitting on its own trap stack with its interrupt frame intact; when it is
/// scheduled again it returns from here, the handler returns, and `iretq`
/// resumes whatever it was doing — in ring 3 or ring 0.
///
/// Safe only because every task has its **own** trap stack
/// (`gdt::set_kernel_stack`) and because interrupts are masked for the duration
/// (the IDT uses interrupt gates, not trap gates), so this cannot nest.
pub fn preempt_if_needed() {
    if need_resched() {
        PREEMPTIONS.fetch_add(1, Ordering::Relaxed);
        yield_now();
    }
}

/// Switches performed from the timer interrupt rather than from a `yield_now`.
static PREEMPTIONS: AtomicU64 = AtomicU64::new(0);

/// How many times the timer has taken a task off the CPU.
#[must_use]
pub fn preemptions() -> u64 {
    PREEMPTIONS.load(Ordering::Relaxed)
}

/// Have all tasks except the boot task and any daemons finished?
///
/// Daemon slots (the netpoll loop) are Runnable for the whole run by design, so
/// they are skipped — otherwise `run_init`'s drive loop would never see the
/// shell exit.
#[must_use]
pub fn all_user_tasks_finished() -> bool {
    // SAFETY: raw-pointer read of the table; single core.
    unsafe {
        (1..MAX_TASKS).all(|s| {
            let t = &(*tasks())[s];
            t.daemon || t.state != State::Runnable
        })
    }
}

/// The running task's slot.
#[must_use]
pub fn current_task() -> usize {
    current()
}

fn set_current(v: usize) {
    // SAFETY: as `current`.
    unsafe { *(&raw mut CURRENT).cast::<usize>() = v };
}

/// Set by the timer tick, cleared by whoever acts on it.
static NEED_RESCHED: AtomicBool = AtomicBool::new(false);

/// Called from the LAPIC timer handler.
pub fn set_need_resched() {
    NEED_RESCHED.store(true, Ordering::Relaxed);
}

/// Has the timer asked for a reschedule since it was last consumed?
pub fn need_resched() -> bool {
    NEED_RESCHED.load(Ordering::Relaxed)
}

/// Register the currently-executing thread as task 0.
pub fn init() {
    KERNEL_ROOT.store(paging::active_root(), Ordering::Relaxed);
    // SAFETY: raw-pointer access to the table; single core, before any switch.
    unsafe {
        (*tasks())[0].state = State::Runnable;
        // The boot task needs a live user context too: it is what the very first
        // `enter_user_mode` publishes its kernel stack into.
        let cur = &raw mut CURRENT_UCTX;
        *cur = &raw mut (*tasks())[0].uctx;
    }
    set_current(0);
}

/// Create a task that runs in a page table of its own.
///
/// `space_root` is installed in `CR3` whenever this task is scheduled. The
/// address space must share the kernel's mappings — see
/// `paging::AddressSpace` — or the switch faults on the instruction after
/// `mov cr3`.
pub fn spawn_in_space(entry: extern "C" fn() -> !, space_root: u64) -> Option<usize> {
    let slot = spawn(entry)?;
    // SAFETY: raw-pointer access to the table; single core, and `slot` was just
    // returned by `spawn`.
    unsafe {
        (*tasks())[slot].space_root = space_root;
    }
    Some(slot)
}

/// Create a daemon task: one that runs for the life of the kernel and is not
/// counted by [`all_user_tasks_finished`]. The netpoll loop is the only caller.
pub fn spawn_daemon(entry: extern "C" fn() -> !) -> Option<usize> {
    let slot = spawn(entry)?;
    // SAFETY: raw-pointer access to the table; single core, and `slot` was just
    // returned by `spawn`.
    unsafe {
        (*tasks())[slot].daemon = true;
    }
    Some(slot)
}

/// Create a task. Returns its slot, or `None` if the table is full.
pub fn spawn(entry: extern "C" fn() -> !) -> Option<usize> {
    // Leaked deliberately: a task's stack must outlive the frame that made it,
    // and nothing here ever reaps a task.
    let stack = vec![0u8; STACK_SIZE].leak();
    let stack_top = stack.as_ptr() as usize + STACK_SIZE;

    // A second stack, for traps taken while this task is in ring 3. Separate
    // from the task's own kernel stack because a preempted task is suspended on
    // the interrupt frame, and the two must not overlap.
    let trap = vec![0u8; STACK_SIZE].leak();
    let trap_top = (trap.as_ptr() as usize + STACK_SIZE) & !0xf;

    // SAFETY: raw-pointer access to the table; single core.
    unsafe {
        let t = tasks();
        for slot in 1..MAX_TASKS {
            if (*t)[slot].state == State::Unused {
                (*t)[slot].ctx = Context::for_task(stack_top, entry);
                (*t)[slot].trap_stack_top = trap_top as u64;
                (*t)[slot].state = State::Runnable;
                return Some(slot);
            }
        }
    }
    None
}

/// Mark the running task finished and switch away for good.
pub fn finish() -> ! {
    // SAFETY: raw-pointer access; single core.
    unsafe {
        (*tasks())[current()].state = State::Finished;
    }
    loop {
        yield_now();
    }
}

/// Switch to the next runnable task, round-robin.
///
/// A no-op when nothing else is runnable — deliberately, so a lone task calling
/// this in a loop makes progress rather than deadlocking against itself.
pub fn yield_now() {
    NEED_RESCHED.store(false, Ordering::Relaxed);

    // SAFETY: raw-pointer access to the table; single core, and the contexts
    // handed to `switch_context` are either built by `Context::for_task` or
    // saved by a previous switch.
    unsafe {
        let t = tasks();
        let cur = current();

        let mut next = None;
        for step in 1..=MAX_TASKS {
            let cand = (cur + step) % MAX_TASKS;
            if (*t)[cand].state == State::Runnable {
                next = Some(cand);
                break;
            }
        }

        let Some(next) = next else { return };
        if next == cur {
            return;
        }

        set_current(next);

        // Repoint the per-task syscall context *before* the switch: the
        // incoming task may resume inside its own syscall and immediately read
        // it on the way back to ring 3.
        let cur_uctx = &raw mut CURRENT_UCTX;
        *cur_uctx = &raw mut (*t)[next].uctx;

        // Install the incoming task's address space. Every space shares the
        // kernel's mappings, so the instructions between here and the switch —
        // and the switch itself — remain mapped throughout.
        let want = match (*t)[next].space_root {
            0 => KERNEL_ROOT.load(Ordering::Relaxed),
            root => root,
        };
        if want != paging::active_root() {
            paging::activate(want);
        }

        // Where a ring-3 trap by the incoming task will land.
        crate::gdt::set_kernel_stack((*t)[next].trap_stack_top);

        let old = &raw mut (*t)[cur].ctx;
        let new = &raw const (*t)[next].ctx;
        switch_context(old, new);
    }
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
/// preemption existed: `preempt_if_needed` consumes `NEED_RESCHED` inside the
/// interrupt handler, so a worker polling for it in ring 0 could never observe
/// it and spun out its whole budget. That the tick drives scheduling is now
/// measured by [`preemptions`] instead, which counts the thing directly rather
/// than inferring it from a flag two parties race for.
fn worker_body(id: usize) -> ! {
    let mut acc: u64 = 0;
    for round in 0..ROUNDS {
        acc = acc.wrapping_mul(31).wrapping_add(round + id as u64);

        // SAFETY: single core, one writer per index.
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

    let mut switches = 0u64;
    loop {
        // SAFETY: raw-pointer read of the table; single core.
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
    t.check("sched: timer preempted at least once", preemptions() > 0);
    t.note("sched: preemptions", preemptions());
}
