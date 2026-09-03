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
use alloc::vec;
use core::sync::atomic::{AtomicBool, Ordering};

/// Per-task kernel stack. Generous: these are `Vec` allocations from a 16 MiB
/// heap, and a stack overflow here has no guard page to catch it.
const STACK_SIZE: usize = 32 * 1024;

/// Maximum tasks, including the boot task in slot 0.
const MAX_TASKS: usize = 4;

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
}

impl Task {
    const fn empty() -> Self {
        Self {
            ctx: Context::empty(),
            state: State::Unused,
        }
    }
}

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
    // SAFETY: raw-pointer access to the table; single core, before any switch.
    unsafe {
        (*tasks())[0].state = State::Runnable;
    }
    set_current(0);
}

/// Create a task. Returns its slot, or `None` if the table is full.
pub fn spawn(entry: extern "C" fn() -> !) -> Option<usize> {
    // Leaked deliberately: a task's stack must outlive the frame that made it,
    // and nothing here ever reaps a task.
    let stack = vec![0u8; STACK_SIZE].leak();
    let stack_top = stack.as_ptr() as usize + STACK_SIZE;

    // SAFETY: raw-pointer access to the table; single core.
    unsafe {
        let t = tasks();
        for slot in 1..MAX_TASKS {
            if (*t)[slot].state == State::Unused {
                (*t)[slot].ctx = Context::for_task(stack_top, entry);
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
/// Set if any worker ever observed a timer-driven reschedule request.
static SAW_TICK_REQUEST: AtomicBool = AtomicBool::new(false);

/// How long a worker will wait for a tick before giving up on one.
///
/// Bounded so a dead timer makes the test *fail* rather than hang the boot —
/// the same rule as `lapic::smoke_test`.
const TICK_WAIT_BUDGET: u64 = 200_000_000;

/// Body shared by the three workers.
///
/// The accumulator is a **local**. It is read and written across a `yield_now`,
/// so if the switch failed to preserve this task's stack or its callee-saved
/// registers, the checksum comes out wrong — which is the property that
/// distinguishes a real context switch from a function call that happens to
/// return.
///
/// Each round waits for the timer to ask for a reschedule before yielding, so
/// the round-robin is genuinely **paced by the tick** rather than by how fast
/// the workers happen to run. The first version yielded immediately and the
/// whole test finished inside a single timer period, observing zero ticks — the
/// scheduler passed and the tick check correctly failed.
fn worker_body(id: usize) -> ! {
    let mut acc: u64 = 0;
    for round in 0..ROUNDS {
        acc = acc.wrapping_mul(31).wrapping_add(round + id as u64);

        // SAFETY: single core, one writer per index.
        unsafe {
            (*(&raw mut COUNTERS).cast::<[u64; WORKERS]>())[id] += 1;
        }

        let mut spins = 0u64;
        while !need_resched() && spins < TICK_WAIT_BUDGET {
            spins += 1;
            core::hint::spin_loop();
        }
        if need_resched() {
            SAW_TICK_REQUEST.store(true, Ordering::Relaxed);
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
    t.check(
        "sched: reschedule was tick-driven",
        SAW_TICK_REQUEST.load(Ordering::Relaxed),
    );
}
