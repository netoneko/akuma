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
//! The one-clock rule: both tables get `crate::net::uptime_us` (the
//! calibrated TSC), so the scheduler, `akuma-exec`, `akuma_primitives::clock`
//! (glue's time syscalls), futex deadlines and smoltcp cannot disagree about
//! what time it is (amd64's reason, verbatim). Until 2026-11-01 these tables
//! read the LAPIC tick (10 ms steps) while the time syscalls read the TSC;
//! one clock is what let those syscalls move to glue.
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
use akuma_threading::{ProcessHooks, ThreadConfig, ThreadRuntime};

/// The slot sentinel for "the boot task (sshd) is running": no spawn slot.
/// Outside the spawn ring's indices by construction, so a child slot can
/// never collide with it.
pub const NO_SLOT: usize = usize::MAX;

/// The spawn-ring slot of the task THIS core runs, or [`NO_SLOT`] for the boot
/// task: the per-CPU block's `slot` word (`gs:[32]`, `akuma-ring3-amd64`), one
/// per core. Every other per-task value lives in `child.rs`'s per-slot table,
/// indexed **through here**.
#[must_use]
pub fn current_slot() -> usize {
    akuma_ring3_amd64::cpu_slot() as usize
}

/// Record the slot THIS core runs now: on every switch-in, and at boot.
pub fn set_current_slot(slot: usize) {
    akuma_ring3_amd64::set_cpu_slot(slot as u64);
}

// ── the X86ArchHooks bodies (the per-core machine effects) ──────────────────

/// The idle thread's tid: outside the spawn ring (children claim 1..NSLOTS,
/// the boot thread is 0), so no array index collides. Its stack is leaked once,
/// below; its CR3 is the kernel's (sched_root_of answers 0).
pub const IDLE_TID: usize = crate::child::NSLOTS;

/// Core `cpu`'s idle thread: `IDLE_TID + cpu`, all above the spawn ring, so no
/// per-slot array indexed by a ring slot collides (the `sched_*_of` maps
/// already treat every tid >= NSLOTS as "kernel-only idle").
#[must_use]
pub const fn idle_tid(cpu: usize) -> usize {
    IDLE_TID + cpu
}

pub extern "C" fn idle_main() -> ! {
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
        crate::smp::bkl_enter();
        crate::net::netpoll();
        crate::smp::bkl_leave();
        crate::exec_twin::tick_window();
        // A sleeper whose deadline has passed is still WAITING: only the wake
        // pass inside `x86_yield` readies it. So a due deadline is a reason to
        // yield too, or a timed park (glue's `nanosleep`, a `poll`/`epoll`
        // timeout) on an otherwise idle core never expires. amd64's wait
        // loops ask the same question (`amd64/src/net.rs`).
        let due = akuma_threading::wake_is_due(akuma_threading::x86_earliest_wake_time(), crate::net::uptime_us());
        if due || akuma_threading::x86_runnable_exists() {
            #[cfg(feature = "trace-err")]
            {
                idle_laps = 0;
            }
            // The pick and the switch hooks read the kernel's shared slot and
            // task tables: they run under the kernel lock (the switch hands
            // the depth to whichever thread runs next, and back).
            crate::smp::bkl_enter();
            akuma_threading::x86_yield();
            crate::smp::bkl_leave();
        } else {
            crate::net::wait_lap();
            #[cfg(feature = "trace-err")]
            {
                idle_laps += 1;
                // One core, under the kernel lock: the dump borrows the syscall ring and the in-flight table,
                // and every idle core reaches this lap count at about the same time.
                if idle_laps == 2000 && akuma_ring3_amd64::cpu_index() == 0 {
                    crate::smp::bkl_enter();
                    crate::trace_hang_dump(4);
                    crate::smp::bkl_leave();
                }
            }
        }
    }
}

/// Seed the idle thread once (from `init`): claim nothing — the tid is ours
/// by const — seed the entry, publish READY.
fn seed_idle() {
    // Called once from `init`; the stack is leaked for the life of the kernel.
    let stack: &'static mut [u64; 2048] = alloc::boxed::Box::leak(alloc::boxed::Box::new([0u64; 2048]));
    let top = ((core::ptr::from_mut(stack) as *const u64 as usize) + 2048 * 8) & !0xf;
    crate::sched::mark_idle(IDLE_TID, 0);
    akuma_threading::x86_seed_entry(IDLE_TID, top, idle_main);
    akuma_threading::x86_publish(IDLE_TID);
}

/// The idle loop's clock feeder — the same one-instruction window
/// `X86ArchHooks::allow_tick` opens (`sched::Tx`), under a name the idle
/// thread can reach.
pub fn tick_window() {
    // One instruction wide, and the tick handler is bounded, so the worst a
    // tick in the window does is count.
    akuma_cpu::park::irq_window();
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
    // print side is a no-op there; the clock hook is the TSC, which glue's
    // time syscalls read).
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
    akuma_threading::register_x86_arch_hooks(akuma_sched_amd64::hooks::<crate::sched::Tx>());
    seed_idle();
    // Glue's arms validate a user buffer before touching it and, when a page is
    // not present, ask this hook to fill it: without it a lazy `mmap` buffer is
    // `EFAULT` to every shared syscall (`lazy.rs`).
    akuma_user_access::set_prefault_hook(crate::lazy::prefault);
    akuma_exec::process::reserve_pid_floor(2);
}

