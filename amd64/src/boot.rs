//! The parts of the boot that are the same whichever protocol got us here.
//!
//! # The problem this solves
//!
//! There are two entry points — [`crate::kmain`] for PVH (a VMM: Firecracker,
//! QEMU `microvm`) and [`crate::multiboot2::kmain_mb2`] for multiboot2 (GRUB, on
//! the bare-metal reference box) — and until 2026-09-07 each carried its own
//! copy of the whole boot, described in `kmain_mb2`'s own comment as
//! "deliberately parallel to `kmain`, and in the same order".
//!
//! Deliberately parallel is a promise nobody can keep. Measured the day this
//! module was written, the multiboot2 path was **not running seven tests** the
//! PVH path ran:
//!
//! ```text
//! blk::smoke_test              usermode::execve_test
//! sched::block_smoke_test      usermode::fork_test
//! usermode::spawn_test         usermode::busybox_test
//! usermode::console_notify_test
//! ```
//!
//! That is the process-lifecycle suite — `fork`, `execve`, `spawn`, a real
//! busybox — missing from **the least-tested path in the tree**, the one that
//! runs on real silicon where the emulators cannot reach. It is why bare metal
//! reported 262 checks where QEMU reported 295, and nobody had noticed, because
//! both said `0 failed`.
//!
//! The drift is not anyone's carelessness; it is what two hand-maintained
//! parallel lists do. `sched::init()` had to be added to both by hand the same
//! week, and adding it to only one would have been a panic on the path with no
//! serial console.
//!
//! # What is shared, and what is deliberately not
//!
//! Two blocks were **identical**, and they are here:
//!
//! - [`early_init`] — descriptor tables, per-CPU block, IDT, SMAP/SMEP/WP, the
//!   scheduler, and dropping the identity map. Byte-for-byte the same, and the
//!   ordering constraints between those six are load-bearing and subtle enough
//!   that having them written down twice was the real hazard.
//! - [`self_tests`] — the suite. One canonical list, one canonical order.
//!
//! What stays per-protocol is what genuinely differs, and it is worth naming so
//! the next person does not try to merge it too: how the console comes up (a
//! UART versus a GRUB framebuffer, which must exist before anything can report
//! a failure), where the machine description comes from, what the root
//! filesystem *is* (a virtio disk, a GRUB module in RAM, or ext2 on USB), how
//! the network is configured (DHCP from a VMM versus a static bare-metal
//! address), and what happens when `init` exits. Those are five different
//! decisions with five different reasons, and a single function taking five
//! callbacks would be a merge in name only.

#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;
use akuma_ryzen_amd64::MachineDescription;
use crate::uaccess::SmapStatus;

use crate::{fd, gdt, idt, lapic, paging, sched, smp, uaccess, usermode};
// Reached only from `self_tests`, which `no-tests` compiles out. Split from the
// list above rather than gated wholesale, because the other half of it is
// `early_init`'s and `late_init`'s.
#[cfg(not(feature = "no-tests"))]
use crate::{blk, fs, mm, net, pci, reboot, sock, xhci};

