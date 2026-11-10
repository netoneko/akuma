//! The scheduler's machine edge. See the crate description; the contracts are stated per item.
//!
//! `akuma-threading` carries `#![forbid(unsafe_code)]` and reaches every instruction, every raw
//! dereference and every symbol it needs through this crate. What lives here:
//!
//! * the **context table** ([`with_mut`]/[`with_ref`]/[`peek`]) - one `Context` per thread slot behind a
//!   per-slot borrow word, so a second overlapping `&mut` is a panic rather than a silent race;
//! * the **switch and entry assembly** and the addresses of its entry points;
//! * the **`msr tpidr*`** and **`msr ttbr0_el1`** writes;
//! * **fresh-stack writers** (a new thread's first frame) and the vouched stack/frame constructors;
//! * the **`ON_CPU`/`PER_CORE_OFFCPU`** gate words and `rust_switch_finished`, the symbol the vector asm calls;
//! * the typed boxes the scheduler hands through registers: the handle waker, the closure box, the
//!   `'static Location` read.
//!
//! `#![no_std]`, `unsafe` allowed. Nothing here makes a scheduling decision.

#![no_std]
#![allow(clippy::declare_interior_mutable_const, clippy::cast_ptr_alignment)]

extern crate alloc;

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use core::task::{RawWaker, RawWakerVTable, Waker};

use akuma_exec_core::thread::{Context, MAX_CORES, MAX_THREADS};
#[allow(unused_imports)]
use akuma_exec_core::thread::IRQ_FRAME_SIZE;
use akuma_stack::StackRegion;

// ── gate words ──────────────────────────────────────────────────────────────────────────────────────────

/// ON-CPU gate for the cross-core stack-sharing races (the SMP=4 boot-storm EL1
/// crashes / BKL_RUSTC_SCALING_BASELINE.md §5.1 corruption family).
///
/// Set while a thread is (or may still be) executing on some core; a thread may
/// be PICKED by a scheduler only when its gate is clear. Two distinct races
/// require this, and thread STATE alone catches neither:
///
/// 1. **Switch-out tail.** `commit_switch` marks the outgoing thread READY and
///    POOL is released when `sgi_scheduler_handler_with_sp` returns — but the
///    switching core keeps EXECUTING ON THE OUTGOING THREAD'S KERNEL STACK
///    until the vector asm's `mov sp, x0` (Rust epilogues, BKL reconcile and
///    tag bookkeeping all run on it after POOL is gone). A peer picking the
///    thread in that window resumes it onto a stack still in use.
/// 2. **Wake-before-switch-out.** `schedule_blocking` stores WAITING (often
///    with an already-expired deadline) while the thread is STILL RUNNING,
///    before its yield-SGI fires. A peer's wake-pass / `mark_thread_ready`
///    flips it READY and a peer scheduler resumes it from its STALE `ctx.sp`
///    (the previous switch-out's frame) while it is still running elsewhere —
///    a double-run on one stack. Observed as EL1 `ret` into kernel data /
///    `ELR=0x8` with SP in another thread's stack, and as §5.1's ERET to EL0
///    with a kernel register file.
///
/// Protocol: `commit_switch` sets the gate for the INCOMING thread (and
/// defensively re-asserts the outgoing thread's, which stays set from its own
/// switch-in); the outgoing tid is recorded per-core and its gate is cleared by
/// `rust_switch_finished`, called from the vector asm immediately AFTER
/// `mov sp, x0`. Bringup sites (boot thread, per-core idle claim) set the gate
/// for the thread they start life running. Pickers and the slot recycler skip
/// gated threads.
pub static ON_CPU: [AtomicU8; MAX_THREADS] = {
    const INIT: AtomicU8 = AtomicU8::new(0);
    [INIT; MAX_THREADS]
};

/// Per-core: the tid whose [`ON_CPU`] gate this core must clear once the
/// vector asm has switched SP (`usize::MAX` = none pending). Sized by
/// `types::MAX_CORES`, the SMP bringup cap.
pub static PER_CORE_OFFCPU: [AtomicUsize; MAX_CORES] = {
    const INIT: AtomicUsize = AtomicUsize::new(usize::MAX);
    [INIT; MAX_CORES]
};


