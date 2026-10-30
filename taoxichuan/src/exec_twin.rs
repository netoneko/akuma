//! The `akuma-exec` / `akuma-threading` runtime twin — taoxichuan's
//! registration of the hook tables the shared scheduler crates read, the twin
//! of `amd64/src/exec_runtime.rs::init` + `amd64/src/sched.rs`'s registration
//! block (plan `docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md` § 1, slice 1).
//!
//! **Written new, not extracted**: this file is taoxichuan's own glue (the
//! composition rule's "the twin's registration glue is taoxichuan's own");
//! every implementation it hands the tables either names a taoxichuan module
//! that already existed (pipes, sockets, the ext2 root, the COM1 console) or
//! is a no-op / stub **with the reason stated** — the
//! `AKUMA_AMD64_STALE_FALSE_HOOKS.md` lesson: a `false` that read like a
//! decision outlived its truth for months on the metal kernel.
//!
//! Like amd64, this target does **not** call `akuma_exec::init` (which would
//! register seven upward surfaces at once — the BKL yield hook, `akuma-mmu`'s
//! scheduler gates, the PMM surviving-mapper, the prefault hook — against
//! subsystems this kernel serves from its own modules). It registers the four
//! tables it needs: `ExecRuntime` + `ExecConfig` through
//! `akuma_exec::runtime::register`, and `ThreadRuntime` + `ThreadConfig` +
//! `ProcessHooks` + `X86ArchHooks` through `akuma_threading`'s register fns.
//!
//! The one-clock rule: [`uptime_us`] is `akuma_lapic_amd64::ticks()` ×
//! `US_PER_TICK_TARGET` — the PIT-calibrated LAPIC tick IS the clock, and
//! both tables get the same function so the scheduler and `akuma-exec`
//! cannot disagree about what time it is (amd64's reason, verbatim).
//!
//! Slot space, until `fork_process` (slice 3) unifies it with
//! `akuma-threading`'s: the spawn ring (`child.rs`'s `SpawnSlot` indices) —
//! [`current_slot`] names the child running now, or [`NO_SLOT`] for the boot
//! task (sshd). It is a stub **shaped as `fn()`** over a single one-core
//! static: the owner's decision is SMP-ready (slice 5), so the SMP slice
//! replaces these two bodies (with the `%gs` per-CPU block), never the
//! callers — which is why every "who am I" read in `child.rs` goes through
//! them and no new `static` current-task is added anywhere else.

use akuma_exec::{ExecConfig, ExecRuntime};
use akuma_threading::{ProcessHooks, ThreadConfig, ThreadRuntime, X86ArchHooks};
// `read_file` on the ext2 root is the `akuma_vfs::Filesystem` trait method —
// child.rs imports the trait for the same reason.
use akuma_vfs::Filesystem as _;

/// The slot sentinel for "the boot task (sshd) is running": no spawn slot.
/// Outside the spawn ring's indices by construction, so a child slot can
/// never collide with it.
pub const NO_SLOT: usize = usize::MAX;

/// The one sanctioned "which slot is running" static on this kernel. Every
/// other per-task value lives in `child.rs`'s per-slot table, indexed
/// **through here**.
static mut CURRENT: usize = NO_SLOT;

/// The spawn-ring slot of the child running now, or [`NO_SLOT`] for the boot
/// task. One-core stub; the SMP slice puts `%gs` behind this body.
#[must_use]
pub fn current_slot() -> usize {
    // SAFETY: single core, IF clear (syscall path or boot) — the discipline
    // that owns this module's static, as `child.rs`'s.
    unsafe { core::ptr::read_volatile(&raw const CURRENT) }
}

/// Record the slot running now. Called on every crossing in and out.
pub fn set_current_slot(slot: usize) {
    // SAFETY: as `current_slot`.
    unsafe {
        *core::ptr::addr_of_mut!(CURRENT) = slot;
    }
}

