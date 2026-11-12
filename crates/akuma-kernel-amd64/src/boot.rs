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

use akuma_ryzen_amd64::MachineDescription;
use crate::uaccess::SmapStatus;

use crate::{gdt, idt, lapic, paging, sched, smp, uaccess, usermode};

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
    smp::XSAVE_ON.store(smp::enable_xsave(), core::sync::atomic::Ordering::Release);
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
    // The `log` crate's sink, the same one the AArch64 kernel installs
    // (`akuma_kernel_core::klog`). Shared crates (`akuma-exec`, `akuma-elf`)
    // report through `log::warn!`/`info!`; with no logger installed every one
    // of those lines was dropped on this target. Info and above, heap-free.
    akuma_kernel_core::klog::init();
    crate::exec_runtime::init();
    // The wall clock, from the CMOS RTC, before anything is mounted: every
    // inode stamp from here on carries the year. Both boot protocols, for the
    // reason this function exists (below). SNTP corrects it once there is a
    // network; without one this is the clock.
    crate::clock::seed_from_rtc();
    // `/proc/cpuinfo`'s block renderer. Here, not beside `power::init`: that one
    // is called from both boot paths separately, and the metal (multiboot2) path
    // is the one that read bare `processor : N` blocks on 2026-10-09 because the
    // registration had been added to `main.rs` only.
    akuma_vfs_glue::set_cpuinfo_renderer(crate::cpuinfo::render);
    // `/proc/health`'s pipe-table lines: the table is in `akuma-syscalls-glue`,
    // which `akuma-vfs-glue` cannot name.
    akuma_vfs_glue::set_health_extra(crate::pipe::render_health);
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
    // `akuma-syscalls-glue`'s own hook table — distinct from
    // `akuma_vfs_glue::VfsGlueHooks` (which `fs::init_vfs` fills) and from
    // `akuma_syscalls_linux`'s. The AArch64 kernel registers this from
    // `akuma-kernel-glue`, which does not build for x86_64. Only three of the
    // eight fields have a real answer on this target — the wall clock, the core
    // count and the by-number console; the rump five are `false`/no-op because
    // there is no rump kernel here. `utc_time_us` is what glue's folded `utimensat` (4b batch
    // 3c) reads for "both times to now", and `futex`'s absolute-deadline arm;
    // without it `touch` on a fresh file never sets a real mtime even on the
    // metal with SNTP synced — the same "glue reads a hook amd64 never
    // registered" shape batch 3a's prefault gap had.
    akuma_syscalls_glue::set_hooks(akuma_syscalls_glue::SyscallHooks {
        box_is_rump: |_| false,
        mark_box_rump: |_| {},
        attach_server: |_, _| {},
        intercept_box_syscall: |_, _| None,
        rump_socket_readable: |_| false,
        utc_time_us: || crate::clock::is_synced().then(crate::clock::now_us),
        probed_core_count: crate::smp::online_cpus,
        // The **third** real answer, and the newest (4b batch 4b): the serial
        // console, answered by fd number, for the poll family's readiness map.
        // Narrowed to the *unbound* spelling once `crate::console` gave a
        // console process a real `ProcessChannel` — glue's own `Stdin` arm
        // answers a bound descriptor better than this can, because it registers
        // a poller and this has no waker. What it still covers is an unbound
        // fd 0, which is not in the fd table at all and would otherwise poll as
        // `EPOLLHUP | EPOLLERR` (`FdState::Missing`). Same shape as the two
        // above: glue reads a hook, and a hook nobody registered is how batch
        // 3a's prefault gap and 3c's `utc_time_us` gap each looked from ring 3.
        poll_console_state: crate::fd::poll_console_state,
        // The **fourth** real answer: the pointer node (`/dev/input/mouse0`),
        // for the poll family's `DevMouse` arm. `has_events` is a `try_lock`
        // and a `pending()`, so the answer is cheap and IRQ-masked, but no
        // poller rings a bell when a record lands — the readiness re-checks at
        // the blocking-poll tick, like the console hook above.
        dev_mouse_has_events: crate::drivers::pointer_has_events,
        // The fifth: the tap node's gateway queue (`/dev/net/tun`), a relaxed flag the pump refreshes.
        dev_tun_has_frame: crate::drivers::hypervisor_tun_has_frame,
    });
    // This kernel's character devices, behind glue's fd arms (`crate::chardev`).
    crate::chardev::register();
    // Resizing the console's terminal resizes the framebuffer view
    // (`console::resize_view`); every other terminal takes glue's answer.
    akuma_syscalls_glue::term::set_winsize_hook(crate::console::resize_view);
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
    // The untimed-park backstop (4b batch 3a). Every "wait until something
    // happens" arm in the tree parks through
    // `akuma_threading::park_indefinitely`, which is `schedule_blocking(u64::MAX)`
    // until a target says otherwise. This target says otherwise, for the reason
    // `sched::BACKSTOP_US` gives: it reaches its scheduler only by being
    // called, so a missing wake path is an unrecoverable hang here where on
    // AArch64 it is a slow one. Registered here, not in `sched::init`, and for
    // this function's founding reason — a park is reachable from the boot suite
    // on **both** protocols, and the version of this that only PVH set would be
    // green on every rig this laptop can run.
    crate::sched::install_untimed_park_backstop();
    // Demand paging **for the kernel's own reach into user memory** (4b batch
    // 3a). Every `akuma-syscalls-glue` arm validates a user buffer before
    // touching it, and the validation is fail-closed without this: a lazily
    // mapped page — which is what a fresh `mmap` hands ring 3 — reads as
    // `EFAULT`. This kernel's own copy path never needed it (it faults and
    // recovers), which is why the gap survived B1 and was only exposed by
    // folding `read(2)`. See `mm::prefault_user_range`.
    akuma_user_access::set_prefault_hook(crate::mm::prefault_user_range);
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
    /// The same machine in the neutral shape (multiarch M2): what memory
    /// bring-up, the block driver and the trampoline check read.
    pub facts: &'a akuma_machine_facts::Facts,
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
/// `console::init` and `usermode::init_syscall` are not tests: without them
/// there is no console descriptor and no `IA32_LSTAR`, so ring 3 cannot make a
/// syscall at all. They sit in the middle of [`self_tests`] because the
/// userspace checks after them need both — and that is exactly why this
/// function exists rather than two lines in each of two places.
///
/// **The rule:** anything added to `self_tests` that *initialises* rather than
/// *checks* belongs here, and the `no-tests` build is what would notice if it
/// did not.
pub fn wire_console_and_syscalls() {
    // The console's `ProcessChannel` and its shared line discipline, brought up
    // at exactly the point `fd::init_console` used to be. That function, and
    // the `TerminalState` it owned, are gone: `akuma-syscalls-glue`'s
    // `Stdin`/`DevTty` arm serves fd 0 now and reaches the line discipline
    // through `Process::terminal_state`, which for a console process **is** the
    // cell this creates. Two line disciplines for one serial line was the old
    // shape and it could not express raw mode, because `ioctl` wrote neither.
    //
    // The *pump* is spawned later (`usermode::run_init`) — nothing produces
    // console input until there is a ring-3 process that could read it.
    crate::console::init();
    usermode::init_syscall();

    // Diagnostic (trash box, uncommitted): the AArch64 kernel turns the BKL-hold
    // profiler on from glue's kernel_main under `kernel_bkl_profile`; amd64 has
    // no such call site, so every `[BKL] stuck` line on the metal read
    // `tag=511` (HOLD_TAG_UNKNOWN) and named nothing. This function is the one
    // bring-up step both entry points spell once — the suite path and
    // `late_init` — so the attribution is on whichever way the box came up.
    akuma_bkl::sync::set_profiling(true);
    crate::serial::puts("[BKLPROF] enabled (amd64 diagnostic)\n");
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
    // Unconditionally safe at ring 0; the IDT is installed and every
    // vector's handler is in place by now.
    akuma_cpu::interrupts::on();
}