/// Clear the outgoing thread's off-CPU gate; the vector asm and the trampolines below call it.
///
/// Called immediately after `mov sp, x0`: this core is
/// now off the outgoing thread's kernel stack, so peers may pick that thread up (or recycle its slot). Clears
/// the off-CPU gate `commit_switch` latched. Runs with IRQs masked on the incoming thread's stack; takes no locks.
#[unsafe(no_mangle)]
pub extern "C" fn rust_switch_finished() {
    let core = akuma_bkl::bkl::current_core_id() as usize;
    if core < MAX_CORES {
        let tid = PER_CORE_OFFCPU[core].swap(usize::MAX, Ordering::SeqCst);
        if tid < MAX_THREADS {
            ON_CPU[tid].store(0, Ordering::Release);
        }
    }
}

// ── registers ───────────────────────────────────────────────────────────────────────────────────────────

/// Set the current exception stack pointer (`TPIDR_EL1`).
///
/// Re-points the base every IRQ entry loads its stack from, so `stack_top` must be the top of a live exception
/// stack of the calling thread - the one fact the caller (`akuma-threading`) owns.
#[cfg(all(target_os = "none", target_arch = "aarch64"))]
#[inline]
pub fn set_exception_stack(stack_top: u64) {
    // SAFETY: a single `msr` with no memory operands; see the doc for what the value must be.
    unsafe { core::arch::asm!("msr tpidr_el1, {}", in(reg) stack_top) };
}

/// Off-target stub: nothing to set.
#[cfg(not(all(target_os = "none", target_arch = "aarch64")))]
#[inline]
pub fn set_exception_stack(_stack_top: u64) {}

/// Set the current thread id in `TPIDRRO_EL0` (read-only from EL0). Every per-slot static is indexed with it.
///
/// Both AArch64 scheduler paths: the SGI switch calls it, and so does the root kernel's hooked
/// machine half (`akuma-kernel-glue`'s `hooked`), whose `set_current_slot` hook it is.
#[cfg(all(target_os = "none", target_arch = "aarch64"))]
#[inline]
pub fn set_thread_register(tid: usize) {
    // SAFETY: a single `msr` with no memory operands; the caller is the scheduler naming the thread it runs.
    unsafe { core::arch::asm!("msr tpidrro_el0, {}", in(reg) tid as u64) };
}

/// Install `new_ttbr0` and flush the TLB (`dsb; msr ttbr0_el1; isb; tlbi vmalle1; dsb; isb`).
///
/// The kernel runs in the TTBR0 low half, so a TTBR0 that does not map kernel text is a machine-wedging fault
/// loop at the next fetch: the caller must pass a root it has just validated (the scheduler installs a
/// thread's own saved root). Refuses (returns `false`) the one value that can never be a root.
///
/// Both AArch64 scheduler paths, for the reason [`set_thread_register`] is.
#[cfg(all(target_os = "none", target_arch = "aarch64"))]
#[must_use]
pub fn install_ttbr0_flush(new_ttbr0: u64) -> bool {
    if new_ttbr0 & 0x0000_FFFF_FFFF_F000 == 0 {
        return false;
    }
    // SAFETY: see the doc - the TTBR0 names the incoming thread's own tables, which map the kernel.
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "msr ttbr0_el1, {ttbr0}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            ttbr0 = in(reg) new_ttbr0,
        );
    }
    true
}

// ── context table ───────────────────────────────────────────────────────────────────────────────────────

/// One slot's `Context`. See [`with_mut`] for who may touch it and when.
#[repr(transparent)]
struct Cell(UnsafeCell<Context>);

// SAFETY: every access path in this crate goes through the borrow word in `BORROW` (`with_mut`/`with_ref`),
// the relaxed word-sized `peek`, or the switch asm, which the switch function gates on that same word.
unsafe impl Sync for Cell {}

static CONTEXTS: [Cell; MAX_THREADS] = {
    const INIT: Cell = Cell(UnsafeCell::new(Context::zero()));
    [INIT; MAX_THREADS]
};

/// Per-slot borrow word: `0` free, `1..WRITER` that many readers, `WRITER` one writer.
static BORROW: [AtomicU32; MAX_THREADS] = {
    const INIT: AtomicU32 = AtomicU32::new(0);
    [INIT; MAX_THREADS]
};
const WRITER: u32 = u32::MAX;