/// Bring the CPU to the state every later step assumes, and report SMAP/SMEP.
///
/// **The order here is load-bearing**, and each step's reason is a bug that was
/// paid for once:
///
/// 1. `gdt::init` — `boot.s` builds its GDT in the low boot region, because the
///    32-bit trampoline has to reach it with paging off. The CPU reads the GDT
///    on every exception delivery, so once the identity map is dropped that
///    table is unmapped and *any* fault becomes a triple fault: the first `#PF`
///    cannot be delivered, which raises `#DF`, whose delivery faults the same
///    way. This rebuilds it in the kernel's own high `.bss`, and must therefore
///    run **before** `drop_identity_map` — which is where it used to be, with
///    the symptom a triple fault inside the heap allocator and `CR2` pointing at
///    the GDT itself.
/// 2. `smp::init_bsp` — the per-CPU block and the Big Kernel Lock, before
///    anything reads `gs:`, which the scheduler and the syscall path both do.
/// 3. `idt::init` — needs nothing but its own static table, so it goes before
///    memory bring-up: a fault down there then prints a diagnosis instead of
///    vanishing.
/// 4. `uaccess::init_smap` — SMAP, SMEP **and `CR0.WP`**. From here a
///    kernel-mode touch of a user page without `stac` is a reportable fault, and
///    a kernel *write* to a read-only page honours the page tables (see that
///    function; `WP` being clear was silently defeating copy-on-write).
/// 5. `sched::init` — before anything can yield. Registration with
///    `akuma-threading` is once-only, and the network bring-up below yields, so
///    this is the single call site per entry point. It allocates nothing, so it
///    does not need the heap; it needs only the three steps above and a live
///    `CR3` to record as the kernel root.
/// 6. `paging::drop_identity_map` — the trampoline's map has done its job.
pub fn early_init() -> SmapStatus {
    gdt::init();
    smp::init_bsp();
    idt::init();
    let smap = uaccess::init_smap();
    sched::init();
    paging::drop_identity_map();
    smap
}

/// Point the shared crates at this kernel's sinks: the console hook and the
/// `akuma-exec` runtime + config.
///
/// **Both entry points must call this, and that is why it is a function.**
/// `set_print_hook` was already duplicated in the two `kmain`s; `exec_runtime`
/// arrived (2026-09-07) on the PVH one only, and the result was a kernel that
/// was green under QEMU and Firecracker and panicked on the metal the moment a
/// syscall folded into `akuma-syscalls-glue` ran — glue's user-copy helpers
/// read `akuma_exec::runtime::config()`, a `Registered` cell that panics when
/// absent. A boot-protocol-shaped failure with a memory-shaped message, which
/// is exactly the drift `early_init` above exists to stop.
///
/// Call it **after memory bring-up and before anything that can take a
/// syscall**. It allocates nothing and reads no hardware — `register` only
/// stores hooks, and `lapic::ticks()` is an atomic answering 0 before the timer
/// runs — so the only ordering it really needs is "before the first excursion".
/// `register` also installs the shared console and clock sinks, so `[T…]`
/// stamps in shared crates start being real here rather than `[T0.00]`.
pub fn install_shared_sinks() {
    // `safe_print!` discards output until a hook is registered, so without this
    // every diagnostic `akuma-virtio` emits — including the one naming why a
    // device failed to initialise — is silently dropped.
    akuma_primitives::console::set_print_hook(crate::serial::puts);
    crate::exec_runtime::init();
    // The entropy source `akuma-syscalls-glue`'s `getrandom(2)` reads. Glue's
    // default is the virtio-rng device, which this target does not have on any
    // rig and cannot have on the bare-metal box; without this, folding
    // `getrandom` into glue would return `EIO` to every ring-3 caller —
    // `sshd`'s key exchange included. See `akuma_primitives::rng`.
    //
    // Here rather than in `net::init` because it is not a network fact and
    // both boot protocols need it: this function exists precisely because a
    // step registered from one `kmain` and not the other is how the metal
    // died at the first folded syscall (`AKUMA_AMD64_C1_STEP3_PREREQUISITES.md`
    // §4).
    akuma_primitives::rng::set_rng_hook(crate::net::rng_fill_checked);
    // The wake effect for the **one** pipe table (4b batch 2b). This target
    // shares `akuma-syscalls-glue`'s table rather than keeping a second
    // instance of `akuma_pipes::PipeTable` — two instances meant two id
    // spaces, which is what made a folded `close(2)` close the wrong pipe.
    // Registered here for the same reason as the two hooks above: one call
    // site, both boot protocols, before anything can create a pipe.
    akuma_syscalls_glue::pipe::set_wake_sink(crate::pipe::wake_sink);
    // The VFS instance: the global mount table plus the four facts
    // `akuma-vfs-glue` cannot discover for itself. Before any mount, and on a
    // `DISK=none` boot where there will never be one — the synthetic `/dev` and
    // `/etc/mtab` nodes resolve against an empty table and still have to
    // answer. Same reason as the two registrations above: one call site, both
    // boot protocols. See `fs::init_vfs`.
    crate::fs::init_vfs();
    // The TLB shootdown. Two registrations, both idempotent and both needed on
    // **both** boot protocols — this function is exactly the remedy for the
    // "registered in one kmain and not the other" failure, and a shootdown
    // armed on PVH but not on multiboot2 would be green on both VMM rigs and
    // silently stale on the metal.
    //
    // - `akuma_mmu::set_shootdown_hooks`: what `flush_tlb_*`'s `AllCores`
    //   arms call. `shootdown::broadcast` answers `false` until the LAPIC is
    //   mapped and a second core is online, so being registered this early —
    //   `lapic::init` has not run yet on the self-test path — degrades to the
    //   core-local flush rather than writing an ICR through a null base.
    // - `akuma_bkl::sync::set_spin_assist`: the BKL ticket wait services pending
    //   shootdowns inline, which is what keeps the sender's acknowledgement
    //   wait bounded while a peer spins IRQ-masked for this very lock (the
    //   deadlock argument lives beside `set_shootdown_hooks` in `akuma-mmu`).
    akuma_mmu::set_shootdown_hooks(crate::shootdown::broadcast, crate::shootdown::wait_for_acks);
    akuma_bkl::sync::set_spin_assist(crate::shootdown::bkl_spin_assist);
}

