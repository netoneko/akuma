//! The scheduler's machine half for the AArch64 root kernel.
//!
//! `akuma-threading`'s hooked path — the one amd64 and the tokio kernels run — switches
//! threads inside an ordinary call and asks the kernel for the machine effects through
//! `ArchHooks`. This module is that table for
//! this kernel, its idle threads and its EL0 entry. The trap front end that feeds it is
//! `akuma_entry::hooked` (`akuma-vectors-aarch64`, D4 in
//! `docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`).
//!
//! What a switch installs is small. An EL0 trap saves the whole user register file in a
//! frame at the top of the thread's own kernel stack and the crossing back restores it from
//! there, so `SP_EL0` and the vector registers need nothing per switch; the switch itself
//! saves the callee-saved kernel state. What is left is the page-table root (`TTBR0_EL1` —
//! the kernel lives in the low half, so every root maps it), the thread's name
//! (`TPIDRRO_EL0`, which every per-slot table is indexed with) and `TPIDR_EL0`.
//!
//! `TPIDR_EL0` is not covered by the frame, because the syscall epilogue in
//! `akuma-exceptions` copies the LIVE register into the frame after every syscall (that is
//! how `set_tpidr_el0` sticks). A thread that blocked inside the syscall finishes it after
//! other threads have run on the core, and each of those set the register on its way to
//! EL0; without a per-switch save the epilogue stamped the last one's TLS into this
//! thread's frame. A two-thread program SIGSEGVed on its first `pthread_join`, with the
//! exited thread's (already unmapped) TLS as its own. The deleted SGI scheduler's IRQ entry
//! saved the register around every switch, which is why only this path had it.
//!
//! # The page-table root, per slot
//!
//! A thread's root is whatever was live when it last switched out — the address space it
//! activated, or the boot tables for a kernel thread — recorded here with the slot's
//! generation and installed when it next switches in. The generation is what keeps a
//! recycled slot from installing the previous occupant's root, which may have been freed
//! since: a mismatch, or a root `akuma-mmu` freed recently, installs the boot tables
//! instead. The record is also this kernel's answer to the free gate's question "does an
//! off-CPU thread still carry this root?" ([`saved_root_probe`]) — the SGI path keeps the
//! same value in its saved `Context`, which this path does not have.
//!
//! # The lock
//!
//! `akuma_bkl` stays with the core across a switch: every thread crosses one holding it
//! (`akuma-threading`'s `yield-takes-bkl` takes it for a thread yielding from a dropped
//! window), the EL0 entry releases it, a trap takes it again. So there is no per-thread
//! depth to move and `transfer_lock_depth` is a no-op, as on amd64.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};

use akuma_exec::process::UserContext;
use akuma_exec::threading::{self, ArchHooks};
use akuma_exec_core::thread::{MAX_CORES, MAX_THREADS, thread_state};
use akuma_primitives::safe_print;
use akuma_vectors_aarch64::Frame;

/// The bits of a `TTBR0_EL1` value that name the L0 table.
const L0_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// Each slot's page-table root as of its last switch-out (`0`: none recorded).
static SAVED_ROOT: [AtomicU64; MAX_THREADS] = [const { AtomicU64::new(0) }; MAX_THREADS];
/// The slot generation [`SAVED_ROOT`]'s entry was recorded under.
static SAVED_GEN: [AtomicU64; MAX_THREADS] = [const { AtomicU64::new(0) }; MAX_THREADS];
/// Each slot's `TPIDR_EL0` as of its last switch-out (see the module doc). A slot that never
/// ran, or a recycled one, installs a stale value, which is harmless: its first crossing to
/// EL0 restores its own frame's value before anything at EL0 reads the register.
static SAVED_TLS: [AtomicU64; MAX_THREADS] = [const { AtomicU64::new(0) }; MAX_THREADS];
/// Core-pinned idle threads, which no picker may offer.
static IDLE: [AtomicBool; MAX_THREADS] = [const { AtomicBool::new(false) }; MAX_THREADS];
/// Each core's idle thread (`usize::MAX`: none yet).
static IDLE_OF_CORE: [AtomicUsize; MAX_CORES] = [const { AtomicUsize::new(usize::MAX) }; MAX_CORES];
/// Switch-ins that found a recorded root freed or stale and installed the boot tables.
static ROOT_DEMOTIONS: AtomicU64 = AtomicU64::new(0);