/// The threading tid the CURRENT M2 shell is registered under. The shell's
/// tid is its spawn-ring slot (child.rs registers with `tid = slot_idx`), so
/// this is written at every SPAWN crossing — the M2 model has one live shell
/// at a time. The switch_to doc's older "tid 0 is the shell" held only for a
/// boot whose first login spawn landed on slot 0; the sched_* maps key on
/// this recorded tid now (session pe boot D/E: the wake switched the shell
/// back under `sched_root_of(2)` = a stale fork-child record, and the shell
/// resumed on an exec-swept address space whose stack pages read zero).
static mut SHELL_THREAD_TID: usize = 0;

/// The spawn crossing names the shell's threading tid. SAFETY: syscall-path
/// discipline (single core, IF clear), as `CURRENT`.
pub fn set_shell_tid(tid: usize) {
    unsafe {
        *core::ptr::addr_of_mut!(SHELL_THREAD_TID) = tid;
    }
}

/// SAFETY: as [`set_shell_tid`].
#[must_use]
pub fn shell_tid() -> usize {
    unsafe { core::ptr::read_volatile(&raw const SHELL_THREAD_TID) }
}

/// The twin's clock: ticks × 10 ms. `saturating_mul` so a ~584-million-year
/// uptime wraps to "forever" instead of truncating into the past.
#[must_use]
pub fn uptime_us() -> u64 {
    akuma_lapic_amd64::ticks().saturating_mul(u64::from(akuma_lapic_amd64::US_PER_TICK_TARGET))
}

// ── the X86ArchHooks bodies (the per-core machine effects) ──────────────────

/// `X86ArchHooks::switch_to` — **real (slice 3)**: the machine effects of a
/// park-point switch. `akuma-threading`'s `x86_yield_now` has already named
/// `next` current and will move the stacks itself (`x86_switch_context` is
/// the crate's own x86 asm); what the machine still needs from the twin is
/// the incoming task's syscall band and its CR3. The shell is threading tid
/// `shell_tid()` — its spawn-ring slot, written at every SPAWN crossing (it
/// was once hardcoded 0, which only held for a boot's first login spawn) —
/// with its SPAWN crossing band and task record 1; 1..NSLOTS real children
/// (leaked pairs, task record tid+1); anything else the idle thread (stack
/// below, CR3 untouched — it runs on the kernel root). `from` needs nothing:
/// the crate saves its context.
fn switch_to(from: usize, to: usize) {
    fx_save(from);
    // (session pg: the per-switch `[tmp] sw to=` print lived here — at
    // preemption scale it rotated the console log every ~30 s and ate the
    // gate verdicts; removed before the first gate boot per PLAN-pg §5.)
    let top = crate::child::sched_kstack_top(to);
    akuma_ring3_amd64::set_syscall_stack_top(top);
    // Session pg fix (the exec'd shell's writev loop, boot pg-2's
    // l0=0 l1=0 br=11 dlen=0 ans=0): the dispatcher's `task()` is
    // `TASKS[TASK_SLOT]` — every syscall's fd table, root_pa and regions
    // source. Crossings set TASK_SLOT (`twin_enter_user`), but a park-resume
    // of ANOTHER task followed by a tick switch back left it naming the
    // resumer's record: the fork child then ran with another task's root_pa,
    // its writev gather read zeros out of the wrong address space, and musl's
    // `__stdio_write` spun forever on the 0 answer. Keep it in step with the
    // CR3 loaded below — the same discipline the sched maps already follow
    // (session pe boot D/E: the shell resumed under a stale record whose
    // stack pages read zero). The idle thread (tid >= NSLOTS) runs kernel
    // code only and leaves the previous task's slot armed, exactly as it
    // leaves CR3.
    if to < crate::child::NSLOTS {
        crate::set_task_slot(crate::child::sched_task_slot_of(to));
    }
    // Session pf preemption: the incoming task's own ring-3 trap stack — a
    // preempting switch leaves the interrupted frame on the interrupted task's
    // stack, so the next ring-3 trap on any task must not land there.
    crate::arm_trap_stack(to);
    let root = crate::child::sched_root_of(to);
    if root != 0 {
        // SAFETY: `root` is a PMM-resident page-table frame naming the
        // incoming task's image and the shared kernel half — the same load
        // every crossing here performs. Single core, IF clear, no TLB
        // aliasing to flush (PCID unused, CR3 writes flush non-global).
        unsafe { core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags)) };
    }
    // Session pi: restore the incoming task's own fs. The MSR is per-core
    // and every switch used to leave it holding the OUTGOING side's (or any
    // earlier task's) TLS pointer — the parked-resume defect that only
    // grandfork (the first image with its own TLS) could surface. One wrmsr
    // per switch, incoming slot's shadow; the idle thread (to >= NSLOTS)
    // runs kernel code only and is skipped.
    let _ = akuma_cpu::msr::set_fs_base(crate::child::slot_fs_of(to));
    fx_restore(to);
}

