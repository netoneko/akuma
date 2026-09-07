//! The `akuma-exec` runtime + config this target registers, and nothing else.
//!
//! # Why this file exists
//!
//! **C1 step 3.** Folding the first syscall arm into `akuma-syscalls-glue`
//! (`docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md`) does not reach the
//! arm at all without this: glue's user-copy helpers are
//! `akuma_exec::process::user_access::*`, and the first thing they touch is
//! `akuma_exec::runtime::config()`, which is a `Registered` cell. Unregistered,
//! it panics —
//!
//! ```text
//! [PANIC] crates/akuma-not-even-once/src/lib.rs:208
//!         akuma-exec: ExecConfig not registered — call akuma_exec::init() first
//! ```
//!
//! — which is exactly what the first folded `uname` did. The C1 hand-off prompt
//! listed three prerequisites for glue (the identity cache, the excursion hooks,
//! an `sc-*` feature selection); this was not among them and is larger than all
//! three.
//!
//! `akuma_exec::register` also points `akuma_primitives::console` and
//! `akuma_primitives::clock` at this kernel's sinks, so `tprint!` in any shared
//! crate starts carrying a real `[T…]` stamp here rather than `[T0.00]`.
//!
//! # The shape: three kinds of hook, and the third is the point
//!
//! The AArch64 twin is `akuma_kernel_glue::build_exec_runtime`. This is not a
//! copy of it — most of that table names subsystems this target does not have —
//! so every field is one of three things, and each is labelled:
//!
//! 1. **Real.** This target has the thing and the hook points at it.
//! 2. **A no-op that provably cannot fire**, with the argument for why written
//!    at the field. Same arrangement the AArch64 table uses for its gated-out
//!    Tier 2 callbacks: the `FileDescriptor` variant is never constructed, so
//!    the callback exists only so the struct compiles.
//! 3. **[`not_wired`] — a loud stub that panics naming itself.**
//!
//! Category 3 is the one worth defending. The alternative is a silent no-op or
//! a plausible default, and this tree's whole history says what that costs: an
//! `Err(-1)` stub on `read_at_by_inode` made every file prefault install a zero
//! page and surfaced as a `rustc` metadata ICE
//! (`docs/archive/PREFAULT_INODE_STUB_ZERO_PAGES.md`); a `close` that reported
//! success and dropped the data is `AKUMA_SELF_HOSTING_AMD64.md`'s open issue 1.
//! A hook that cannot be served yet should say so, at the moment it is reached,
//! with its own name in the message — now that the panic handler prints the
//! message at all (`main.rs`, fixed in the same change).
//!
//! **A category-3 field firing is not a bug in this file.** It is the next
//! syscall family asking for a subsystem that has not been folded yet, and the
//! message names which one.
//!
//! # What is deliberately NOT here
//!
//! The fd-lifecycle family — `pipe_*`, `eventfd_*`, `unix_sock_*`,
//! `epoll_destroy`, `pidfd_close`, `remove_socket`, `socket_clone_ref`,
//! `flock_release` — is called from `akuma_exec`'s **own** `FdTable` teardown.
//! This target does not use that table; `fd.rs` has its own `FDS`/`FILES`. So
//! none of them can fire, and wiring them to `crate::pipe` would be actively
//! wrong: an `akuma-exec` pipe id and a `crate::pipe::PipeId` are different
//! namespaces, and mapping one onto the other would close a pipe nobody asked
//! about. They become real in **C2**, when the fd surface folds.

use akuma_exec::{ExecConfig, ExecRuntime};

/// A hook this target cannot serve yet: panic, naming itself.
///
/// See the module header for why this is not a no-op. The message is the whole
/// value, so it names the hook and the step that will wire it.
macro_rules! not_wired {
    ($name:literal, $step:literal) => {
        panic!(concat!(
            "amd64: ExecRuntime::", $name, " was called but is not wired yet (", $step, "). ",
            "A syscall folded into akuma-syscalls-glue has reached a subsystem this target ",
            "still serves from amd64/src. See amd64/src/exec_runtime.rs."
        ))
    };
}

/// Mask local IRQs on this core.
///
/// Not `akuma_cpu::daif::mask_irq()`: **that is a silent no-op on x86_64** —
/// its `asm!` is `#[cfg(target_arch = "aarch64")]` and every other arm falls
/// through to an empty body. That is a real pre-existing defect on this target
/// and a wider one than this file (`akuma_primitives::irq::irq_save_mask`,
/// `IrqGuard` and `PreemptGuard` all route through it, so `akuma-bkl`'s ticket
/// wait is not IRQ-atomic here), but it is **not** C1's to fix: it wants its own
/// change and its own SMP=4 A/B. Recorded rather than ridden along with.
fn disable_irqs() {
    // SAFETY: masking is the conservative direction; no memory effect.
    unsafe { core::arch::asm!("cli", options(nomem, nostack, preserves_flags)) };
}