fn current_slot() -> usize {
    akuma_primitives::preempt::current_tid()
}

fn set_current_slot(slot: usize) {
    akuma_switch::set_thread_register(slot);
}

fn core_index() -> usize {
    akuma_exec::bkl::current_core_id() as usize % MAX_CORES
}

fn idle_slot() -> usize {
    let idle = IDLE_OF_CORE[core_index()].load(Relaxed);
    // Before this core has an idle thread the caller is its only thread.
    if idle == usize::MAX { current_slot() } else { idle }
}

fn can_run(slot: usize) -> bool {
    IDLE.get(slot).is_some_and(|i| !i.load(Relaxed))
}

/// The root `slot` should run in: its record when that is still its own and still live,
/// else the boot tables.
fn root_for(slot: usize) -> u64 {
    let boot = akuma_exec::mmu::get_boot_ttbr0();
    let Some(rec) = SAVED_ROOT.get(slot) else { return boot };
    let root = rec.swap(0, Relaxed);
    if root == 0 {
        return boot;
    }
    let same_occupant = SAVED_GEN[slot].load(Relaxed) == threading::thread_generation(slot);
    if same_occupant && !akuma_exec::mmu::l0_recently_freed(root & L0_MASK) {
        return root;
    }
    let n = ROOT_DEMOTIONS.fetch_add(1, Relaxed);
    if n < 8 || n.is_power_of_two() {
        safe_print!(
            128,
            "[hooked] slot {} root {:#x} is {} - boot tables instead (x{})\n",
            slot,
            root,
            if same_occupant { "freed" } else { "a previous occupant's" },
            n + 1
        );
    }
    boot
}

fn switch_to(from: usize, to: usize) {
    // An idle thread may be leaving with its tick stopped (`idle_main`'s halt, ended by an
    // SGI that switched it out before it could re-arm): the incoming thread needs the tick.
    if IDLE.get(from).is_some_and(|i| i.load(Relaxed)) {
        crate::timer::rearm_tick();
    }
    // BKL-hold profiler (one relaxed load unless `bkl-profile` turned it on): the core's
    // sampling cache follows the incoming thread, as the SGI path's
    // `set_current_thread_register` does. Without it a waiter blames the incoming thread's
    // hold on whatever the outgoing one was doing.
    akuma_exec::sync::load_thread_tag_to_core(akuma_exec::bkl::current_core_id(), to);
    if let (Some(out), Some(inc)) = (SAVED_TLS.get(from), SAVED_TLS.get(to)) {
        out.store(akuma_cpu::sysreg::tpidr_el0(), Relaxed);
        akuma_cpu::sysreg::set_tpidr_el0(inc.load(Relaxed));
    }
    let live = akuma_exec::mmu::get_current_ttbr0() as u64;
    if let Some(rec) = SAVED_ROOT.get(from) {
        // A finished thread never switches in again; recording its root would only make
        // the free gate wait on it.
        let keep = threading::get_thread_state(from) != thread_state::TERMINATED;
        SAVED_GEN[from].store(threading::thread_generation(from), Relaxed);
        rec.store(if keep { live } else { 0 }, Relaxed);
    }
    let root = root_for(to);
    if root & L0_MASK == live & L0_MASK {
        return;
    }
    // Publish the transition to the per-core live-root registry the address-space free
    // gate reads (`mmu::any_core_on_l0`), as the SGI switch and `activate`/`deactivate`
    // do: PREV covers the outgoing root across the `msr`, ACTIVE the incoming one from
    // before the hardware can walk it. Unpublished, the registry went on naming whatever
    // the core last `activate`d — pinning that address space after its process died, and
    // leaving the root installed here unprotected while it was live.
    let token = akuma_exec::mmu::publish_l0_begin(root, akuma_exec::bkl::current_core_id() as usize);
    if !akuma_switch::install_ttbr0_flush(root) {
        safe_print!(96, "[hooked] slot {} has no page-table root to install\n", to);
        akuma_exec::bkl::leave_kernel();
        loop {
            akuma_cpu::park::wfi();
        }
    }
    akuma_exec::mmu::publish_l0_end(token);
}