/// One task's `fxsave` image: x87/MMX/SSE state. The kernel is soft-float, so
/// at a switch the registers still hold the OUTGOING task's user values, and
/// without this every preemption or park handed them to whichever task ran
/// next - LLVM's vector code in rustc's codegen threads then computed garbage
/// (an out-of-range index that spelled "loss", a `free()` of a misaligned
/// pointer). XSAVE/AVX is never enabled here, so the legacy area is the whole
/// state (`docs/archive/AMD64_SMP_BRINGUP.md` has the amd64 twin of this).
#[repr(C, align(16))]
struct FxArea([u8; 512]);

/// The power-on x87/SSE state: `FCW = 0x037F`, `MXCSR = 0x1F80` (every SSE
/// exception masked). All-zero would unmask them and the first `divss` traps.
const fn fx_init() -> FxArea {
    let mut a = [0u8; 512];
    a[0] = 0x7F;
    a[1] = 0x03;
    a[24] = 0x80;
    a[25] = 0x1F;
    FxArea(a)
}

/// One area per ring slot plus the idle thread (`IDLE_TID`). SAFETY: single
/// core, IF clear at every switch; only ever touched through raw pointers.
static mut FX: [FxArea; crate::child::NSLOTS + 1] = [const { fx_init() }; crate::child::NSLOTS + 1];

fn fx_save(slot: usize) {
    if slot <= crate::child::NSLOTS {
        // SAFETY: `FX[slot]` is a 16-byte-aligned 512-byte area; `fxsave64`
        // writes exactly that and touches no flags.
        unsafe {
            core::arch::asm!("fxsave64 [{}]", in(reg) (&raw mut FX[slot]).cast::<u8>(), options(nostack, preserves_flags));
        }
    }
}

fn fx_restore(slot: usize) {
    if slot <= crate::child::NSLOTS {
        // SAFETY: as `fx_save`; every area holds either `fx_init()` or an
        // image `fxsave64` wrote, both valid `fxrstor64` inputs.
        unsafe {
            core::arch::asm!("fxrstor64 [{}]", in(reg) (&raw const FX[slot]).cast::<u8>(), options(nostack, preserves_flags, readonly));
        }
    }
}

/// A slot is being given to a new task: it starts from the power-on state, not
/// the previous occupant's registers.
pub fn fx_reset(slot: usize) {
    if slot <= crate::child::NSLOTS {
        // SAFETY: single core, IF clear (the claim path); the slot is not running.
        unsafe { (&raw mut FX[slot]).write(fx_init()) };
    }
}

/// The idle thread's tid: outside the spawn ring (children claim 1..NSLOTS,
/// the shell is 0), so no array index collides. Its stack is leaked once,
/// below; its CR3 is the kernel's (sched_root_of answers 0).
pub const IDLE_TID: usize = crate::child::NSLOTS;

static IDLE_STACK: spin_idle::Once<U64Arr> = spin_idle::Once::NEW;
type U64Arr = alloc::boxed::Box<[u64; 2048]>;
mod spin_idle {
    //! One-shot init for the idle stack — `Once` without a lock dependency
    //! (single core, IF clear at every caller).
    pub struct Once<T>(core::cell::UnsafeCell<Option<T>>);
    impl<T> Once<T> {
        #[allow(clippy::declare_interior_mutable_const)] // a const only to initialise `static`s
        pub const NEW: Self = Self(core::cell::UnsafeCell::new(None));
        /// SAFETY: single core, IF clear (the registration path).
        pub fn call(&self, make: impl FnOnce() -> T) -> *mut T {
            unsafe {
                let p = &mut *self.0.get();
                if p.is_none() {
                    *p = Some(make());
                }
                p.as_mut().unwrap_unchecked()
            }
        }
    }
    // SAFETY: only touched from the single-core IF-clear path.
    unsafe impl<T> Sync for Once<T> {}
}