/// Bring up the other cores, or say why not. Returns how many started.
///
/// The one copy of a sequence that was written three times — in [`self_tests`], in the PVH
/// `kmain`'s no-tests arm and in the multiboot2 `start_secondaries` closure — and had drifted:
/// the suite printed why it stayed single-core, the other two stayed silent. `nosmp` is a
/// bring-up lever (a framebuffer console interleaves `[BKL] stuck` chatter from four cores into
/// every line); `smp=N` caps the count; `keep_out` is the caller's to fill because what might sit
/// on the trampoline page differs by protocol (a PVH start-info block, or GRUB's information
/// block and root image).
// Its result is informational; callers that cannot act on it ignore it.
#[allow(clippy::must_use_candidate)]
pub fn bring_up_secondaries(
    madt: Option<&akuma_ryzen_amd64::acpi::Madt>,
    facts: &akuma_machine_facts::Facts,
    cmdline: &str,
    keep_out: &[(u64, u64)],
) -> usize {
    smp::set_cpu_cap_from_cmdline(cmdline);
    if cmdline.split_ascii_whitespace().any(|t| t == "nosmp") {
        crate::serial::puts("  smp:  nosmp on the command line — single core\n");
        0
    } else if smp::trampoline_page_available(facts, keep_out) {
        smp::start_secondaries(madt)
    } else {
        crate::serial::puts("  smp:  trampoline page is not free RAM — single core\n");
        0
    }
}