/// The free gate's probe: an off-CPU slot that can still be switched in, carrying the
/// root at `l0_base`. Lock-free and bounded; a stale-generation record is not installable
/// ([`root_for`]) and does not count.
fn saved_root_probe(l0_base: u64) -> Option<(usize, u8)> {
    let l0 = l0_base & L0_MASK;
    if l0 == 0 {
        return None;
    }
    (0..MAX_THREADS)
        .find(|&i| {
            let named = SAVED_ROOT[i].load(Relaxed) & L0_MASK == l0
                && SAVED_GEN[i].load(Relaxed) == threading::thread_generation(i);
            let state = threading::get_thread_state(i);
            named && (state != thread_state::TERMINATED && state != thread_state::FREE || threading::on_cpu_flag(i))
        })
        .map(|i| (i, threading::get_thread_state(i)))
}

/// Boot-suite hook: record `root` as `slot`'s saved root under its current generation,
/// returning the previous record — the hooked path's `threading::test_swap_saved_ctx_ttbr0`,
/// which stages "an off-CPU thread still carries this root" for the free-gate test. Never
/// call this outside tests.
#[doc(hidden)]
pub fn test_swap_saved_root(slot: usize, root: u64) -> u64 {
    let Some(rec) = SAVED_ROOT.get(slot) else { return 0 };
    SAVED_GEN[slot].store(threading::thread_generation(slot), Relaxed);
    rec.swap(root, Relaxed)
}

/// A user slot's kernel stack (the pool's), with the slot's per-thread records reset.
fn prepare_task_slot(slot: usize) -> Option<usize> {
    let top = threading::ensure_pool_stack_top(slot, crate::config::USER_THREAD_STACK_SIZE)?;
    IDLE[slot].store(false, Relaxed);
    SAVED_ROOT[slot].store(0, Relaxed);
    Some(top)
}

/// Fork and clone children start from their process's image context (the entry
/// trampoline reads it), so there is nothing to keep per slot.
fn write_user_context(_slot: usize, _ctx: &UserContext) {}

fn read_user_context(slot: usize) -> Option<UserContext> {
    threading::published_user_context(slot)
}

fn hooks() -> ArchHooks {
    ArchHooks {
        switch_to,
        current_slot,
        set_current_slot,
        idle_slot,
        can_run,
        transfer_lock_depth: |_, _| {},
        allow_tick: akuma_cpu::park::irq_window,
        write_user_context,
        read_user_context,
        prepare_task_slot,
    }
}

/// Enter EL0 with `ctx`, never to return: [`akuma_exec::ExecRuntime::enter_user`] for this path.
///
/// The thread's later EL0 traps land at the top of its own kernel stack, which
/// `akuma_vectors_aarch64::enter_user` points `SP_EL1` at; the lock is released first,
/// since EL0 never holds it.
pub fn enter_user(ctx: &UserContext) -> ! {
    let slot = current_slot();
    let top = threading::slot_stack_bounds(slot).map(|(base, size)| (base + size) & !0xF);
    let refuse = ctx.spsr & 0xF != 0;
    if refuse || top.is_none() {
        safe_print!(
            160,
            "[FATAL] hooked enter_user: slot {} spsr={:#x} stack={} - refusing the crossing\n",
            slot,
            ctx.spsr,
            if top.is_some() { "ok" } else { "none" }
        );
        akuma_exec::bkl::leave_kernel();
        loop {
            akuma_cpu::park::wfi();
        }
    }
    let mut f = Frame::zeroed();
    f.x = [
        ctx.x0, ctx.x1, ctx.x2, ctx.x3, ctx.x4, ctx.x5, ctx.x6, ctx.x7, ctx.x8, ctx.x9, ctx.x10, ctx.x11, ctx.x12,
        ctx.x13, ctx.x14, ctx.x15, ctx.x16, ctx.x17, ctx.x18, ctx.x19, ctx.x20, ctx.x21, ctx.x22, ctx.x23, ctx.x24,
        ctx.x25, ctx.x26, ctx.x27, ctx.x28, ctx.x29, ctx.x30,
    ];
    f.sp = ctx.sp;
    f.elr = ctx.pc;
    f.spsr = ctx.spsr;
    f.tpidr = ctx.tpidr;
    akuma_exec::bkl::leave_kernel();
    akuma_vectors_aarch64::enter_user(&f, top.unwrap_or(0) as u64)
}

/// A core's idle thread: let the clock in, run whatever became ready, collect dead slots,
/// else halt.
/// It takes the lock for each piece of scheduler work and halts without it; `wfi` ends on
/// a pending interrupt even while masked, and the window after it takes that interrupt.
/// The longest an idle core halts with its tick stopped: the bound on how late an itimer
/// (which has no deadline the idle loop can read) or a missed wake deadline is served.
const IDLE_HALT_MAX_US: u64 = 10_000;

