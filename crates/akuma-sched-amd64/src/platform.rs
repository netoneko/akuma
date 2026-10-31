//! The seam between the shared hook bodies and one kernel.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::{FxArea, Machine, Stacks, XCR0_ENABLED, XSAVE_ON};

/// "Any core": a `pinned` word with no pin.
pub const NO_CPU: u32 = u32::MAX;

/// The low bits of a page-table root are flags and the top bits reserved; a root is reduced
/// to its frame before it is compared with the freed-root ring.
const L0_BASE_MASK: u64 = 0x0000_FFFF_FFFF_F000;

static RESERVED_CORES: AtomicU64 = AtomicU64::new(0);

/// Cores kept free of unmasked, unpinned threads (a VM's cores). The boot core never is.
pub fn reserve_cores(mask: u64) {
    RESERVED_CORES.store(mask & !1, Ordering::Relaxed);
}

/// The reserved-core mask.
#[must_use]
pub fn reserved_cores() -> u64 {
    RESERVED_CORES.load(Ordering::Relaxed)
}

/// Everything a switch needs from the kernel it runs in.
///
/// Required methods are facts about the machine; the provided ones are diagnostics a kernel
/// may wire to its own counters and console (the metal kernel does; the defaults are no-ops).
pub trait Platform: 'static {
    /// The kernel's per-thread user-context record (`UserCtx`).
    type User: 'static;
    /// Bytes in each kernel / trap stack.
    const STACK_SIZE: usize;
    /// Reload `CR3` even when the incoming root equals the active one. A kernel that can prove a root
    /// frame was not freed and reissued (the metal kernel's freed-L0 ring) skips the write; one that
    /// cannot must keep it, because it doubles as the flush a recycled root frame needs.
    const RELOAD_SAME_ROOT: bool = false;

    /// The machine table, indexed by `akuma-threading` thread slot.
    fn machines() -> &'static [Machine<Self::User>];
    /// A fresh user-context record.
    fn new_user() -> Self::User;

    /// This core's index.
    fn cpu_index() -> usize;
    /// The slot this core runs now.
    fn current_task() -> usize;

    /// The kernel's own page-table root: what a thread with `space_root == 0` runs in. Recorded
    /// at init, not re-read at switch time (by then `CR3` holds the *outgoing* thread's).
    fn kernel_root() -> u64;
    /// The root this core has installed now.
    fn active_root() -> u64;
    /// Make `root` this core's page-table root.
    ///
    /// # Safety
    /// `root` must be a live PML4 that maps the kernel half (every address space does).
    unsafe fn activate(root: u64);

    /// Point the TSS `rsp0` at `top`: where a ring-3 trap on this core lands.
    fn set_kernel_stack(top: u64);
    /// Publish `user` as the record the syscall path addresses on this core.
    fn set_current_uctx(user: *mut Self::User);
    /// `(IA32_FS_BASE, user GS base)` for a thread (`slot` is the thread, `user` its record).
    fn user_bases(slot: usize, user: &Self::User) -> (u64, u64);

    /// The page-table root `slot` runs in, given the one its machine record holds (`0` =
    /// the kernel's). A kernel that derives the root from its own task record (so an `exec`
    /// that swaps it needs no second write) overrides this.
    fn space_root_of(_slot: usize, recorded: u64) -> u64 {
        recorded
    }
    /// Where a ring-3 trap on this core lands while `slot` runs, given the machine record's.
    fn trap_top_of(_slot: usize, recorded: u64) -> u64 {
        recorded
    }
    /// Per-kernel machine state the shared body cannot know (a syscall band, a dispatcher's
    /// task record), installed after the common state and before the FP switch.
    fn after_install(_from: usize, _to: usize) {}

    /// Was the page-table root with this frame recently freed? (`akuma-mmu`'s freed-L0 ring.)
    /// A switch refuses to install one.
    fn l0_recently_freed(_l0: u64) -> bool {
        false
    }
    /// Does this core hold the kernel lock? (Diagnostic.)
    fn lock_held() -> bool {
        true
    }

    /// Diagnostic: a switch from `from` to `to` begins.
    fn switch_entry(_from: usize, _to: usize) {}
    /// Diagnostic: the incoming root was freed and was demoted to the kernel root.
    fn freed_root(_root: u64, _from: usize, _to: usize) {}
    /// Diagnostic: `to` has no FS base while `from` had one, and `to` is not idle.
    fn stale_fs_window() {}
    /// Diagnostic: the switch ran without the kernel lock.
    fn switch_without_lock(_from: usize, _to: usize) {}
    /// Diagnostic: canary checks on both stacks, after the machine state is installed.
    fn check_stacks(_from: usize, _to: usize) {}
    /// A slot just got its stacks (first use or recycle): paint canaries, reset counters.
    fn slot_prepared(_slot: usize, _stacks: &Stacks) {}
}