/// Unmask local IRQs on this core. See [`disable_irqs`].
fn enable_irqs() {
    // SAFETY: the callers that unmask are the ones that masked.
    unsafe { core::arch::asm!("sti", options(nomem, nostack, preserves_flags)) };
}

/// Build the runtime table. See the module header for the three categories.
fn runtime() -> ExecRuntime {
    ExecRuntime {
        // ── real ──────────────────────────────────────────────────────────
        // The same clock `threading::ThreadRuntime` was already given, so the
        // scheduler and `akuma-exec` cannot disagree about what time it is.
        uptime_us: crate::net::uptime_us,
        disable_irqs,
        enable_irqs,
        end_of_interrupt: |_vector| crate::lapic::eoi(),
        heap_stats: || {
            let s = akuma_alloc::stats();
            (s.heap_size, s.allocated)
        },
        is_memory_low: akuma_alloc::is_memory_low,
        print_str: crate::serial::puts,
        // `fs::read_file` returns `Option`; the hook's error channel is an
        // `i32` errno-ish code and `akuma-exec` only tests for `Err`.
        read_file: |path| crate::fs::read_file(path).ok_or(-1),
        file_size: |path| crate::fs::metadata(path).map(|m| m.size).ok_or("fs error"),
        // This target resolves symlinks inside `fd.rs`'s path walk rather than
        // as a separate pass, so the honest answer for "the path with symlinks
        // resolved" is the path itself: every caller here hands the result
        // straight back to a `fd.rs` entry point, which resolves it again.
        // Identity is correct, not a stub — and it stops being identity in C2,
        // when path resolution moves to the VFS.
        resolve_symlinks: |path| alloc::string::String::from(path),

        // ── no-ops that cannot fire ───────────────────────────────────────
        // No cross-core scheduler IPI on this target: `akuma-threading`'s x86
        // switch is cooperative and synchronous (see `sched.rs`), which is why
        // `ThreadRuntime` was given the same three no-ops. Keeping them
        // identical is deliberate — two tables disagreeing about whether a wake
        // rings a core is the kind of difference that shows up as a hang.
        trigger_sgi: |_| {},
        wake_core: |_| {},
        wake_remote_idle: || false,
        // The execve/ELF-load BKL-drop is an `smp-shared` optimisation this
        // target does not build; `false` is what the AArch64 kernel answers
        // without that feature too.
        exec_bkl_drop_enabled: || false,
        // `akuma-exec`'s process table is not built here (C1 step 5), so no
        // process it knows about can exit.
        on_process_exit: |_pid| {},
        // ITIMER_REAL/alarm expiry, driven from the tick. This target has no
        // itimers at all — that is C3 (`clock.rs`) — and nothing arms one, so
        // there is never an expiry to check.
        check_itimers: || {},
        // Containers are not built for this target: there is no box table and
        // `sc-containers` is not in its feature set, so no namespace exists to
        // return and nothing can set one.
        get_box_namespace: |_box_id| None,
        set_spawn_namespace: |_ns| {},
        clear_spawn_namespace: || {},

        // ── not wired: the fd-lifecycle family (C2) ───────────────────────
        // Called only from `akuma_exec::FdTable` teardown, which this target
        // does not use. See the module header on why pointing these at
        // `crate::pipe` would be wrong rather than merely premature.
        remove_socket: |_| not_wired!("remove_socket", "C2: fd.rs folds into glue"),
        socket_clone_ref: |_| not_wired!("socket_clone_ref", "C2: fd.rs folds into glue"),
        rump_socket_clone_ref: |_, _| not_wired!("rump_socket_clone_ref", "rump is not built for this target"),
        pipe_close_write: |_| not_wired!("pipe_close_write", "C2: fd.rs folds into glue"),
        pipe_close_read: |_| not_wired!("pipe_close_read", "C2: fd.rs folds into glue"),
        pipe_clone_ref: |_, _| not_wired!("pipe_clone_ref", "C2: fd.rs folds into glue"),
        eventfd_close: |_| not_wired!("eventfd_close", "sc-eventfd is not in this target's feature set"),
        eventfd_clone_ref: |_| not_wired!("eventfd_clone_ref", "sc-eventfd is not in this target's feature set"),
        unix_sock_close: |_| not_wired!("unix_sock_close", "AF_UNIX is not built for this target"),
        unix_sock_clone_ref: |_| not_wired!("unix_sock_clone_ref", "AF_UNIX is not built for this target"),
        epoll_destroy: |_| not_wired!("epoll_destroy", "sc-epoll is not in this target's feature set"),
        pidfd_close: |_| not_wired!("pidfd_close", "sc-pidfd is not in this target's feature set"),
        flock_release: |_, _, _| not_wired!("flock_release", "C2: fd.rs folds into glue"),

        // ── not wired: the VFS read surface (C2) ──────────────────────────
        // `read_file` above is enough for what glue reaches today. These three
        // are the lazy/prefault path, and a wrong answer here is the
        // `[0,0,0,0]` zero-page class of bug — the one case in this tree where
        // a stub returning `Err` was itself the defect
        // (`docs/archive/PREFAULT_INODE_STUB_ZERO_PAGES.md`). So: panic, and
        // wire them for real when file-backed mappings go through glue.
        read_at: |_, _, _| not_wired!("read_at", "C2: the VFS read surface"),
        resolve_file_id: |_| not_wired!("resolve_file_id", "C2: the VFS read surface"),
        read_at_by_inode: |_, _, _, _| not_wired!("read_at_by_inode", "C2: the VFS read surface"),

        // ── not wired: process lifecycle (C1 step 5) ──────────────────────
        // `akuma-exec` calls this from `clear_child_tid` on process exit. This
        // target's futex table is `crate::futex`, keyed by its own task ids —
        // a different namespace from `akuma-exec`'s pids, so forwarding would
        // wake the wrong waiter rather than none.
        futex_wake: |_, _, _| not_wired!("futex_wake", "C1 step 5: PROCS folds into akuma-exec"),
    }
}