/// What the shared suite needs to know about the machine it is running on.
// Five `bool`s, and clippy would rather they were flags. They are not: each is a
// separate question with a separate answer per boot protocol, and the whole
// point of this struct is that a reader can see *which* question each entry
// point answered differently. A packed encoding is exactly what gives that up.
#[allow(clippy::struct_excessive_bools)]
#[cfg(not(feature = "no-tests"))]
pub struct SuiteCtx<'a> {
    pub machine: &'a MachineDescription,
    /// The kernel command line, however this protocol obtained it. Every
    /// `strace`/`nosmp`/`netprobe`-style decision below reads it from here, so
    /// the two paths cannot answer the same flag differently.
    pub cmdline: &'a str,
    pub smap: SmapStatus,
    /// PCI was scanned, so its tests can run.
    pub have_pci: bool,
    /// Test xHCI if a controller is present. PVH says yes whenever PCI was
    /// scanned; bare metal says yes only when the command line asked for USB,
    /// because bringing the controller up on a box booted from RAM is a slow
    /// no-op with a real chance of hanging on somebody's dock.
    pub want_xhci: bool,
    pub have_disk: bool,
    pub have_fs: bool,
    pub have_net: bool,
    /// Regions the SMP trampoline must not land on.
    pub keep_out: [(u64, u64); 2],
}

/// The initialisation the suite performs **inline**, spelled once.
///
/// `fd::init_console` and `usermode::init_syscall` are not tests: without them
/// there is no console descriptor and no `IA32_LSTAR`, so ring 3 cannot make a
/// syscall at all. They sit in the middle of [`self_tests`] because the
/// userspace checks after them need both — and that is exactly why this
/// function exists rather than two lines in each of two places.
///
/// **The rule:** anything added to `self_tests` that *initialises* rather than
/// *checks* belongs here, and the `no-tests` build is what would notice if it
/// did not.
fn wire_console_and_syscalls() {
    fd::init_console();
    usermode::init_syscall();
}

/// Bring the machine up to `init` **without** running the suite: the LAPIC, the
/// secondaries, the console fd, the syscall MSRs, the timer, and interrupts on.
///
/// Two callers, and they are the same situation reached two ways:
///
/// * `skiptests` on the command line — a runtime lever, so a trusted build on
///   the bare-metal box does not re-prove demand paging and the ELF loader on
///   every reboot while someone watches a television scroll;
/// * a `no-tests` build, where there is no suite to skip.
///
/// This was the `skiptests` arm of `multiboot2::kmain_mb2`, written out inline,
/// and it is a function because `no-tests` would otherwise have been a *second*
/// copy of it. Every step here is real bring-up that [`self_tests`] happens to
/// also perform on the way past — which is the thing about that function worth
/// remembering, and why its header names the two interleaved init points.
///
/// `secondaries` is a hook rather than an argument list because the trampoline
/// check needs a `keep_out` range that differs by boot protocol: what might be
/// sitting on that page is a PVH start-info block on one path and GRUB's
/// information block or root image on the other. Only the caller knows.
///
/// Interrupts are **on** when this returns. Every test path ends in `cli`; with
/// no suite, nothing else turns them back on before the netpoll daemon and
/// `run_init` need them.
pub fn late_init(secondaries: impl FnOnce()) {
    lapic::init();
    secondaries();
    wire_console_and_syscalls();
    lapic::start_timer();
    // SAFETY: unconditionally safe at ring 0; the IDT is installed and every
    // vector's handler is in place by now.
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
}