/// `X86ArchHooks::switch_to`: install `to`'s machine state on this core, with `from` still
/// current and before the stack moves.
///
/// The order inside is the contract, and every line of it was a bug once:
///
/// - `CR3` **before** the switch - every address space shares the kernel's upper-half
///   mappings, so the stack and the code stay mapped across the write. Written only when the
///   root actually changes. A root recently freed is refused (demoted to the kernel root).
/// - `FS` restored on every switch (`IA32_FS_BASE` is one per-core register); `GS` too,
///   because the kernel's own `%gs` is the other half of the pair and a stale user value
///   would follow a thread that never set one onto another core.
/// - the FP state of `from` saved before `to`'s is restored.
pub fn switch_to<P: Platform>(from: usize, to: usize) {
    P::switch_entry(from, to);
    let table = P::machines();
    let (from_m, to_m) = (&table[from], &table[to]);

    let mut want = match P::space_root_of(to, to_m.space_root.load(Ordering::Relaxed)) {
        0 => P::kernel_root(),
        root => root,
    };
    if P::l0_recently_freed(want & L0_BASE_MASK) {
        P::freed_root(want, from, to);
        to_m.space_root.store(0, Ordering::Relaxed);
        want = P::kernel_root();
    }
    if P::RELOAD_SAME_ROOT || want != P::active_root() {
        // SAFETY: `want` is the kernel root or a thread's recorded root, both live PML4s
        // sharing the kernel half; a freed root was refused above.
        unsafe { P::activate(want) };
    }

    P::set_kernel_stack(P::trap_top_of(to, to_m.trap_stack_top.load(Ordering::Relaxed)));
    P::set_current_uctx(to_m.uctx.as_ptr());

    let (fs, gs) = to_m.uctx.with(|u| P::user_bases(to, u));
    if fs == 0
        && from_m.uctx.with(|u| P::user_bases(from, u).0) != 0
        && !to_m.idle.load(Ordering::Relaxed)
    {
        P::stale_fs_window();
    }
    let _ = akuma_cpu::msr::set_fs_base(fs);
    let _ = akuma_cpu::msr::set_user_gs_base(gs);

    if !P::lock_held() {
        P::switch_without_lock(from, to);
    }
    P::check_stacks(from, to);
    P::after_install(from, to);

    let fx_out = from_m.fx.as_ptr();
    let fx_in = to_m.fx.as_ptr().cast_const();
    // SAFETY: both areas are 64-byte aligned and sized for the widest format; `to`'s holds
    // `initial` or a prior save; the kernel lock keeps the table single-writer.
    unsafe {
        if XSAVE_ON.load(Ordering::Relaxed) {
            core::arch::asm!(
                "xsave64 [{out}]",
                "xrstor64 [{inp}]",
                out = in(reg) fx_out,
                inp = in(reg) fx_in,
                in("eax") XCR0_ENABLED as u32,
                in("edx") 0u32,
                options(nostack, preserves_flags)
            );
        } else {
            core::arch::asm!(
                "fxsave64 [{out}]",
                "fxrstor64 [{inp}]",
                out = in(reg) fx_out,
                inp = in(reg) fx_in,
                options(nostack, preserves_flags)
            );
        }
    }
}

/// `X86ArchHooks::can_run`: may this core run `slot`? An idle thread is never chosen by the
/// picker (a core goes idle through `idle_slot`); everything else is the pin / affinity /
/// reserved-core decision (`akuma_vm_spec::affinity::can_run`).
pub fn can_run<P: Platform>(slot: usize) -> bool {
    let cpu = P::cpu_index() as u32;
    let m = &P::machines()[slot];
    if m.idle.load(Ordering::Relaxed) {
        return false;
    }
    let pinned = m.pinned.load(Ordering::Relaxed);
    akuma_vm_spec::affinity::can_run(
        (pinned != NO_CPU).then_some(pinned),
        m.affinity.load(Ordering::Relaxed),
        RESERVED_CORES.load(Ordering::Relaxed),
        cpu,
    )
}

/// `X86ArchHooks::prepare_task_slot`: give a freshly claimed slot its kernel and trap stacks
/// and its default machine state, and answer the top of the kernel stack.
///
/// A recycled slot reuses the stacks it already owns. Every field the picker or the switch
/// reads is reset here, so a slot cannot inherit the previous occupant's `space_root`, `pinned`
/// core or user context; `affinity` is inherited from the *creating* thread, as on Linux.
pub fn prepare_task_slot<P: Platform>(slot: usize) -> Option<usize> {
    let table = P::machines();
    let m = table.get(slot)?;
    let stacks = if let Some(stacks) = m.stacks.get() {
        stacks
    } else {
        m.stacks.set(Stacks::leak_new(P::STACK_SIZE));
        m.stacks.get()?
    };
    let stack_top = stacks.kernel_base() + P::STACK_SIZE;
    let trap_top = (stacks.trap_base() + P::STACK_SIZE) & !0xf;
    P::slot_prepared(slot, &stacks);

    m.trap_stack_top.store(trap_top as u64, Ordering::Relaxed);
    m.space_root.store(0, Ordering::Relaxed);
    m.daemon.store(false, Ordering::Relaxed);
    m.idle.store(false, Ordering::Relaxed);
    m.pinned.store(NO_CPU, Ordering::Relaxed);
    let inherited = table.get(P::current_task()).map_or(0, |c| c.affinity.load(Ordering::Relaxed));
    m.affinity.store(inherited, Ordering::Relaxed);
    m.uctx.set(P::new_user());
    m.fx.set(FxArea::initial());
    Some(stack_top)
}
