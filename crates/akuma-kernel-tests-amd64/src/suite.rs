//! The suite's one list: what runs, in what order, on both boot paths.
//!
//! Moved from `amd64/src/boot.rs` unchanged but for the paths: each `x::smoke_test` the
//! kernel still carries inline is reached through `akuma_kernel_amd64::testing` (`k` here).

use akuma_kernel_amd64::testing as k;
use akuma_selftest::Suite;

use k::boot::{SuiteCtx, Verdict};

/// Run the whole self-test suite. One list, one order, both boot paths.
///
/// Ordering notes that are not obvious from the call list:
///
/// - `idt::smoke_test` needs the PMM (demand paging), which is why it is here
///   and not next to `idt::init`.
/// - `idt::user_copy_smoke_test` goes after demand paging is known-good: it is
///   the one path on which a kernel-mode `#PF` is not fatal, and it takes three
///   of them on purpose.
/// - The scheduler tests want a live tick, so the timer is started for them and
///   stopped again — every test in between ends with interrupts masked.
/// - `console::init` and `usermode::init_syscall` sit *inside* the suite
///   because the userspace tests after them need both. They are spelled once,
///   in `boot::wire_console_and_syscalls`, which `boot::late_init` also calls — see
///   there for the rule.
/// - `lapic::clock_rate_check` is last and **before any user process**: it is
///   what leaves interrupts on for the rest of the boot, and it checks the tick
///   *rate*, not merely that ticks arrive — a clock that is only moving still
///   scales every network timeout by however wrong it is.
///
/// # Compiled out by `no-tests`
///
/// Which is why the two initialisation points above are called out: this
/// function is not purely a suite, and a build without it still needs what it
/// sets up. `boot::late_init` is that subset, in this order.
pub fn self_tests(cx: &SuiteCtx) -> Verdict {
    let mut t = Suite::new("Akuma/amd64 self-test", k::boot::suite_emit);
    run(&mut t, cx)
}