extern "C" fn idle_main() -> ! {
    loop {
        akuma_exec::bkl::enter_kernel();
        // BKL-hold profiler only (a relaxed load unless `bkl-profile` turned it on).
        akuma_exec::sync::set_holder_tag(akuma_exec::bkl::current_core_id(), akuma_exec::sync::HOLD_TAG_IDLE);
        // The collector, not the x86 reaper: this kernel's process teardown is its cleanup
        // callback, which the reaper does not run (`akuma-threading/collector-recycles`).
        threading::reclaim_terminated_slots();
        let due = threading::wake_is_due(threading::earliest_wake_time(), crate::timer::uptime_us());
        let ran = (due || threading::runnable_exists()) && threading::try_yield();
        akuma_exec::bkl::leave_kernel();
        if !ran && !threading::runnable_exists() {
            // Take the halt out of this slot's CPU bill, as amd64's idle loop does: the
            // switch bills a slot for its whole residency, so without it every second this
            // core slept was `user` time in `/proc/stat` and `top` showed it pegged.
            let entered = crate::timer::uptime_us();
            // Tickless while halted: an idle core's tick does nothing but wake it, and under
            // KVM every wake is an exit, on every core, a thousand times a second at a 1 ms
            // tick. Sleep to the earliest thing that is due instead — a parked thread's
            // timeout, an alarm — capped, because itimers have no deadline to ask for and the
            // cap bounds how late one fires. New work arrives by SGI (`ThreadWaker::wake`,
            // the netpoll doorbell), which ends the halt like any interrupt.
            let mut deadline = entered.saturating_add(IDLE_HALT_MAX_US);
            if let Some(w) = threading::earliest_wake_time() {
                deadline = deadline.min(w);
            }
            if let Some(a) = akuma_exec::alarms::next_deadline() {
                deadline = deadline.min(a);
            }
            crate::timer::arm_idle_deadline(deadline);
            akuma_cpu::park::wfi();
            crate::timer::rearm_tick();
            threading::credit_halted_time(threading::current_thread_id(), entered);
        }
        akuma_cpu::park::irq_window();
    }
}

/// A secondary core's idle thread: its boot context, adopted on its boot stack
/// ([`akuma_entry::smp_shared::HookedSecondary::adopt`]). Unpickable before it is latched
/// running, so no peer can resume it before this core first switches away.
fn adopt_secondary(core: usize, base: akuma_primitives::addr::VirtAddr, size: usize) -> Option<usize> {
    let slot = threading::adopt_core_idle(core, base, size)?;
    mark_idle(core, slot);
    safe_print!(64, "[hooked] core {} online, idle thread is slot {}\n", core, slot);
    Some(slot)
}

fn secondary_idle() -> ! {
    idle_main()
}

/// Make `slot` core `core`'s idle thread.
fn mark_idle(core: usize, slot: usize) {
    IDLE[slot].store(true, Relaxed);
    IDLE_OF_CORE[core % MAX_CORES].store(slot, Relaxed);
}

/// Register the hooked scheduler's table and the free gate's probe, and seed this core's
/// idle thread. After `threading::init` (the slot table) and before any thread but the
/// boot thread runs.
pub fn init() {
    threading::register_arch_hooks(hooks());
    threading::register_saved_root_probe(saved_root_probe);
    set_current_slot(0);
    let Some(idle) = threading::claim_slot() else {
        safe_print!(64, "[FATAL] hooked: no slot for core 0's idle thread\n");
        loop {
            akuma_cpu::park::wfi();
        }
    };
    let Some(top) = threading::ensure_pool_stack_top(idle, crate::config::SYSTEM_THREAD_STACK_SIZE) else {
        safe_print!(64, "[FATAL] hooked: no stack for core 0's idle thread\n");
        loop {
            akuma_cpu::park::wfi();
        }
    };
    mark_idle(0, idle);
    threading::seed_entry(idle, top, idle_main);
    threading::publish_slot(idle);
    akuma_entry::smp_shared::register_hooked_secondary(akuma_entry::smp_shared::HookedSecondary {
        adopt: adopt_secondary,
        idle: secondary_idle,
    });
    safe_print!(96, "[hooked] scheduler hooks registered; core 0 idle thread is slot {}\n", idle);
}