/// Build the config. Overlapping fields mirror the `threading::ThreadConfig`
/// this target already registers in `sched.rs` rather than
/// `akuma-config`'s AArch64 values — two tables disagreeing about the stack
/// size or the canary is a silent corruption, and `sched.rs` is the one that
/// actually allocates the stacks.
fn config() -> ExecConfig {
    ExecConfig {
        max_threads: akuma_config::MAX_THREADS,
        // `sched.rs` registers `reserved_threads: 0` — this target has no
        // reserved system-thread band — and the two must agree.
        reserved_threads: 0,
        kernel_stack_size: crate::sched::STACK_SIZE,
        // No linker-derived boot-stack bounds here yet, and `sched.rs` passes
        // the same zeroes. Zero means "no slot-0 stack bounds", which is what
        // disables canary placement below; a made-up range would stamp a canary
        // into whatever happens to live there
        // (`docs/LOW_MEMORY_ENVIRONMENT.md` "Known bug").
        boot_stack_base: 0,
        boot_stack_top: 0,
        default_thread_stack_size: crate::sched::STACK_SIZE,
        system_thread_stack_size: crate::sched::STACK_SIZE,
        user_thread_stack_size: crate::sched::STACK_SIZE,
        // **The user stack, not the kernel one.** This read
        // `sched::STACK_SIZE` — 32 KiB, the per-thread *kernel* stack — which
        // was harmless while nothing consulted the field and became a wrong
        // answer to ring 3 the moment C1 step 3 batch 3 folded `prlimit64`
        // into glue: `RLIMIT_STACK` is this number, and `busybox ulimit -s`
        // reported 32 where the program actually has 512 KiB.
        //
        // It is also an unusually literal limit here. `loader::build_stack`
        // maps exactly `ELF_STACK_PAGES` eagerly, with no growth policy and no
        // guard page, so a program that recurses past it takes a `#PF` nothing
        // will service — the value is the hard edge, not a policy hint.
        user_stack_size: crate::usermode::ELF_STACK_PAGES * 4096,
        // Off, and off in `sched.rs` too — see `boot_stack_base` above.
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
        proc_stdin_max_size: akuma_config::PROC_STDIN_MAX_SIZE,
        proc_stdout_max_size: akuma_config::PROC_STDOUT_MAX_SIZE,
        // This target's own CoW fork is `mm.rs` + `akuma-cow`, not
        // `akuma-exec`'s, and it is SMP=1-only (`AKUMA_AMD64_COW.md`). The flag
        // gates `akuma-exec`'s path, which is not reachable here.
        cow_fork_enabled: false,
        vfork_fastpath_enabled: false,
        // No signal delivery on this target at all — that is A2.
        pthread_kill_eintr_enabled: false,
        // No file-page cache here yet; every file mapping gets its own copy
        // (`AKUMA_AMD64_MEMORY_CLOSEOUT.md`).
        shared_file_pages_enabled: false,
    }
}

/// Register both tables. Call once, before any syscall can reach glue.
///
/// Idempotent by `Registered`, but ordering is not: `akuma_exec::register` also
/// installs the shared console and clock hooks, so anything printed by a shared
/// crate before this point is stamped `[T0.00]`.
pub fn init() {
    akuma_exec::runtime::register(runtime(), config());
}