fn thread_runtime() -> ThreadRuntime {
    ThreadRuntime {
        uptime_us: crate::net::uptime_us,
        trigger_sgi: |_| crate::smp::kick_halted_core(),
        wake_core: |_| {},
        wake_remote_idle: || false,
        // ThreadRuntime's own EOI is dead on x86 (amd64's reason: the real
        // EOI lives in ExecRuntime, which has it below).
        end_of_interrupt: |_| {},
        print_str: crate::console::puts,
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
        uptime_us: crate::net::uptime_us,
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
        print_str: crate::console::puts,
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
        // The descriptor table is `akuma-exec`'s, and every table behind it is
        // `akuma-syscalls-glue`'s (TAOXICHUAN_CLEANUP_PLAN.md step 2b): one id
        // space per kind, released when the last descriptor naming it drops.
        pipe_close_write: akuma_syscalls_glue::pipe::pipe_close_write,
        pipe_close_read: akuma_syscalls_glue::pipe::pipe_close_read,
        pipe_clone_ref: akuma_syscalls_glue::pipe::pipe_clone_ref,
        pipe_write: akuma_syscalls_glue::pipe::pipe_write_no_sigpipe,
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
        // No itimers, audio or ptys on this target yet — each arm's
        // subsystem has no table here to release from, and the first one that
        // arrives wires its row (amd64 wired each exactly when its syscall
        // landed). The futex table is glue's, the one `futex` serves.
        check_itimers: || {},
        futex_wake: akuma_syscalls_glue::futex_wake,
        eventfd_close: akuma_syscalls_glue::eventfd::eventfd_close,
        eventfd_clone_ref: akuma_syscalls_glue::eventfd::eventfd_clone_ref,
        unix_sock_close: akuma_syscalls_glue::unixsock::unix_sock_close,
        unix_sock_clone_ref: akuma_syscalls_glue::unixsock::unix_sock_clone_ref,
        epoll_destroy: akuma_syscalls_glue::poll::epoll_destroy,
        pidfd_close: akuma_runtime_tokio::PIDFD_CLOSE,
        dsp_close: || {},
        pty_clone_ref: |_, _| {},
        pty_close: |_, _| {},
        flock_release: akuma_syscalls_glue::flock::flock_release,
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
        // The fork memory pass: the twin's copy-on-write clone, INTO the
        // child's fresh `UserAddressSpace` root.
        fork_share_memory: twin_fork_share_memory,
        // The child's CR3 + spawn-row binding — the twin's slot binding.
        bind_child_task: |tid, child, kind| twin_bind_child_task(tid, child, kind),
    }
}

/// `ExecRuntime::fork_share_memory` — this kernel's copy-on-write clone, INTO
/// the child address space `fork_process` step 1 built. The parent's live root
/// (through its shared view) is walked by `clone_user_half`; the child's fresh
/// root gets the kernel half and the parent's user leaves, shared
/// copy-on-write. Ledger note: the shared frames are this kernel's, not the
/// crate ledger's — a child `Process::drop` frees only what it tracked.
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
    crate::sched::set_slot_fs(cur, ctx.fs_base);
    enter_user_on(root, &ef)
}

/// Load `root` (when non-zero) and `sysret` into `ef` with the kernel lock
/// released: the one way a task enters ring 3 here — a new thread or fork
/// child ([`twin_enter_user`]) and an `execve`'s new image.
pub fn enter_user_on(root: u64, ef: &akuma_ring3_amd64::EntryFrame) -> ! {
    if root != 0 {
        // SAFETY: `root` is a PMM-resident page-table frame naming the task's
        // image and the shared kernel half this code runs on.
        unsafe {
            core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
        };
    }
    // Ring 3 runs without the kernel lock.
    let _ = crate::smp::bkl_leave_all();
    // SAFETY: ring3's contract — an enterable ring-3 state for the address
    // space just loaded, kernel IF clear. `enter_user` is `!`.
    unsafe { akuma_ring3_amd64::enter_user(ef) }
}

/// Enter ring 3 for the thread running now from a captured context: [`twin_enter_user`] for the init thread, which has no
/// parent to fork it (`child::start_init_thread`).
pub fn enter_user_ctx(ctx: &akuma_exec::process::UserContext) -> ! {
    twin_enter_user(ctx)
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
        // What glue's `prlimit64(RLIMIT_STACK)` reports (nothing else reads it on
        // this kernel: the loader is its own): the 8 MiB the main stack may grow to.
        user_stack_size: crate::STACK_MAX as usize,
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
        // A process's captured stdout keeps at most 1 MiB; there is no
        // stdin pump.
        proc_stdin_max_size: 0,
        proc_stdout_max_size: 1024 * 1024,
        // The crate's region-walk CoW does not run: `fork_share_memory`
        // (`clone_user_half`) shares the parent's leaves copy-on-write itself.
        cow_fork_enabled: false,
        vfork_fastpath_enabled: false,
        // A pending `tkill` alone does not interrupt a blocking call: glue-amd64's
        // `tkill` interrupts the thread itself when the default action kills.
        pthread_kill_eintr_enabled: false,
        shared_file_pages_enabled: false,  // no file-page cache
    }
}
