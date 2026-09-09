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
//! # The fd-lifecycle family, and how its argument aged
//!
//! This header used to say the whole family — `pipe_*`, `eventfd_*`,
//! `unix_sock_*`, `epoll_destroy`, `pidfd_close`, `remove_socket`,
//! `socket_clone_ref`, `flock_release` — could not fire, because it is called
//! from `akuma_exec`'s **own** `FdTable` teardown and this target uses
//! `fd.rs`'s `FDS`/`FILES` instead; and that wiring them to `crate::pipe`
//! would be *actively wrong*, an `akuma-exec` pipe id and a
//! `crate::pipe::PipeId` being different namespaces.
//!
//! **Both halves of that were true and are now not.** C2 slice 4 mirrors every
//! descriptor this target opens into the process's registered
//! `SharedFdTable`, and `SharedFdTable::drop` runs `close_all()` — so the
//! family fires. And the namespace argument was about a world in which no
//! `SharedFdTable` here ever held a pipe or a socket: the payload of the
//! `FileDescriptor::PipeRead`/`PipeWrite`/`Socket` this target constructs *is*
//! a `crate::pipe` id and an `akuma_net::socket` index, because those are the
//! only allocators it has. The pipe hooks were wired in slice 4 and the socket
//! hooks in slice 7, each at its own field with that argument restated.
//!
//! **Ten `not_wired!` stubs are left** — it was 16 when the C2 plan was
//! written — and they divide cleanly, each saying which:
//!
//! - **not built for this target** — `rump_socket_clone_ref`, `eventfd_*`,
//!   `unix_sock_*`, `epoll_destroy`, `pidfd_close`. Seven of these, and none
//!   is C2's business.
//! - **the subsystem does not exist here** — `flock_release` (nothing
//!   dispatches `flock(2)`), `resolve_file_id` and `read_at_by_inode` (they
//!   name a file by `(mount id, inode)`; this target has one filesystem and no
//!   mount table).
//!
//! None of the ten still says "C2", and that is the point of having gone
//! through them: a stub whose stated reason is a *step* stops being true when
//! the step lands, and nothing in the type system notices.

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
/// `akuma_cpu::daif::mask_irq()` since 2026-09-08, when that grew real x86
/// arms (`cli`/`sti`). The warning this function used to carry — that `daif`
/// is a silent no-op on x86_64 and every `IrqGuard` with it — is retired by
/// the same change; `paging.rs`'s private `pushfq`/`cli` copy migrated at the
/// same time. Unconditional by the hook's contract: `with_irqs_disabled` pairs
/// these, so nesting is a caller property it already manages.
fn disable_irqs() {
    akuma_cpu::daif::mask_irq();
}