extern "C" fn idle_main() -> ! {
    // The idle loop: keep the tick alive (one `sti` window per lap — the
    // clock is what timed parks and uptime read) and yield the moment
    // anything becomes READY. Without the yield, a woken task would wait
    // for... nothing: the waker only readies, it cannot switch.
    // Session pj (pi's HI1, closed at the contract level): when NOTHING is
    // runnable, HALT — `net::wait_lap` (sti;hlt;cli), the same lap every
    // parked waiter has used since ph. This is the one idle-contract clause
    // the other kernels all have and this kernel lacked (amd64
    // machine.rs:517 "A halted processor wakes when something can be offered
    // to it"; kernel-glue idle_halt: without it the idle thread pins the
    // host vCPU at 100%). The relay thread shares this vCPU's ONE pinned
    // host core, and only a halted vCPU ever hands that core back — a
    // spinning idle lap's netpoll cannot receive what the starved relay
    // cannot run to deliver. The 100 Hz tick wakes the halt; the yield
    // check stays after the lap.
    #[cfg(feature = "trace-err")]
    let mut idle_laps = 0u32;
    loop {
        // The NIC pump: one poll per lap (per tick-wake now). The idle lap
        // is the everything-path's last resort — an all-parked system used
        // to leave the NIC unposted and the relay deaf.
        crate::net::netpoll();
        crate::exec_twin::tick_window();
        if akuma_threading::x86_runnable_exists() {
            #[cfg(feature = "trace-err")]
            {
                idle_laps = 0;
            }
            akuma_threading::x86_yield();
        } else {
            crate::net::wait_lap();
            #[cfg(feature = "trace-err")]
            {
                idle_laps += 1;
                if idle_laps == 2000 {
                    crate::trace_hang_dump(4);
                }
            }
        }
    }
}

/// Seed the idle thread once (from `init`): claim nothing — the tid is ours
/// by const — seed the entry, publish READY.
fn seed_idle() {
    // SAFETY: single core, IF clear — the registration path owns the Once.
    let stack: &'static mut [u64; 2048] =
        unsafe { &mut *IDLE_STACK.call(|| alloc::boxed::Box::new([0u64; 2048])) };
    let top = ((core::ptr::from_mut(stack) as *const u64 as usize) + 2048 * 8) & !0xf;
    akuma_threading::x86_seed_entry(IDLE_TID, top, idle_main);
    akuma_threading::x86_publish(IDLE_TID);
}

/// `X86ArchHooks::allow_tick` — the frozen-clock escape, real: open a
/// one-instruction interrupt window, then mask again. Meaningless without a
/// tick; this kernel has one now (session pb). Nothing calls it until
/// blocking syscalls arrive (slice 3) — a kernel that runs IF=0 everywhere
/// already gets its ticks in ring 3.
/// The idle loop's clock feeder — the same one-instruction window
/// [`allow_tick`] opens, under a name the idle thread can reach.
pub fn tick_window() {
    // SAFETY: `sti`/`cli` at ring 0 touch no memory or stack; the window is
    // one instruction wide and the tick handler is bounded, so the worst a
    // tick in the window does is count.
    unsafe {
        core::arch::asm!("sti", "nop", "cli", options(nomem, nostack, preserves_flags));
    }
}

fn allow_tick() {    // SAFETY: `sti`/`cli` are unconditionally executable at ring 0 and touch
    // no memory or stack; the window is one instruction wide and the tick
    // handler (`timer_trap`) is bounded and allocation-free, so the worst a
    // tick in the window does is count.
    unsafe {
        core::arch::asm!(
            "sti",
            "nop",
            "cli",
            options(nomem, nostack, preserves_flags)
        );
    }
}

// ── registration ────────────────────────────────────────────────────────────