struct WriteGuard(usize);
impl Drop for WriteGuard {
    fn drop(&mut self) {
        BORROW[self.0].store(0, Ordering::Release);
    }
}
struct ReadGuard(usize);
impl Drop for ReadGuard {
    fn drop(&mut self) {
        BORROW[self.0].fetch_sub(1, Ordering::Release);
    }
}

/// Run `f` on slot `idx`'s `Context`, exclusively.
///
/// The scheduler's cross-core protocol decides *who* may call this (the slot's `INITIALIZING` claimant, the
/// holder of `POOL` across a switch, or the thread itself - `docs/archive/BKL_PHASE7D_THREAD_CONTEXTS.md`);
/// this function **checks** the part a type can: if any other `with_mut`/`with_ref` on the same slot is live,
/// it panics. A lost claim therefore stops the kernel at the second writer instead of corrupting a stack
/// pointer.
///
/// # Panics
/// If `idx >= MAX_THREADS`, or the slot is already borrowed.
pub fn with_mut<R>(idx: usize, f: impl FnOnce(&mut Context) -> R) -> R {
    assert!(
        BORROW[idx].compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed).is_ok(),
        "Context slot {idx} borrowed while a write was requested"
    );
    let _guard = WriteGuard(idx);
    // SAFETY: the CAS above made this the only live borrow of the slot, and `switch_contexts` refuses to run
    // while the word is non-zero, so no other path reads or writes the cell until `_guard` drops.
    f(unsafe { &mut *CONTEXTS[idx].0.get() })
}

/// Run `f` on a shared view of slot `idx`'s `Context`. Panics if a writer is live. Many readers may overlap.
///
/// # Panics
/// If `idx >= MAX_THREADS`, or a `with_mut` on the slot is live.
pub fn with_ref<R>(idx: usize, f: impl FnOnce(&Context) -> R) -> R {
    let mut cur = BORROW[idx].load(Ordering::Relaxed);
    loop {
        assert!(cur < WRITER - 1, "Context slot {idx} read while a write is live");
        match BORROW[idx].compare_exchange_weak(cur, cur + 1, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => break,
            Err(now) => cur = now,
        }
    }
    let _guard = ReadGuard(idx);
    // SAFETY: the borrow word excludes writers while `_guard` lives.
    f(unsafe { &*CONTEXTS[idx].0.get() })
}

/// Overwrite slot `idx`'s `Context` with `ctx`.
pub fn set(idx: usize, ctx: Context) {
    with_mut(idx, |c| *c = ctx);
}

/// The `Context` words display-only paths and self-read paths look at.
#[derive(Clone, Copy)]
pub enum CtxField {
    #[cfg(target_arch = "aarch64")]
    Sp,
    #[cfg(target_arch = "aarch64")]
    Ttbr0,
    #[cfg(target_arch = "aarch64")]
    X30,
    #[cfg(target_arch = "aarch64")]
    UserEntry,
    #[cfg(target_arch = "aarch64")]
    UserSp,
    #[cfg(target_arch = "aarch64")]
    IsUserProcess,
    #[cfg(target_arch = "x86_64")]
    Rsp,
}

impl CtxField {
    fn offset(self) -> usize {
        use core::mem::offset_of;
        match self {
            #[cfg(target_arch = "aarch64")]
            Self::Sp => offset_of!(Context, sp),
            #[cfg(target_arch = "aarch64")]
            Self::Ttbr0 => offset_of!(Context, ttbr0),
            #[cfg(target_arch = "aarch64")]
            Self::X30 => offset_of!(Context, x30),
            #[cfg(target_arch = "aarch64")]
            Self::UserEntry => offset_of!(Context, user_entry),
            #[cfg(target_arch = "aarch64")]
            Self::UserSp => offset_of!(Context, user_sp),
            #[cfg(target_arch = "aarch64")]
            Self::IsUserProcess => offset_of!(Context, is_user_process),
            #[cfg(target_arch = "x86_64")]
            Self::Rsp => offset_of!(Context, rsp),
        }
    }
}