/// Unmask local IRQs on this core. See [`disable_irqs`].
fn enable_irqs() {
    akuma_cpu::daif::unmask_irq();
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
        // Both discard *which* `FsError` the VFS reported: the hook's error
        // channels are an `i32` errno-ish code and a `&'static str`, and
        // `akuma-exec` only tests for `Err`.
        read_file: |path| crate::fs::read_file(path).map_err(|_| -1),
        file_size: |path| {
            crate::fs::metadata(path)
                .map(|m| m.size)
                .map_err(|_| "fs error")
        },
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

        // ── wired C2 slice 7 ──────────────────────────────────────────────
        // The socket hooks, for the reason the pipe hooks below already carry
        // and by the same argument: `fd.rs`'s `alloc_socket_fd` builds a
        // `FileDescriptor::Socket(idx)` whose payload **is** an
        // `akuma_net::socket` index, and slice 4 mirrors that descriptor into
        // the registered table, so a `SharedFdTable` on this target can hold
        // one. The two namespaces do not meet; there is one socket table.
        //
        // They were `not_wired!` — i.e. `panic!` — and that is not a
        // theoretical cost. `SharedFdTable::drop` runs `close_all()`, which
        // fires these per entry, so the *only* thing standing between a
        // socket in a mirror and a kernel panic was every caller remembering
        // `fd::clear_table_mirror` first. Slice 6 found a path that did not
        // (`sys_spawn`'s `spawn_process_task` failure, where an unregistered
        // `Arc<SharedFdTable>` dropped with three descriptors in it), and a
        // panic is a poor way to learn that. Wired, the forgotten case is a
        // double close instead of a dead machine — still a bug, and one the
        // refcounts can be made to catch, rather than one that takes the box
        // down.
        remove_socket: |idx| crate::sock::close(idx),
        socket_clone_ref: akuma_net::socket::socket_clone_ref,
        rump_socket_clone_ref: |_, _| not_wired!("rump_socket_clone_ref", "rump is not built for this target"),
        // ── wired C2 slice 4 ──────────────────────────────────────────────
        // The pipe hooks. The module header's warning — that mapping an
        // `ExecRuntime` pipe id onto `crate::pipe` would be "actively wrong" —
        // described the pre-C2 world, where no `SharedFdTable` on this target
        // ever carried a pipe. Slice 4 mirrors descriptors into the registered
        // tables, so `FileDescriptor::PipeRead/PipeWrite` now exist here — and
        // every one of them carries a `crate::pipe::PipeId`, because
        // `crate::pipe` is the only pipe allocator this target has. The two
        // namespaces do not meet; on this target the variant's payload IS a
        // `crate::pipe` id. (`akuma-pipes`, whose ids these hooks were shaped
        // by, exists only inside AArch64's kernel.)
        pipe_close_write: |id| crate::pipe::close_write(id as usize),
        pipe_close_read: |id| crate::pipe::close_read(id as usize),
        pipe_clone_ref: |id, is_write| crate::pipe::clone_ref(id as usize, is_write),
        eventfd_close: |_| not_wired!("eventfd_close", "sc-eventfd is not in this target's feature set"),
        eventfd_clone_ref: |_| not_wired!("eventfd_clone_ref", "sc-eventfd is not in this target's feature set"),
        unix_sock_close: |_| not_wired!("unix_sock_close", "AF_UNIX is not built for this target"),
        unix_sock_clone_ref: |_| not_wired!("unix_sock_clone_ref", "AF_UNIX is not built for this target"),
        epoll_destroy: |_| not_wired!("epoll_destroy", "sc-epoll is not in this target's feature set"),
        pidfd_close: |_| not_wired!("pidfd_close", "sc-pidfd is not in this target's feature set"),
        // Not `fd.rs` folding any more, and the reason is narrower than it
        // was: **this target has no `flock`.** `sys_flock` is not dispatched,
        // nothing takes a lock, so nothing can release one. Reaching here
        // means a folded arm brought advisory locking with it, and the panic
        // is the notice.
        flock_release: |_, _, _| not_wired!("flock_release", "this target dispatches no flock(2)"),

        // ── wired C2 slice 7 ──────────────────────────────────────────────
        // The **path-addressed** read. `fs::read_at` is what slice 5 rebuilt
        // this target's whole `read`/`pread` path on top of, so it is the same
        // byte surface ring 3 already gets, not a second one — and the two
        // callers that reach it (`akuma-elf`'s `ElfSource::Path`, and glue's
        // partial reads) then behave here as they do on AArch64.
        read_at: |path, off, buf| crate::fs::read_at(path, off, buf).map_err(|_| -1),

        // ── still not wired: the **inode**-addressed read surface (C2) ────
        // These two are the lazy/prefault path and they are a different
        // question from `read_at` above: they name a file by `(mount id,
        // inode)`, and this target has no mount table to give an id from —
        // `KernelFile::new` leaves the inode 0 ("read by path") precisely
        // because there is one filesystem here. Answering with an invented
        // pair would be the `[0,0,0,0]` zero-page class of bug, the one case
        // in this tree where a stub returning `Err` was itself the defect
        // (`docs/archive/PREFAULT_INODE_STUB_ZERO_PAGES.md`). So: panic, and
        // wire them for real when this target has a mount table to key on.
        resolve_file_id: |_| not_wired!("resolve_file_id", "no mount table to give an id from"),
        read_at_by_inode: |_, _, _, _| not_wired!("read_at_by_inode", "no mount table to give an id from"),

        // ── wired 5b slice 4 ──────────────────────────────────────────────
        // `akuma-exec` calls this from `clear_child_tid` on process exit —
        // the `pthread_join` wake. It was a `not_wired!` panic on the grounds
        // that `crate::futex` is "keyed by its own task ids, a different
        // namespace from `akuma-exec`'s pids"; that named the wrong half of the
        // table. The waiter *identity* is a scheduler task slot, but the
        // **key** is `(tgid, uaddr)`, and since 5b slice 2 that tgid is
        // `current_pid()`, i.e. `akuma-exec`'s own pid. So the argument arrives
        // in the namespace the table already uses and needs no translation.
        futex_wake: |tgid, uaddr, count| {
            let _ = crate::futex::wake_key(tgid, uaddr, count);
        },
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
///
/// # Why this is not `akuma_exec::init`
///
/// That function registers seven upward surfaces at once — the BKL yield hook,
/// `akuma-mmu`'s scheduler gates, the PMM's surviving-mapper walk, the prefault
/// hook, `akuma-threading`'s whole table — against subsystems this target
/// serves from `amd64/src` and in an order `boot.rs` has not reached yet. So
/// this registers the two tables it needs, **and each further hook explicitly
/// when a step starts depending on it**, rather than taking all seven and
/// finding out which ones fire.
///
/// `akuma-elf`'s four VFS callbacks are the first of those, added by C1
/// step 6. They are `require()`, not `get()`, on purpose (see that crate's
/// module header): unregistered, the first dynamically-linked binary reaches
/// `load_interp_for` and panics with "VfsHooks not registered" rather than
/// reading zeros. **One** of the four forwards an `ExecRuntime` field that is
/// still a stub: `read_at` was wired for real in C2 slice 7 (it is
/// `fs::read_at`, the surface every `read`/`pread` on this target already goes
/// through), and `resolve_file_id` is not, because it wants a `(mount id,
/// inode)` pair from a target with no mount table. That is the right shape:
/// both are reached only by `ElfSource::Path`, which this target's profile
/// never selects, and if the stub ever is, the panic names itself.
pub fn init() {
    let rt = runtime();
    akuma_exec::runtime::register(rt, config());
    akuma_elf::register_vfs_hooks(akuma_elf::VfsHooks {
        read_file: rt.read_file,
        read_at: rt.read_at,
        resolve_file_id: rt.resolve_file_id,
        exec_bkl_drop_enabled: rt.exec_bkl_drop_enabled,
    });
}