/// Register all four tables. Called once from `kernel_main`, right after
/// `lapic_init` (the tick must exist before it can be anyone's clock; PMM and
/// the heap must be up before anything here can allocate). `Registered` is
/// once-only, so an accidental second call would be silent — there is none.
pub fn init() {
    // ExecRuntime first: its registration also points `akuma_primitives`'
    // shared `safe_print!` sink and clock hook at this kernel (the console
    // hook is the same `console::puts` `kernel_main` already set, so the
    // print side is a no-op there; the clock hook switches the primitives'
    // diagnostic timestamps to the tick clock — taoxichuan's own time
    // syscalls keep reading `net::uptime_us` explicitly, so nothing else
    // moves).
    akuma_exec::runtime::register(exec_runtime(), exec_config());

    // The scheduler's platform table + tuning knobs. The four IPI/IRQ-bound
    // fields are no-ops with amd64's reasons: this x86 scheduler is (will be)
    // a call, not an ISR, and there is one core.
    akuma_threading::register(thread_runtime(), thread_config());

    // The process layer's eight answers. `pid_for_thread`/`find_pid_by_thread`
    // resolve over the spawn ring today (the only pid table this kernel has);
    // the rest are no-ops for subsystems that do not exist here — each with
    // its reason, not a bare `false`.
    akuma_threading::register_process_hooks(process_hooks());

    // The machine effects. `switch_to` is real from slice 3; the per-slot
    // state arms landed in slice 2. The idle thread is seeded last: READY
    // from the first registration, it only ever gets the core when every
    // real task is parked (its loop feeds the tick and yields on wakeups).
    akuma_threading::register_x86_arch_hooks(X86ArchHooks {
        switch_to,
        current_slot,
        set_current_slot,
        idle_slot,
        can_run,
        transfer_lock_depth,
        allow_tick,
        write_user_context: crate::child::write_user_context,
        read_user_context: crate::child::read_user_context,
        prepare_task_slot: crate::child::prepare_task_slot,
    });
    seed_idle();
    akuma_exec::process::reserve_pid_floor(2);
}

/// The spawn ring's width, for `can_run`'s bounds check.
/// The idle answer: the boot task (`NO_SLOT` in the ring's naming). Nothing
/// can call this until a picker exists (slice 3).
fn idle_slot() -> usize {
    IDLE_TID
}

/// Pinning: nothing pins yet, so any slot in the ring (or the boot task) can
/// run. `can_run` returns `false` only where a real pinning table would.
fn can_run(slot: usize) -> bool {
    slot == NO_SLOT || slot <= crate::child::NSLOTS
}

/// The BKL's recursion depth is per-thread and the lock per-core; this
/// kernel takes no BKL yet (no threads), so there is no depth to move.
fn transfer_lock_depth(_from: usize, _to: usize) {}

fn thread_runtime() -> ThreadRuntime {
    ThreadRuntime {
        uptime_us,
        trigger_sgi: |_| {},
        wake_core: |_| {},
        wake_remote_idle: || false,
        // ThreadRuntime's own EOI is dead on x86 (amd64's reason: the real
        // EOI lives in ExecRuntime, which has it below).
        end_of_interrupt: |_| {},
        print_str: akuma_comconsole::puts,
    }
}

fn thread_config() -> ThreadConfig {
    // amd64's `config()` values where they are machine facts, this kernel's
    // where they are ours: the syscall-crossing stack band is
    // `ring3::SYSCALL_STACK_SIZE` (64 KiB, the T-Y bug-3 number), and the
    // boot-stack bounds stay 0 — "no slot-0 stack bounds", which disables
    // canary placement rather than stamping a canary into whatever lives
    // there (docs/LOW_MEMORY_ENVIRONMENT.md "Known bug").
    ThreadConfig {
        reserved_threads: 0,
        kernel_stack_size: akuma_ring3_amd64::SYSCALL_STACK_SIZE,
        system_thread_stack_size: akuma_ring3_amd64::SYSCALL_STACK_SIZE,
        user_thread_stack_size: akuma_ring3_amd64::SYSCALL_STACK_SIZE,
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
    }
}