/// The command-line policies that need no hardware, set once for both boot paths: the `smp=N`
/// cap and the idle wake budget (`idlesleep[=N]`, `idlemwait`).
///
/// Both entry points call this right after the command line is in hand. Until 2026-11-10 the PVH
/// `kmain` set the idle policy early and the multiboot2 path set it only inside its
/// `start_secondaries` closure, which the full-suite boot never runs — so `idlesleep` was silently
/// ignored by a GRUB-booted tests kernel.
pub fn apply_cmdline_policies(cmdline: &str) {
    smp::set_cpu_cap_from_cmdline(cmdline);
    crate::sched::set_idle_policy_from_cmdline(cmdline);
}

/// The value of a `prefix=value` token on the command line, or `None`.
///
/// Pure and allocation-free: the one parse of a valued boot flag, over the
/// `&str` both boot protocols now hand the shared code (PVH copies it out of the
/// start-info block, multiboot2 gets it directly).
#[must_use]
pub fn token_value<'a>(cmdline: &'a str, prefix: &str) -> Option<&'a str> {
    cmdline.split_ascii_whitespace().find_map(|t| t.strip_prefix(prefix))
}

/// Keep the boot command line (`akuma_params::capture`), once, for both entry points: what
/// `init=`/`initargs=` are read from at the end of the boot (`akuma_params::init`, shared with
/// the AArch64 kernel). A failure is reported and leaves the line empty: the defaults apply.
pub fn keep_cmdline(cmdline: &str) {
    let r = akuma_params::capture(|buf| {
        if cmdline.len() > buf.len() {
            return Err(akuma_params::Failure::TooLong);
        }
        buf[..cmdline.len()].copy_from_slice(cmdline.as_bytes());
        Ok(cmdline.len())
    });
    if r.is_err() {
        crate::serial::puts("  [params] the command line was not kept: init= and initargs= are ignored\n");
    }
}

/// Apply the boot flags that steer the running kernel rather than the boot:
/// `fbverbose`, `fbtrace`, `strace`, `strace_err`, `strace_pid=`, `strace_nr=`,
/// `nopidfd`.
///
/// One function for both entry points. Until 2026-11-10 the PVH `kmain` parsed
/// all seven and the multiboot2 `boot_to_init` parsed two, so `strace` on the
/// GRUB-booted metal silently did nothing — the same "registered from one
/// `kmain` and not the other" failure `install_shared_sinks` exists for.
pub fn apply_runtime_flags(cmdline: &str) {
    use core::sync::atomic::Ordering::Relaxed;
    let has = |w: &str| cmdline.split_ascii_whitespace().any(|t| t == w);
    crate::serial::set_fb_verbose(has("fbverbose"));
    crate::multiboot2::set_fb_trace(has("fbtrace"));
    if has("strace") {
        crate::usermode::SYSCALL_TRACE.store(true, Relaxed);
    }
    if has("strace_err") {
        crate::usermode::SYSCALL_TRACE_ERRORS.store(true, Relaxed);
    }
    if has("nopidfd") {
        crate::usermode::NO_PIDFD.store(true, Relaxed);
    }
    if let Some(n) = token_value(cmdline, "strace_pid=").and_then(|v| v.parse::<u64>().ok()) {
        crate::usermode::SYSCALL_TRACE_PID.store(n, Relaxed);
        crate::usermode::SYSCALL_TRACE.store(true, Relaxed);
    }
    if let Some(list) = token_value(cmdline, "strace_nr=") {
        crate::usermode::trace_nrs_from(list);
    }
}