/// Read one word of slot `idx`'s context without owning the slot (the accepted display-only race).
///
/// One relaxed, naturally aligned word load: the value may be stale but is never torn, and it takes no
/// borrow, so it is legal while the owner holds a `with_mut`. **Do not use the result for a correctness
/// decision**, and do not use it where the caller owns the slot.
///
/// # Panics
/// If `idx >= MAX_THREADS`.
#[must_use]
pub fn peek(idx: usize, field: CtxField) -> u64 {
    word_cell(idx, field).load(Ordering::Relaxed)
}

/// An atomic view of one word of slot `idx`'s context. Holds no borrow: for [`peek`] and for the host
/// tests that race claimants on purpose.
#[doc(hidden)]
#[must_use]
pub fn word_cell(idx: usize, field: CtxField) -> &'static AtomicU64 {
    let base = CONTEXTS[idx].0.get().cast::<u8>();
    // SAFETY: `CONTEXTS` is a `static`, so the cell is live for the whole run; `field.offset()` is the offset
    // of a `u64` field of the `repr(C)` `Context`, so the address is 8-aligned and inside the cell, and an
    // `AtomicU64` view of it is valid for atomic access from any thread. A concurrent plain write by the
    // owner is at worst observed stale by a relaxed load.
    unsafe { AtomicU64::from_ptr(base.add(field.offset()).cast::<u64>()) }
}

// ── the switch ──────────────────────────────────────────────────────────────────────────────────────────



extern "C" fn thread_start() -> ! { panic!("not on bare metal aarch64") }
extern "C" fn thread_start_closure() -> ! { panic!("not on bare metal aarch64") }

/// Address of the SGI path's `extern "C"` thread entry trampoline.
#[must_use]
pub fn thread_start_addr() -> u64 { thread_start as *const () as u64 }

/// Address of the SGI path's closure entry trampoline (`x19` = fn, `x20` = data, `x21` = IRQ-mask flag).
#[must_use]
pub fn thread_start_closure_addr() -> u64 { thread_start_closure as *const () as u64 }

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    r#"
    .section .text
.global akuma_threading_x86_switch_context
akuma_threading_x86_switch_context:
    /* System V: rdi = &mut old.rsp is NOT how this is called — see the Rust
     * wrapper below, which passes raw `*mut Context`/`*const Context` whose
     * first field is `magic`, not `rsp`. The offset is computed there so this
     * asm never has to know the struct layout beyond "rsp is a u64 field",
     * matching `amd64::sched::switch_context`'s own contract one level up.
     *
     * `pushfq`/`popfq` are not symmetry for its own sake, and leaving them out
     * is how this port shipped originally: `IF` has to belong to the task, not
     * to whoever last ran on this core. A syscall (interrupts off) that yields
     * to a kernel task (interrupts on) must come back with them off, and it did
     * not until `amd64/src/sched.rs` Stage U — the resumer's state leaked into
     * the resumed, presenting as an unrelated intermittent hang. Restored here
     * 2026-09-06 before anything adopts this switch
     * (`docs/archive/AKUMA_PIPES_EXTRACTION.md` § "two of them disagree"). */
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

.global akuma_threading_x86_thread_entry_trampoline
akuma_threading_x86_thread_entry_trampoline:
    /* Reached via the `ret` above the first time a freshly spawned thread is
     * switched in — see `build_closure_context`, which pre-loads r12 =
     * closure_ptr and rbx = the boxed-closure trampoline fn pointer at
     * exactly the stack slots this switch's `pop`s read them from. `jmp`,
     * not `call`: the trampoline is `fn(*mut ()) -> !` and never returns, so
     * there is no return address to arrange.
     *
     * First scheduler entry of a never-yet-run thread (ON_CPU's doc: the
     * outgoing tid's gate "is cleared by rust_switch_finished ... at the
     * task's first scheduler entry" — the AArch64 vector asm does it right
     * after `mov sp, x0`). This trampoline IS that first entry on this
     * target: a fresh task never returns to x86_switch_context's tail, so
     * its `rust_switch_finished()` never runs for us. Without the call here
     * the OUTGOING thread's ON_CPU gate stays latched forever, and pickers
     * and `runnable_exists` skip gated threads — the preempted thread
     * can never be scheduled again. Seen on Taoxichuan stage 12: the first
     * `sleep_until` park yielded (per its own contract) to the freshly
     * seeded idle thread, the gate on the parker latched, idle saw nothing
     * runnable and halted, and the whole guest wedged on its first timed
     * poll (2026-10-31). Stack alignment: rsp == entry_slot here, 16-byte
     * aligned, exactly the pre-`call` ABI state. */
    call rust_switch_finished
    mov rdi, r12
    jmp rbx