fn process_hooks() -> ProcessHooks {
    ProcessHooks {
        // No retired-process drain exists on this target (no process table to
        // retire from) — the twin's honest emptiness, to be revisited with the
        // slot swap.
        clear_draining: |_| {},
        drain_in_flight: |_| false,
        lifecycle_trace_on: || false,
        pid_for_thread: crate::child::pid_for_slot,
        find_pid_by_thread: crate::child::pid_for_slot,
        // No signals — out of scope through slice 3.
        is_current_interrupted: || false,
        proc_dump_info: |_| None,
        dump_orphan_processes: || {},
    }
}

fn exec_runtime() -> ExecRuntime {
    ExecRuntime {
        // ── real ──────────────────────────────────────────────────────────
        uptime_us,
        // IF discipline is already this kernel's rule (the syscall
        // instruction masks; nothing in the kernel unmasks), so these are
        // the honest `cli`/`sti` pair, same shape as amd64's `daif` calls.
        disable_irqs: akuma_cpu::daif::mask_irq,
        enable_irqs: akuma_cpu::daif::unmask_irq,
        end_of_interrupt: |_vector| akuma_lapic_amd64::eoi(),
        heap_stats: || {
            let s = akuma_alloc::stats();
            (s.heap_size, s.allocated)
        },
        is_memory_low: akuma_alloc::is_memory_low,
        print_str: akuma_comconsole::puts,
        // The ext2 root, read through this kernel's own paths. `resolve_inode`
        // + `read_at_by_inode` are the same bytes ring 3 already gets.
        read_file: |path| crate::root_fs().read_file(path).map_err(|_| -1),
        read_at: |path, off, buf| {
            crate::root_fs()
                .resolve_inode(path)
                .and_then(|ino| crate::root_fs().read_at_by_inode(ino, off, buf))
                .map_err(|_| -1)
        },
        read_at_by_inode: |_path, ino, off, buf| {
            crate::root_fs().read_at_by_inode(ino, off, buf).map_err(|_| -1)
        },
        // Symlinks: this target resolves inside its own path walk
        // (`resolve_effective`), so the resolved answer for a path IS the
        // resolved path — identity-with-a-resolver, the same stance amd64's
        // comment takes for its `fd.rs`.
        resolve_symlinks: crate::resolve_effective,
        // The pipes: M3's table already exists, and its two lifecycle arms
        // plus the kernel-memory write are exactly what these hooks ask for.
        pipe_close_write: |id| crate::pipes::release(id, true),
        pipe_close_read: |id| crate::pipes::release(id, false),
        pipe_clone_ref: crate::pipes::clone_ref,
        pipe_write: crate::pipes::write_no_sigpipe,
        // The AF_INET socket table is real (stage 4); the fd-table mirror
        // these two serve (`SharedFdTable`) is not built here, but the arms
        // are the same functions this kernel's own close/fork paths run.
        remove_socket: akuma_net::socket::remove_socket,
        socket_clone_ref: akuma_net::socket::socket_clone_ref,
        // A plain user-memory store: no SMAP on this guest, `copy_to_user`
        // already bounds-checks.
        write_user_tid: |va, tid| crate::copy_to_user(va as u64, &tid.to_le_bytes()),
        // The "no ProcessInfo page" answer, verbatim amd64's: identity
        // resolves elsewhere, and `0` tells the shared fork path so.
        alloc_process_info: |_space| Ok(0),

        // ── no-ops with a reason (subsystems this kernel does not build) ──
        // One core: the wake IPIs have no core to ring (kept identical to
        // ThreadRuntime's three, per amd64's "two tables disagreeing" note).
        trigger_sgi: |_| {},
        wake_core: |_| {},
        wake_remote_idle: || false,
        // No smp-shared execve BKL-drop on a one-core kernel.
        exec_bkl_drop_enabled: || false,
        // No process table: nothing `akuma-exec` knows about can exit.
        on_process_exit: |_pid| {},
        // No itimers, futexes, eventfds-in-tables, AF_UNIX, epoll, pidfds,
        // audio, ptys or flock on this target yet — each arm's subsystem has
        // no table here to release from, and the first one that arrives wires
        // its row (amd64 wired each exactly when its syscall landed).
        check_itimers: || {},
        futex_wake: |_, _, _| {},
        eventfd_close: |_| {},
        eventfd_clone_ref: |_| {},
        unix_sock_close: |_| {},
        unix_sock_clone_ref: |_| {},
        epoll_destroy: |_| {},
        pidfd_close: |_| {},
        dsp_close: || {},
        pty_clone_ref: |_, _| {},
        pty_close: |_, _| {},
        flock_release: |_, _, _| {},
        // No box/container namespace registry; `None` is what AArch64 answers
        // with the feature off.
        get_box_namespace: |_| None,
        set_spawn_namespace: |_| {},
        clear_spawn_namespace: || {},
        // No rump boxes on this target.
        rump_socket_clone_ref: |_, _| {},
        // No mount table yet: no `(mount id, inode)` pair to give. A stub, as
        // the plan's table says — reached only by `ElfSource::Path` (slice 3).
        resolve_file_id: |_path| Err(-1),
        // stat sizes live in this kernel's own stat arms, not behind a
        // one-path helper; unwired until a consumer exists (slice 3).
        file_size: |_path| Err("taoxichuan: file_size not wired (unreachable until fork_process)"),

        // ── slice 3 arms (session pd) — the fork seam, wired ───────────────
        // The child's ring-3 entry: the bind table names the CR3, the
        // captured UserContext becomes an EntryFrame, `ring3::enter_user`
        // sysrets. Never returns (the child lives in ring 3; its syscalls
        // come back through the dispatcher).
        enter_user: twin_enter_user,
        // The fork memory pass: the twin's proven eager-copy walk, INTO the
        // child's fresh `UserAddressSpace` root.
        fork_share_memory: twin_fork_share_memory,
        // The child's CR3 + spawn-row binding — the twin's slot binding.
        bind_child_task: |tid, child, kind| twin_bind_child_task(tid, child, kind),
    }
}