fn run(t: &mut Suite, cx: &SuiteCtx) -> Verdict {
    let flag = |name: &str| cx.cmdline.split_ascii_whitespace().any(|w| w == name);

    // The tests below drive syscall bodies with kernel-stack buffers where a
    // program would pass user pointers. `uaccess` refuses kernel addresses —
    // that is its job — so they run inside the same bypass window the AArch64
    // kernel's boot tests use. Dropped before the verdict, because `run_init`
    // runs a real program and its bad pointers must be `EFAULT`.
    let user_ptr_bypass = akuma_user_access::BypassValidationGuard::new();

    // Guest-clock stamp for the whole suite, reported as a `note` at the end.
    //
    // **Read what this is before using it to compare anything.** Two facts,
    // both measured 2026-09-07 on QEMU/TCG at `SMP=4`:
    //
    // 1. *Host wall time cannot A/B this target.* The same unchanged binary
    //    booted in 41 s, 42 s and 68 s. A host-side stopwatch will confirm any
    //    hypothesis you bring it, and it did — three "measurements" of a
    //    context-switch change here were pure noise before this note existed.
    // 2. *Neither can this number, and it is the more dangerous of the two
    //    because it looks trustworthy.* It is dominated by the tests that wait
    //    on a **timeout** — the netpoll/DNS/SNTP family — so whether the
    //    boot-time clock sync happens to land moves it by seconds. One
    //    unchanged binary measured 2.43 M, 2.61 M and 2.54 M µs in three
    //    consecutive boots (a tidy ±3.5%, which is exactly what made it look
    //    like a signal) and **6.82 M** on the fourth. Its host wall times over
    //    the same four runs moved the opposite way.
    //
    // So: useful as a per-boot fact in the log, and as a tripwire for a change
    // that alters how long the suite *waits*. Not a benchmark, and three
    // agreeing samples do not make it one. A hot-path cost wants a probe that
    // runs the path in a loop, the way `scripts/benchmarks/` does it.
    let suite_start_us = k::net::uptime_us();

    // The shared `log` sink (`akuma_kernel_core::klog`), installed by
    // `install_shared_sinks` on both boot protocols. Without it the shared
    // crates' `log::warn!` lines vanish on this target and nothing says so.
    t.check("klog: the log crate's sink is installed", akuma_kernel_core::klog::is_installed());
    k::mem::smoke_test(t);
    k::paging::smoke_test(t);
    // The *other* x86 walker: `akuma-mmu`'s, which the shared crates reach
    // through and C1 folds this kernel onto. Right after `paging::smoke_test`
    // because the two are the same job done twice, and a divergence between
    // them is easiest to read when their results are adjacent.
    crate::uas_tests::smoke_test(t);
    k::idt::smoke_test(t);
    k::idt::user_copy_smoke_test(t);
    k::uaccess::smoke_test(t, cx.smap);

    if t.check("lapic: initialised", k::lapic::init()) {
        k::lapic::smoke_test(t);
        // Restart the timer the smoke test stopped: the scheduler wants a live
        // tick to drive NEED_RESCHED.
        k::lapic::start_timer();
        k::sched::smoke_test(t);
        k::sched::block_smoke_test(t);
        #[cfg(feature = "ring0-preempt")]
        k::sched::ring0_preempt_smoke_test(t);
        k::lapic::stop_timer();
    }
    // Needs no tick and runs no task: it claims one unpublished slot, writes a
    // ring-3 register file into it through `akuma-threading`, reads it back and
    // abandons the slot. Outside the LAPIC block for that reason.
    k::sched::user_context_smoke_test(t);
    // Beside it for the same reason: it claims two unpublished slots and never
    // publishes either, so it needs neither a tick nor a running task.
    k::sched::reaper_smoke_test(t);

    k::pipe::smoke_test(t);
    // The signal ABI and its two filters. Beside `pipe` rather than with the
    // ring-3 tests below because it needs no process and no ring-3 excursion —
    // it is structure layout and two pure functions. The half that needs a real
    // musl program is `/probes/sigprobe`, run by `amd64_ring3_check.py`.
    k::signal::smoke_test(t);
    k::reboot::smoke_test(t);

    // How many failures were xHCI's, reported rather than acted on: an
    // optional peripheral misbehaving is worth telling apart from the kernel
    // misbehaving when you are reading the log, and it used to gate whether
    // `init` ran at all. It no longer does — see `Verdict::passed`.
    let failures_before_usb = t.failed();
    if cx.have_pci {
        k::pci::smoke_test(t);
        let have_xhci = cx.want_xhci
            && k::pci::find_class(0x0c, 0x03).is_some_and(|d| d.header.prog_if == 0x30);
        if cx.want_xhci {
            k::xhci::smoke_test(t, have_xhci);
        } else {
            t.note("xhci: not requested (pass `usb` or `root=/dev/sda1`)", 0);
        }
    }
    let usb_failures = t.failed() - failures_before_usb;
    if usb_failures > 0 {
        t.note("xhci: of the failures above, this many were USB's", u64::from(usb_failures));
    }

    k::blk::smoke_test(t, cx.have_disk);
    k::fs::smoke_test(t, cx.have_fs);
    k::fd::smoke_test(t, cx.have_fs);
    k::mm::smoke_test(t);
    k::net::smoke_test(t, cx.have_net);
    k::sock::smoke_test(t, cx.have_net);

    k::boot::wire_console_and_syscalls();
    // `O_NONBLOCK` on the console descriptor, which needs the console the line
    // above just created — see `fd::console_nonblock_test`.
    k::fd::console_nonblock_test(t);
    // The dispatch table itself, before anything runs through it: the
    // legacy-x86 list and the neutral `Syscall` table must stay disjoint,
    // and the x86_64 -> asm-generic hop C1 folds through must still happen.
    crate::usermode_tests::dispatch_smoke_test(t, cx.have_fs);
    crate::usermode_tests::identity_cost_test(t);
    crate::usermode_tests::smoke_test(t);
    crate::usermode_tests::preempt_test(t);
    // The per-core live-L0 registry, here rather than with the rest of
    // `uas::smoke_test` because it asks about switches that have actually
    // happened — see the function's own note.
    crate::uas_tests::live_l0_registry_test(t);

    // The other cores. `nosmp` boots single-core: a bring-up lever, not policy —
    // on a machine whose only console is a framebuffer, `[BKL] stuck` chatter
    // from four cores interleaves into every other line, and taking them out of
    // the picture is the cheapest way to decide whether a fault is cross-core.
    let nosmp = flag("nosmp");
    let started = k::boot::bring_up_secondaries(cx.machine.madt.as_ref(), cx.facts, cx.cmdline, &cx.keep_out);
    let expected_aps = if nosmp {
        0
    } else {
        let cap = k::smp::cpu_cap();
        cx.machine
            .madt
            .as_ref()
            .map_or(0, |m| m.cpus().len().saturating_sub(1).min(k::smp::MAX_CPUS - 1).min(cap - 1))
    };
    k::smp::smoke_test(t, expected_aps, started);
    // Right after the secondaries come up: the shootdown's whole termination
    // argument rests on the cores being able to take and acknowledge the IPI,
    // and proving it before any user task exists keeps a later wedge
    // unambiguous. Single core notes and skips rather than failing.
    k::shootdown::smoke_test(t);
    crate::usermode_tests::smp_parallel_test(t);

    k::lapic::start_timer();
    // **A descriptor identity for every test below that reads a child's
    // stdout.** The fourth time this has been the prerequisite nobody listed
    // (`fd::boot_row_register`, and batch 2b/2d found the first three): every
    // `akuma-syscalls-glue` arm resolves `current_process_shared()` first, and
    // this suite runs on the boot task, which is registered nowhere. `read`
    // and `write` are glue's arms since 4b batch 3a, and a spawned child's
    // stdout is a pipe *descriptor* — so `spawn`, `busybox`, `execve`, `fork`
    // and `redirect` all went `EBADF` at once when the arms moved.
    //
    // Bracketing the whole block rather than each test, and **outside** their
    // `free_count()` windows on purpose: `make_test_process` builds an address
    // space, so a register/release inside one of those windows would read as
    // the leak the check is looking for.
    let usermode_boot_tid = k::fd::boot_row_register();
    crate::usermode_tests::elf_test(t);
    crate::usermode_tests::thread_test(t);
    crate::usermode_tests::kill_test(t);
    crate::usermode_tests::sigregs_test(t);
    crate::usermode_tests::socketpair_smoke_test(t);
    crate::usermode_tests::fdprobe_test(t);
    crate::usermode_tests::spawn_test(t);
    crate::usermode_tests::spawn_stale_group_state_test(t);
    crate::usermode_tests::winsize_to_child_test(t);
    crate::usermode_tests::session_terminal_is_private_test(t);
    crate::usermode_tests::wait4_ownership_test(t);
    crate::usermode_tests::console_notify_test(t);
    crate::usermode_tests::busybox_test(t);
    crate::usermode_tests::execve_test(t);

    // `strace` traces the fork test specifically: it is the one that exercises
    // the most syscall surface in the shortest time, and tracing the whole boot
    // buries it.
    let trace_fork = flag("strace");
    if trace_fork {
        k::usermode::SYSCALL_TRACE.store(true, core::sync::atomic::Ordering::Relaxed);
    }
    crate::usermode_tests::fork_test(t);
    if trace_fork {
        k::usermode::SYSCALL_TRACE.store(false, core::sync::atomic::Ordering::Relaxed);
    }

    crate::usermode_tests::redirect_test(t);
    crate::usermode_tests::sigsuspend_test(t);
    crate::usermode_tests::closed_stdin_test(t);
    // Hand pid 1 back before `run_init` claims it for the real init process.
    let drained = k::fd::boot_row_release(usermode_boot_tid);
    t.check("usermode: the borrowed identity was reclaimed", drained >= 1);
    k::lapic::stop_timer();

    // Deliberately here and not beside `mm::smoke_test`: this is the one check
    // that needs real programs to have run first. See its own doc.
    k::mm::demand_paging_report(t);

    if k::net::netpoll_drain_selftest(t, cx.have_net) {
        if cx.have_net {
            k::lapic::start_timer();
            k::clock::sync_via_sntp();
            k::lapic::stop_timer();
        }
        // **The timer must run for this one**, and it is the same bracket the
        // SNTP sync above already uses. The test asks whether the netpoll daemon
        // gets scheduled; `lapic::stop_timer()` a few lines up means it is being
        // asked under a condition the daemon never actually runs in, and its own
        // budget — `net::uptime_us()`, i.e. `lapic::ticks()` — cannot advance
        // either, so the timeout it thinks it has does not exist.
        //
        // Measured 2026-09-07 on bare metal across two boots of the same binary:
        // `netpoll laps 101` on one and `laps 0` on the next, the second having
        // spun to the crate's yield cap. Networking was healthy on both — the
        // daemon starts lapping as soon as the timer is restarted at the end of
        // the suite — so what varied was the measurement, not the machine. Run
        // it with the clock on and both of its bounds mean what they say.
        k::lapic::start_timer();
        k::net::netpoll_spawn_selftest(t);
        k::lapic::stop_timer();
        if flag("netprobe") {
            k::net::enable_probe();
        }
    }

    k::lapic::clock_rate_check(t);

    drop(user_ptr_bypass);

    // What the suite's own workload did to the scheduler. Notes rather than
    // checks: the numbers depend on timing and on how much work the boot found
    // to do, so a threshold here would be a flake. `backstop releases` is the
    // one to watch — see `sched::BACKSTOP_US`; a number that grows names a wait
    // whose wake path is missing.
    t.note("sched: parks over the whole suite", k::sched::blocks());
    t.note("sched: wakes over the whole suite", k::sched::wakes());
    t.note("sched: backstop releases (0 is the healthy value)", k::sched::backstop_wakes());
    // A note, not a check, for the same reason: how many slots the sweep has
    // carried `TERMINATED → FREE` depends on how many threads the boot happened
    // to run and on whether the idle loop got a turn. Worth printing because a
    // number that stays 0 on a box that has run processes is the signature of
    // the gap this exists to close — no collector is calling it
    // (`docs/archive/AKUMA_AMD64_NO_SLOT_RECYCLER.md`).
    t.note("sched: thread slots reaped", akuma_threading::reaps());

    t.note("suite: guest microseconds elapsed", k::net::uptime_us().saturating_sub(suite_start_us));
    // **Bounded-hold tripwire** (`akuma-bkl`'s `MAX_WAIT_SPINS`). A healthy
    // ticket lock serves a waiter in microseconds; six figures of spins means
    // some holder kept the BKL across a seconds-long kernel stretch and every
    // other core sat IRQ-masked behind it. Compared against the previous
    // boot's note by eye — an order-of-magnitude jump is the regression.
    t.note("bkl: longest waiter spins this boot", akuma_bkl::sync::max_wait_spins());
    Verdict { passed: t.report() }
}