/// How long the boot waits for DHCP before giving up on a wall clock.
///
/// Generous, because on the Realtek machine the receiver has to be restarted
/// once before anything arrives (`akuma-net-nic`'s `rtl8169` stall recovery).
/// A boot that waits eight seconds and gets a clock is worth more than one that
/// gives up in two and cannot verify a certificate.
pub const SETTLE_BUDGET_MS: u64 = 8_000;

/// DHCP, then the wall clock (SNTP), in the only order that works: SNTP needs
/// an address and a route. Both boot paths run this; the netpoll daemon, which
/// must come last (see `net::settle_for_dhcp`), is the caller's next line.
pub fn dhcp_then_clock() {
    if crate::net::settle_for_dhcp(SETTLE_BUDGET_MS) {
        crate::clock::sync_via_sntp();
    } else {
        crate::serial::puts("  net:  DHCP did not settle; no wall clock yet (SNTP will retry)\n");
    }
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

/// The boot suite: `akuma-kernel-tests-amd64`'s `self_tests`, registered by the bin's
/// `before_main` hook.
///
/// The suite lives in its own crate (multiarch, the tests crate step) and this crate cannot
/// depend on it, so both entry points reach it through this one function pointer. It builds
/// the [`Suite`](akuma_selftest::Suite) itself (over [`suite_emit`]) and returns the verdict.
///
/// # Compiled out by `no-tests`
///
/// [`self_tests`] is not purely a suite: it calls [`wire_console_and_syscalls`] and brings up
/// the secondaries, and a build without it still needs what it sets up. [`late_init`] is that
/// subset, in this order.
#[cfg(not(feature = "no-tests"))]
pub type SelfTests = fn(&SuiteCtx) -> Verdict;

#[cfg(not(feature = "no-tests"))]
static SELF_TESTS: akuma_primitives::Registered<SelfTests> = akuma_primitives::Registered::new(
    "akuma-kernel-amd64: boot suite not registered — the bin must be \
     `akuma_kernel::entry!(akuma_kernel_tests_amd64::register)`",
);

/// Install the boot suite. Idempotent by `Registered`'s contract: a second call is ignored.
#[cfg(not(feature = "no-tests"))]
pub fn register_self_tests(suite: SelfTests) {
    SELF_TESTS.register(suite);
}

/// Run the registered boot suite. One list, one order, both boot paths.
#[cfg(not(feature = "no-tests"))]
pub fn self_tests(cx: &SuiteCtx) -> Verdict {
    (SELF_TESTS.require())(cx)
}

#[cfg(not(feature = "no-tests"))]
/// The suite's console emit, with a BKL drop at every check boundary.
///
/// The boot thread runs the whole suite as one kernel stretch, and the BSP
/// owns the BKL for all of it (owner never clears — every BSP acquire after
/// the first is the reentrant fast path). Measured 2026-09-20 with the
/// `akuma-bkl` wait-state sampler: cores 2-4 ticketed behind one BSP hold and
/// spun 2M/4M/16.7M iterations while `netprobe`/netpoll brought the stack up —
/// ~22 s of suite — and any userspace thread woken in that window (the litter
/// raft child) starves with no core to run on. Dropping at every check
/// boundary caps the BSP's hold to one check's length; the cost is one
/// release/re-ticket per printed line, on a path that is prints-dominated
/// anyway.
pub fn suite_emit(s: &str) {
    crate::serial::puts(s);
    crate::smp::bkl_drop_window();
}