/// `ExecRuntime::fork_share_memory` — the twin's eager copy, INTO the child
/// address space `fork_process` step 1 built. The parent's live root (the
/// shell's, through its shared view) is walked leaf-by-leaf by the twin's
/// own proven cloner; the child's fresh root gets the kernel half and the
/// cloned user subtrees. Ledger note: the copied frames are the twin's, not
/// the crate ledger's — a child `Process::drop` frees only what it tracked.
/// Bounded by the gate's fork counts; named in RESULT-pd.md.
fn twin_fork_share_memory(
    parent: &akuma_exec::process::Process,
    child: &mut akuma_exec::process::Process,
) -> Result<(), &'static str> {
    let src = parent.address_space.l0_phys() as u64;
    let dst = child.address_space.get_mut().l0_phys() as u64;
    crate::clone_user_half(src, dst).ok_or("taoxichuan: user-half clone failed")
}

/// `ExecRuntime::bind_child_task` — the twin's spawn-row: which pid, which
/// root, which spawn-ring slot. The crate publishes the thread next.
fn twin_bind_child_task(
    tid: usize,
    child: &akuma_exec::process::Process,
    kind: akuma_exec::process::ChildKind,
) -> Result<(), &'static str> {
    let root = child.address_space.l0_phys() as u64;
    crate::child::bind_child_task(
        tid,
        child.pid,
        root,
        kind == akuma_exec::process::ChildKind::Thread,
    )
}