/// What [`self_tests`] concluded.
#[cfg(not(feature = "no-tests"))]
pub struct Verdict {
    /// Every check passed.
    ///
    /// **Not a gate on `init`.** Both entry points start the init program
    /// whether or not this is true — see the reversal documented in
    /// `multiboot2::kmain_mb2`. This is a line for the log and for the harness
    /// that greps it, not permission to boot.
    pub passed: bool,
}

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
/// - `fd::init_console` and `usermode::init_syscall` sit *inside* the suite
///   because the userspace tests after them need both. They are spelled once,
///   in [`wire_console_and_syscalls`], which [`late_init`] also calls — see
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
/// sets up. [`late_init`] is that subset, in this order.
#[cfg(not(feature = "no-tests"))]
pub fn self_tests(t: &mut Suite, cx: &SuiteCtx) -> Verdict {
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
    let suite_start_us = crate::net::uptime_us();

    crate::mem::smoke_test(t);
    paging::smoke_test(t);
    // The *other* x86 walker: `akuma-mmu`'s, which the shared crates reach
    // through and C1 folds this kernel onto. Right after `paging::smoke_test`
    // because the two are the same job done twice, and a divergence between
    // them is easiest to read when their results are adjacent.
    crate::uas::smoke_test(t);
    idt::smoke_test(t);
    idt::user_copy_smoke_test(t);
    uaccess::smoke_test(t, cx.smap);

    if t.check("lapic: initialised", lapic::init()) {
        lapic::smoke_test(t);
        // Restart the timer the smoke test stopped: the scheduler wants a live
        // tick to drive NEED_RESCHED.
        lapic::start_timer();
        sched::smoke_test(t);
        sched::block_smoke_test(t);
        lapic::stop_timer();
    }

    crate::pipe::smoke_test(t);
    reboot::smoke_test(t);

    // How many failures were xHCI's, reported rather than acted on: an
    // optional peripheral misbehaving is worth telling apart from the kernel
    // misbehaving when you are reading the log, and it used to gate whether
    // `init` ran at all. It no longer does — see `Verdict::passed`.
    let failures_before_usb = t.failed();
    if cx.have_pci {
        pci::smoke_test(t);
        let have_xhci = cx.want_xhci
            && pci::find_class(0x0c, 0x03).is_some_and(|d| d.header.prog_if == 0x30);
        if cx.want_xhci {
            xhci::smoke_test(t, have_xhci);
        } else {
            t.note("xhci: not requested (pass `usb` or `root=/dev/sda1`)", 0);
        }
    }
    let usb_failures = t.failed() - failures_before_usb;
    if usb_failures > 0 {
        t.note("xhci: of the failures above, this many were USB's", u64::from(usb_failures));
    }

    blk::smoke_test(t, cx.have_disk);
    fs::smoke_test(t, cx.have_fs);
    fd::smoke_test(t, cx.have_fs);
    mm::smoke_test(t);
    net::smoke_test(t, cx.have_net);
    sock::smoke_test(t, cx.have_net);

    wire_console_and_syscalls();
    // The dispatch table itself, before anything runs through it: the
    // legacy-x86 list and the neutral `Syscall` table must stay disjoint,
    // and the x86_64 -> asm-generic hop C1 folds through must still happen.
    usermode::dispatch_smoke_test(t, cx.have_fs);
    usermode::identity_cost_test(t);
    usermode::smoke_test(t);
    usermode::preempt_test(t);
    // The per-core live-L0 registry, here rather than with the rest of
    // `uas::smoke_test` because it asks about switches that have actually
    // happened — see the function's own note.
    crate::uas::live_l0_registry_test(t);

    // The other cores. `nosmp` boots single-core: a bring-up lever, not policy —
    // on a machine whose only console is a framebuffer, `[BKL] stuck` chatter
    // from four cores interleaves into every other line, and taking them out of
    // the picture is the cheapest way to decide whether a fault is cross-core.
    let nosmp = flag("nosmp");
    let expected_aps = if nosmp {
        0
    } else {
        cx.machine
            .madt
            .as_ref()
            .map_or(0, |m| m.cpus().len().saturating_sub(1).min(smp::MAX_CPUS - 1))
    };
    let started = if nosmp {
        crate::serial::puts("  smp:  nosmp on the command line — single core\n");
        0
    } else if smp::trampoline_page_available(cx.machine, &cx.keep_out) {
        smp::start_secondaries(cx.machine.madt.as_ref())
    } else {
        // What might be sitting on the trampoline page differs by protocol: a
        // PVH start-info block, or the information block and root image GRUB
        // left in RAM. Either one there means single core rather than a copy
        // over it — which is why `keep_out` is the caller's to fill in.
        crate::serial::puts("  smp:  trampoline page is not free RAM — single core\n");
        0
    };
    smp::smoke_test(t, expected_aps, started);
    // Right after the secondaries come up: the shootdown's whole termination
    // argument rests on the cores being able to take and acknowledge the IPI,
    // and proving it before any user task exists keeps a later wedge
    // unambiguous. Single core notes and skips rather than failing.
    crate::shootdown::smoke_test(t);
    usermode::smp_parallel_test(t);

    lapic::start_timer();
    usermode::elf_test(t);
    usermode::thread_test(t);
    usermode::fdprobe_test(t);
    usermode::spawn_test(t);
    usermode::wait4_ownership_test(t);
    #[cfg(feature = "console-notify")]
    usermode::console_notify_test(t);
    usermode::busybox_test(t);
    usermode::execve_test(t);

    // `strace` traces the fork test specifically: it is the one that exercises
    // the most syscall surface in the shortest time, and tracing the whole boot
    // buries it.
    let trace_fork = flag("strace");
    if trace_fork {
        usermode::SYSCALL_TRACE.store(true, core::sync::atomic::Ordering::Relaxed);
    }
    usermode::fork_test(t);
    if trace_fork {
        usermode::SYSCALL_TRACE.store(false, core::sync::atomic::Ordering::Relaxed);
    }

    usermode::redirect_test(t);
    lapic::stop_timer();

    // Deliberately here and not beside `mm::smoke_test`: this is the one check
    // that needs real programs to have run first. See its own doc.
    mm::demand_paging_report(t);

    if net::netpoll_drain_selftest(t, cx.have_net) {
        if cx.have_net {
            lapic::start_timer();
            crate::clock::sync_via_sntp();
            lapic::stop_timer();
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
        lapic::start_timer();
        net::netpoll_spawn_selftest(t);
        lapic::stop_timer();
        if flag("netprobe") {
            net::enable_probe();
        }
    }

    lapic::clock_rate_check(t);

    drop(user_ptr_bypass);

    // What the suite's own workload did to the scheduler. Notes rather than
    // checks: the numbers depend on timing and on how much work the boot found
    // to do, so a threshold here would be a flake. `backstop releases` is the
    // one to watch — see `sched::BACKSTOP_US`; a number that grows names a wait
    // whose wake path is missing.
    t.note("sched: parks over the whole suite", sched::blocks());
    t.note("sched: wakes over the whole suite", sched::wakes());
    t.note("sched: backstop releases (0 is the healthy value)", sched::backstop_wakes());

    t.note("suite: guest microseconds elapsed", crate::net::uptime_us().saturating_sub(suite_start_us));
    Verdict { passed: t.report() }
}