"#
);

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    fn akuma_threading_x86_switch_context(old_rsp: *mut u64, new_rsp: *const u64);
    fn akuma_threading_x86_thread_entry_trampoline() -> !;
}

// Bare metal only, like the SGI path's asm above: a host build (`cargo test` with the default
// features, which carry `hooked-sched`) assembles for Mach-O/ELF-host, where `.section .text` is not
// valid, and never switches: `switch_contexts` has a host arm, and the trampoline a host stub.
#[cfg(all(target_os = "none", target_arch = "aarch64"))]
core::arch::global_asm!(
    r#"
    .section .text
.global akuma_threading_hooked_switch_context
akuma_threading_hooked_switch_context:
    /* x0 = *mut Context (outgoing), x1 = *const Context (incoming). */
    sub sp, sp, #64
    stp d8, d9, [sp, #0]
    stp d10, d11, [sp, #16]
    stp d12, d13, [sp, #32]
    stp d14, d15, [sp, #48]
    stp x19, x20, [x0, #8]
    stp x21, x22, [x0, #24]
    stp x23, x24, [x0, #40]
    stp x25, x26, [x0, #56]
    stp x27, x28, [x0, #72]
    stp x29, x30, [x0, #88]
    mov x9, sp
    str x9, [x0, #104]
    mrs x9, daif
    str x9, [x0, #112]

    ldp x19, x20, [x1, #8]
    ldp x21, x22, [x1, #24]
    ldp x23, x24, [x1, #40]
    ldp x25, x26, [x1, #56]
    ldp x27, x28, [x1, #72]
    ldp x29, x30, [x1, #88]
    ldr x9, [x1, #104]
    mov sp, x9
    ldp d8, d9, [sp, #0]
    ldp d10, d11, [sp, #16]
    ldp d12, d13, [sp, #32]
    ldp d14, d15, [sp, #48]
    add sp, sp, #64
    ldr x9, [x1, #112]
    msr daif, x9
    ret

.global akuma_threading_hooked_thread_entry_trampoline
akuma_threading_hooked_thread_entry_trampoline:
    /* First switch-in of a fresh thread: the `ret` above lands here with
     * x19 = closure_ptr and x20 = the boxed-closure trampoline, seeded by
     * `build_closure_context`. As on x86, this is the task's first scheduler
     * entry, so it clears the outgoing thread's ON_CPU gate itself. `br`, not
     * `blr`: the trampoline never returns. */
    bl rust_switch_finished
    mov x0, x19
    br x20
"#
);

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
unsafe extern "C" {
    fn akuma_threading_hooked_switch_context(old: *mut Context, new: *const Context);
    fn akuma_threading_hooked_thread_entry_trampoline() -> !;
}

/// Host stand-in for the entry trampoline; its address is all a host build takes.
#[cfg(all(not(target_os = "none"), target_arch = "aarch64"))]
extern "C" fn akuma_threading_hooked_thread_entry_trampoline() -> ! {
    unreachable!("akuma-switch: host builds have no context switch");
}

#[cfg(target_arch = "aarch64")]
const _: () = {
    assert!(core::mem::offset_of!(Context, x19) == 8);
    assert!(core::mem::offset_of!(Context, x29) == 88);
    assert!(core::mem::offset_of!(Context, x30) == 96);
    assert!(core::mem::offset_of!(Context, sp) == 104);
    assert!(core::mem::offset_of!(Context, daif) == 112);
};


/// Save the current thread's register state onto its own stack, switch to `new`'s stack, and resume it - at
/// the point it last called this same function, or (never run) at the entry trampoline.
///
/// Safe because it checks what a function can: both indices in range and distinct, and neither slot borrowed
/// through [`with_mut`]/[`with_ref`] (the switch reads `new`'s saved words and writes `old`'s). The part it
/// cannot see is the scheduler's: `new` must not be running on another core (`ON_CPU`) and `old` must be
/// the thread executing this call - both stated at the one call site in `akuma-threading`.
///
/// # Panics
/// If `old == new`, an index is out of range, or either slot is borrowed.
pub fn switch_contexts(old: usize, new: usize) {
    assert!(old != new, "switch_contexts: old == new ({old})");
    assert!(
        BORROW[old].load(Ordering::Acquire) == 0 && BORROW[new].load(Ordering::Acquire) == 0,
        "switch_contexts: slot {old} or {new} is borrowed"
    );
    let (o, n) = (CONTEXTS[old].0.get(), CONTEXTS[new].0.get());
    #[cfg(target_arch = "x86_64")]
    // SAFETY: see the doc; `rsp` is `Context`'s only register field on this target, spelled so the asm only
    // has to know "a `u64` slot".
    unsafe {
        akuma_threading_x86_switch_context(&raw mut (*o).rsp, &raw const (*n).rsp);
    }
    #[cfg(all(target_os = "none", target_arch = "aarch64"))]
    // SAFETY: see the doc.
    unsafe {
        akuma_threading_hooked_switch_context(o, n);
    }
    // Host builds (`cargo test` with the default features) have no switch asm and never switch.
    #[cfg(all(not(target_os = "none"), target_arch = "aarch64"))]
    {
        let _ = (o, n);
        unreachable!("akuma-switch: host builds have no context switch");
    }
}

/// The address a fresh hooked thread's first switch returns into.
#[cfg(target_arch = "x86_64")]
#[must_use]
pub fn entry_trampoline_addr() -> u64 { akuma_threading_x86_thread_entry_trampoline as *const () as u64 }

/// The address a fresh hooked thread's first switch returns into.
#[cfg(target_arch = "aarch64")]
#[must_use]
pub fn entry_trampoline_addr() -> u64 { akuma_threading_hooked_thread_entry_trampoline as *const () as u64 }

// ── fresh-stack writers ─────────────────────────────────────────────────────────────────────────────────

/// How much of a stack's top the writers below may touch: a frame plus the register file, nothing deeper.
#[allow(dead_code)]
const SEED_WINDOW: usize = 4096;

/// A stack's region, vouched by its owner.
///
/// The one place the scheduler states that `[base, base + size)` is
/// mapped RAM it owns: a slot's PMM-backed stack (mapped for the slot's life), the boot stack, or the canary
/// words at a stack's base. None is aliased by a Rust reference.
#[must_use]
pub fn vouch_stack(base: usize, size: usize) -> StackRegion {
    // SAFETY: the contract above, which the caller asserts by calling; it is the same claim the scheduler
    // made with `StackRegion::new` before this crate existed.
    unsafe { StackRegion::new(base, size) }
}

/// The hooked path's initial `rflags`: `IF` set plus the always-one bit 1.
#[cfg(target_arch = "x86_64")]
const X86_INITIAL_RFLAGS: u64 = 0x202;

/// Build the [`Context`] for a thread that has never run, so the first [`switch_contexts`] into it starts
/// `trampoline(closure_ptr)` (x86_64). `stack_top` is the top of a stack nothing runs on yet.
///
/// Seven words below `entry_slot`: `r15 r14 r13 r12 rbx rbp rflags`, then the return address. A zero
/// `rflags` would start the thread with interrupts masked forever.
///
/// # Panics
/// If `stack_top` is too small to hold the frame.
#[cfg(target_arch = "x86_64")]
#[must_use]
pub fn build_closure_context(stack_top: usize, trampoline: u64, closure_ptr: u64) -> Context {
    let entry_slot = (stack_top - 16) & !0xf;
    let region = vouch_stack(stack_top - SEED_WINDOW, SEED_WINDOW);
    let back = |k: usize| stack_top - (entry_slot - 8 * k);
    let words = [
        entry_trampoline_addr(), // return address
        X86_INITIAL_RFLAGS,      // rflags
        0,                       // rbp
        trampoline,              // rbx
        closure_ptr,             // r12
        0,                       // r13
        0,                       // r14
        0,                       // r15
    ];
    for (k, w) in words.into_iter().enumerate() {
        assert!(region.write_word_below_top(back(k), w), "x86 context frame outside the stack");
    }
    let mut ctx = Context::zero();
    ctx.rsp = (entry_slot - 56) as u64;
    ctx
}

/// A fresh thread's [`Context`] on the aarch64 hooked path: a zeroed d8-d15 block, `DAIF` = 0, returning into
/// the entry trampoline with `closure_ptr` in x19 and `trampoline` in x20.
#[cfg(target_arch = "aarch64")]
#[must_use]
pub fn build_closure_context(stack_top: usize, trampoline: u64, closure_ptr: u64) -> Context {
    let entry_sp = (stack_top & !0xf) - 64;
    let region = vouch_stack(stack_top - SEED_WINDOW, SEED_WINDOW);
    assert!(region.zero_below_top(stack_top - entry_sp, 64), "hooked context frame outside the stack");
    let mut ctx = Context::zero();
    ctx.x19 = closure_ptr;
    ctx.x20 = trampoline;
    ctx.x30 = entry_trampoline_addr();
    ctx.sp = entry_sp as u64;
    ctx.daif = 0;
    ctx
}

/// Name the IRQ frame at `sp` (aarch64): the live frame the vector asm just built, or the `Context.sp` of a
/// thread that is not running (its stack cannot be freed or reused while it is not running).
#[cfg(target_arch = "aarch64")]
#[must_use]
pub fn irq_frame_at(sp: u64) -> akuma_trap_aarch64::irq_frame::IrqFrame {
    assert!(sp != 0 && sp.is_multiple_of(16), "irq_frame_at: bad frame address {sp:#x}");
    // SAFETY: the doc above is the contract; the alignment and null checks are what can be checked here.
    unsafe { akuma_trap_aarch64::irq_frame::IrqFrame::at(sp) }
}

/// Write the word at byte `off` of the IRQ frame at `sp` (aarch64): how fork patches a child's `x0`/`x1` into the
/// frame its first switch-in will restore. `sp` is a not-running thread's `Context.sp`.
///
/// # Panics
/// If `off` is not 8-aligned or the word is outside the [`IRQ_FRAME_SIZE`]-byte frame.
#[cfg(target_arch = "aarch64")]
pub fn write_frame_word(sp: u64, off: usize, value: u64) {
    assert!(sp != 0 && sp.is_multiple_of(16), "write_frame_word: bad frame address {sp:#x}");
    let region = vouch_stack(sp as usize, IRQ_FRAME_SIZE);
    assert!(
        off.is_multiple_of(8) && region.write_word_below_top(IRQ_FRAME_SIZE - off, value),
        "write_frame_word: offset {off} outside the frame"
    );
}

/// Read the word at `addr` on a not-running thread's kernel stack (the saved-frame check before a switch).
///
/// # Panics
/// If `addr` is null or not 8-aligned.
#[must_use]
pub fn read_stack_word(addr: usize) -> u64 {
    assert!(addr != 0 && addr.is_multiple_of(8), "read_stack_word: bad address {addr:#x}");
    // SAFETY: a kernel stack word of a thread nothing is running (`ON_CPU`), which the caller is about to
    // restore from anyway; null and alignment are checked above.
    unsafe { (addr as *const u64).read_volatile() }
}

/// The bytes of a `#[repr(C)]` plain-data value, for tests that pin a wire layout.
#[doc(hidden)]
#[must_use]
pub fn bytes_of<T: Copy, const N: usize>(v: &T) -> [u8; N] {
    assert_eq!(core::mem::size_of::<T>(), N, "bytes_of: size mismatch");
    // SAFETY: sizes match; the caller passes integer-only `repr(C)` data with no padding it asserts on.
    unsafe { core::mem::transmute_copy(v) }
}

/// Set up a fake IRQ frame on a new thread's stack and return the SP that points at it (aarch64 SGI path).
///
/// `[sp+0]` x30 = `exit_stub`, `[sp+80]` x20, `[sp+88]` x21, `[sp+104]` x19, `ELR`/`SPSR` through the
/// frame view; every other word is zero. `x21 != 0` starts the thread with IRQs masked.
///
/// # Panics
/// If the frame does not fit below `stack_top`.
#[cfg(target_arch = "aarch64")]
#[must_use]
pub fn setup_fake_irq_frame(stack_top: u64, entry_point: u64, x19: u64, x20: u64, x21: u64, exit_stub: u64) -> u64 {
    let size = IRQ_FRAME_SIZE;
    let frame_base = stack_top - size as u64;
    let region = vouch_stack(frame_base as usize, size);
    assert!(region.zero_below_top(size, size), "fake IRQ frame outside the stack");
    let put = |off: usize, v: u64| assert!(region.write_word_below_top(size - off, v), "fake IRQ frame word");
    put(0, exit_stub);
    put(80, x20);
    put(88, x21);
    put(104, x19);
    let view = irq_frame_at(frame_base);
    view.write_elr(entry_point);
    view.write_spsr(if x21 != 0 { 0x0000_03C5 } else { 0x0000_0345 });
    frame_base
}

// ── typed boxes the scheduler hands through registers ───────────────────────────────────────────────────

/// Wake by a packed handle: the one effect a [`handle_waker`] performs.
pub trait HandleWake {
    fn wake(handle: u64);
}

struct Vt<W>(PhantomData<W>);

impl<W: HandleWake> Vt<W> {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(Self::clone, Self::wake, Self::wake_by_ref, Self::drop);

    fn raw(handle: u64) -> RawWaker {
        RawWaker::new(handle as *const (), &Self::VTABLE)
    }
    unsafe fn clone(data: *const ()) -> RawWaker {
        RawWaker::new(data, &Self::VTABLE)
    }
    unsafe fn wake(data: *const ()) {
        W::wake(data as u64);
    }
    unsafe fn wake_by_ref(data: *const ()) {
        W::wake(data as u64);
    }
    unsafe fn drop(_data: *const ()) {}
}

/// A `Waker` whose data word *is* the packed handle: no allocation, and a clone is a copy of the word.
#[must_use]
pub fn handle_waker<W: HandleWake>(handle: u64) -> Waker {
    // SAFETY: the vtable's four functions uphold `RawWaker`'s contract trivially: the data word is an integer
    // owned by nobody, `clone` copies it, `drop` releases nothing, and `wake` only calls `W::wake`.
    unsafe { Waker::from_raw(Vt::<W>::raw(handle)) }
}

/// Run the boxed closure behind `ptr`, which `Box::into_raw(Box::new(f))` produced for this exact `F`.
///
/// `ptr` crosses a register (`x20` / `r12`) on its way here, so the type cannot ride with it; the
/// spawn paths pair `closure_trampoline::<F>` with the box of the same `F` and nothing else reaches this.
pub fn call_boxed_closure<F: FnOnce() -> !>(ptr: *mut ()) -> ! {
    // SAFETY: see the doc - `ptr` is an unconsumed `Box<F>`.
    let closure = unsafe { Box::from_raw(ptr.cast::<F>()) };
    closure()
}

/// Drop the boxed closure behind `ptr` without running it (a spawn that failed after boxing).
pub fn drop_boxed_closure<F>(ptr: *mut ()) {
    // SAFETY: as [`call_boxed_closure`]: an unconsumed `Box<F>`.
    drop(unsafe { Box::from_raw(ptr.cast::<F>()) });
}

/// A `&'static Location` from the address `note_park_site` stored, or `None` for 0.
///
/// The only writer stores `Location::caller()` as an address; a `Location` is in the binary's read-only data
/// and lives for the whole program, so the pointer never dangles.
#[must_use]
pub fn static_location(addr: usize) -> Option<&'static core::panic::Location<'static>> {
    if addr == 0 {
        return None;
    }
    // SAFETY: see the doc.
    Some(unsafe { &*(addr as *const core::panic::Location<'static>) })
}

/// Copy the `T` a thread published at `addr` (its EL0 trap frame on its own kernel stack), or `None` for 0.
///
/// A non-zero entry names a live, aligned `T`: stored by the owning thread's syscall entry and cleared by its
/// epilogue.
#[must_use]
pub fn read_published<T: Copy>(addr: usize) -> Option<T> {
    if addr == 0 || !addr.is_multiple_of(core::mem::align_of::<T>()) {
        return None;
    }
    // SAFETY: see the doc; null and alignment were checked above.
    Some(unsafe { core::ptr::read(addr as *const T) })
}