/// `ExecRuntime::enter_user` — the child's first and only crossing into ring
/// 3. The bind table names the CR3 (loaded here: sysret does not touch it);
/// the captured context becomes an EntryFrame; `ring3::enter_user` sysrets
/// and never returns. The FS base rides the MSR — the child inherits the
/// parent's TLS pointer in hardware, fork semantics.
fn twin_enter_user(ctx: &akuma_exec::process::UserContext) -> ! {
    let cur = crate::exec_twin::current_slot();
    // (session pg: `[tmp] enter` print removed with the sw-to print — same
    // pre-gate-boot readability pass, PLAN-pg §5.)
    // The task record (not the bind table) is the root's authority — a real
    // child that has exec'd must enter on the NEW image's root.
    if cur != crate::exec_twin::NO_SLOT && cur < crate::child::NSLOTS + 1 {
        crate::set_task_slot(crate::child::sched_task_slot_of(cur));
    }
    let root = crate::child::sched_root_of(cur);
    if root != 0 {
        // SAFETY: `root` is a PMM-resident page-table frame naming the
        // child's image and the shared kernel half; the load is the same one
        // `child_enter` performs at every crossing.
        unsafe {
            core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
        };
    }
    let ef = akuma_ring3_amd64::EntryFrame {
        r15: ctx.regs[11],
        r14: ctx.regs[10],
        r13: ctx.regs[9],
        r12: ctx.regs[8],
        rbp: ctx.regs[7],
        rbx: ctx.regs[6],
        r10: ctx.regs[3],
        r8: ctx.regs[4],
        r9: ctx.regs[5],
        rdx: ctx.regs[2],
        rsi: ctx.regs[1],
        rdi: ctx.regs[0],
        rax: ctx.rax,
        rip: ctx.pc,
        rflags: 0x202,
        rsp: ctx.sp,
    };
    // SAFETY: ring3's contract — an enterable ring-3 state, kernel IF clear,
    // CR3 already the child's. `enter_user` is `!`: the child lives in ring 3.
    // Session pi: install the CAPTURED fs before entry. This crossing used to
    // leave IA32_FS_BASE holding the last-ran task's TLS pointer — the fork
    // "inheritance" was whatever the one core's MSR happened to hold, which
    // is also the shape every not-via-child_enter entry got (boot pi-2's PK2
    // crash carried the shell's 0x713198 into a fresh CR3 with NO fs wrmsr
    // exit in its whole window). A captured 0 (an exec'd image's record) is
    // the clean start the amd64 kernel hands its images; a captured parent
    // value is the real-fork inheritance (capture_user_context reads the
    // live MSR in the parent's context). One MSR write per Process entry —
    // entries are per fork/spawn, never per tick.
    let _ = akuma_cpu::msr::set_fs_base(ctx.fs_base);
    crate::child::set_slot_fs(cur, ctx.fs_base);
    unsafe { akuma_ring3_amd64::enter_user(&ef) }
}

fn exec_config() -> ExecConfig {
    // Mirrors amd64's `config()` where the fact is the machine's, this
    // kernel's numbers where they are ours. Nothing reads these today (no
    // shared syscalls run); the table exists so the day one does, the values
    // are already this kernel's truth.
    ExecConfig {
        max_threads: 16, // the spawn ring + the idle thread + headroom
        reserved_threads: 0,
        kernel_stack_size: akuma_ring3_amd64::SYSCALL_STACK_SIZE,
        boot_stack_base: 0,
        boot_stack_top: 0,
        default_thread_stack_size: akuma_ring3_amd64::SYSCALL_STACK_SIZE,
        system_thread_stack_size: akuma_ring3_amd64::SYSCALL_STACK_SIZE,
        user_thread_stack_size: akuma_ring3_amd64::SYSCALL_STACK_SIZE,
        user_stack_size: akuma_ring3_amd64::SYSCALL_STACK_SIZE,
        enable_stack_canaries: false,
        stack_canary: 0,
        canary_words: 0,
        network_thread_ratio: 0,
        prioritize_never_scheduled: false,
        deferred_thread_cleanup: false,
        thread_cleanup_cooldown_us: 0,
        process_reclaim_cooldown_us: 0,
        syscall_debug_info_enabled: false,
        fork_brk_serial_progress: false,
        enable_sgi_debug_prints: false,
        // The spawn slot's 1 MiB stdout cap IS this kernel's proc-stdout
        // bound (child.rs OUT_CAP); there is no stdin pump.
        proc_stdin_max_size: 0,
        proc_stdout_max_size: 1024 * 1024,
        cow_fork_enabled: false,  // the M2 fork is an eager copy, no CoW
        vfork_fastpath_enabled: false,
        pthread_kill_eintr_enabled: false, // no signals
        shared_file_pages_enabled: false,  // no file-page cache
    }
}
