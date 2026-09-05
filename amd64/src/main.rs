//! amd64 kernel entry.
//!
//! Scope, deliberately narrow: bring an x86_64 machine from the multiboot
//! handoff to executing Rust in long mode with a working console, then run
//! whatever arch-neutral crate logic can be reached from here. There is no
//! userspace, no interrupt handling, no scheduler and no MMU management beyond
//! the identity map `boot.s` builds — those arrive as the crates that own them
//! stop assuming AArch64.
//!
//! What this file is really measuring is the claim in
//! `docs/archive/REDUCING_PLATFORM_DEPENDENCY.md`: that 81.7% of the tree's
//! production code is already architecture-neutral. Every crate that boots
//! usefully from here is evidence for it; every crate that cannot is a seam
//! the proposal has to name.

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![feature(abi_x86_interrupt)]

extern crate alloc;

/// This package is x86_64-only, and says so in one line rather than in five.
///
/// It is deliberately absent from `default-members`, so `cargo build` at the
/// repo root (which targets `aarch64-unknown-none`) never reaches it. A
/// `--workspace` invocation does, though, and without this guard the failure is
/// a pile of "invalid register `dx`" and "att_syntax is only supported on x86"
/// from `boot.s` — which reads like the amd64 port is broken rather than like
/// the target was wrong.
///
/// Not solved with cargo's `per-package-target` / `forced-target`, which is the
/// mechanism actually designed for this: it is an unstable cargo feature and
/// `cargo-features` is only accepted in the workspace root manifest. Putting it
/// there would make the root manifest nightly-cargo-only, and this tree builds
/// itself inside the guest (`acceptance/10`) where that is a risk with no
/// upside.
#[cfg(not(target_arch = "x86_64"))]
compile_error!(
    "akuma-amd64 is an x86_64 target: build it with \
     `cargo build -p akuma-amd64 --target x86_64-unknown-none` (or amd64/run.sh)"
);

#[cfg(target_arch = "x86_64")]
mod blk;
#[cfg(target_arch = "x86_64")]
mod clock;
#[cfg(target_arch = "x86_64")]
mod dns;
#[cfg(target_arch = "x86_64")]
mod fd;
#[cfg(target_arch = "x86_64")]
mod fs;
#[cfg(target_arch = "x86_64")]
mod gdt;
#[cfg(target_arch = "x86_64")]
mod usermode;
#[cfg(target_arch = "x86_64")]
mod idt;
#[cfg(target_arch = "x86_64")]
mod input;
#[cfg(target_arch = "x86_64")]
mod kbd;
#[cfg(target_arch = "x86_64")]
mod lapic;
#[cfg(target_arch = "x86_64")]
mod machine;
#[cfg(target_arch = "x86_64")]
mod loader;
#[cfg(target_arch = "x86_64")]
mod mem;
#[cfg(target_arch = "x86_64")]
mod mm;
/// The GRUB/multiboot2 entry and its framebuffer console — the bare-metal way
/// in, used when there is no VMM and no serial port.
mod multiboot2;
#[cfg(target_arch = "x86_64")]
mod net;
#[cfg(target_arch = "x86_64")]
mod paging;
/// PCI enumeration — how a bare-metal boot finds the USB controllers, the NIC
/// and the disk that a VMM would otherwise have announced.
#[cfg(target_arch = "x86_64")]
mod pci;
/// The `reboot(2)` syscall and the x86 machine reset under it.
#[cfg(target_arch = "x86_64")]
mod reboot;
/// A span of RAM as a block device, so a machine with no storage driver can
/// still mount the root filesystem its boot loader left in memory.
mod ramdisk;
#[cfg(target_arch = "x86_64")]
mod phys;
#[cfg(target_arch = "x86_64")]
mod pipe;
#[cfg(target_arch = "x86_64")]
mod port;
#[cfg(target_arch = "x86_64")]
mod sched;
#[cfg(target_arch = "x86_64")]
mod serial;
#[cfg(target_arch = "x86_64")]
mod smp;
#[cfg(target_arch = "x86_64")]
mod sock;
mod uaccess;

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(include_str!("boot.s"), options(att_syntax));

/// The kernel heap.
///
/// `#[global_allocator]` is a binary-level declaration, so it lives here rather
/// than in `akuma-alloc` — exactly as `src/main.rs` does it for the aarch64
/// kernel. The crate exports the implementation; a binary installs it.
#[cfg(target_arch = "x86_64")]
#[global_allocator]
static ALLOCATOR: akuma_alloc::KernelAllocator = akuma_alloc::KernelAllocator;

/// Out of memory.
///
/// The aarch64 kernel kills the faulting process here. This target has no
/// processes, so there is nothing to kill and panicking is the honest response.
#[cfg(target_arch = "x86_64")]
#[alloc_error_handler]
fn alloc_error_handler(layout: core::alloc::Layout) -> ! {
    serial::puts("\n[OOM] allocation of ");
    serial::put_dec(layout.size() as u64);
    serial::puts(" bytes failed\n");
    halt();
}

/// Long-mode entry, called from `boot.s` with the `hvm_start_info` pointer.
///
/// `extern "C"` and `#[unsafe(no_mangle)]` because the far-jumped-to assembly
/// resolves it by symbol name; the argument arrives in `%rdi` per System V,
/// having come in from the PVH ABI's `%ebx`.
#[cfg(target_arch = "x86_64")]
#[unsafe(no_mangle)]
pub extern "C" fn kmain(hvm_start_info: u64) -> ! {
    serial::init();

    serial::puts("\n");
    serial::puts("Akuma/amd64 — long mode reached\n");
    serial::puts("  uart: ");
    serial::puts(if serial::present() { "present" } else { "absent (reads report no data)" });
    serial::puts("  kbd: ");
    serial::puts(if kbd::init() { "i8042 present\n" } else { "no i8042\n" });
    serial::puts("  hvm_start_info @ 0x");
    serial::put_hex(hvm_start_info);
    serial::puts("\n");

    // Descriptor tables first, and the ORDER HERE IS LOAD-BEARING.
    //
    // `boot.s` builds its GDT in the low boot region, because the 32-bit
    // trampoline has to reach it with paging off. The CPU reads the GDT on every
    // exception delivery — it loads CS from the IDT entry's selector — so once
    // the identity map is dropped, that GDT is unmapped and *any* fault becomes
    // a triple fault. The first #PF cannot be delivered, which raises #DF, whose
    // delivery faults for the same reason.
    //
    // `gdt::init` rebuilds the table in the kernel's own high .bss, so GDTR
    // points somewhere that survives. It must therefore run before
    // `drop_identity_map`, not after — which is where it used to be, and the
    // symptom was a triple fault inside the heap allocator with CR2 pointing at
    // 0x2010d8: the GDT itself.
    gdt::init();

    // The BSP's per-CPU block and the Big Kernel Lock, before anything reads
    // `gs:` — which the scheduler, the syscall path and every `SAFETY: under
    // the BKL` comment below do. Nothing else runs yet, so the lock is taken
    // uncontended; every kernel task is born holding it.
    smp::init_bsp();

    // `idt::init` needs nothing but its own static table, so it goes here rather
    // than after the memory subsystem: a fault during memory bring-up then
    // prints a diagnosis instead of vanishing.
    idt::init();

    // SMAP/SMEP, right after the IDT: from here on a kernel-mode touch of a user
    // page without `stac` is a fault the handler can report, and every user
    // access below goes through `uaccess`, which brackets its copies.
    let smap = uaccess::init_smap();
    serial::puts("  smap: ");
    serial::puts(if smap.cpuid_smap { "on" } else { "off (CPUID lacks SMAP)" });
    serial::puts("  smep: ");
    serial::puts(if smap.cpuid_smep { "on\n" } else { "off (CPUID lacks SMEP)\n" });

    // The trampoline's identity map has done its job: the kernel is executing
    // from its high linked address, its stack is in the physmap, and both
    // descriptor tables are now high. Dropping it hands the lower half to
    // userspace.
    paging::drop_identity_map();

    // Read the machine's description of itself. After `drop_identity_map`
    // because every read goes through the physmap, and after `idt::init` because
    // a VMM-supplied pointer that escapes the bounds check should fault
    // reportably rather than triple-fault.
    let machine = machine::describe(hvm_start_info);
    machine::report(&machine);

    // PCI enumeration. Pure port I/O, so it needs nothing but the console —
    // before `mem::init` is fine. On this (VMM) path it finds nothing, which is
    // correct: the devices are virtio-MMIO. It is here so the one code path
    // runs on both entries.
    pci::scan();
    pci::report();

    if !mem::init(&machine) {
        serial::puts("\nAkuma/amd64 — memory bring-up FAILED\n");
        halt();
    }

    // Give the shared crates a console. `safe_print!` discards output until a
    // hook is registered, so without this every diagnostic `akuma-virtio` emits
    // — including the one naming why a device failed to initialise — is silently
    // dropped. One line, and it is the difference between a driver that reports
    // and a driver that goes quiet.
    akuma_primitives::console::set_print_hook(serial::puts);

    // Block devices, after the heap (the virtio HAL allocates DMA buffers from
    // it) and after the IDT (a bad transport address should fault reportably).
    let have_disk = blk::init(&machine.virtio);
    // The filesystem, on top of that disk. Both are best-effort: a machine with
    // no drive still boots, which is what `DISK=none` and every stage before
    // Stage M did.
    let have_fs = have_disk && fs::mount_root();

    // Networking, after the heap (the stack allocates) and after the virtio
    // window is set (the NIC is another slot in the same array the disk came
    // from). DHCP on: both machines run a server — QEMU's user-mode stack, and
    // dnsmasq on the Firecracker host (`amd64/net-setup.sh`).
    let have_net = net::init(true);

    let mut t = akuma_selftest::Suite::new("Akuma/amd64 self-test", serial::puts);

    // The self-tests below drive syscall bodies with kernel-stack buffers where
    // a program would pass user pointers. `uaccess` refuses kernel addresses —
    // that is its job — so the tests run inside the same bypass window the
    // AArch64 kernel's boot tests use. Dropped before the verdict: `run_init`
    // runs a real program, and its bad pointers must be EFAULT.
    let user_ptr_bypass = akuma_user_access::BypassValidationGuard::new();

    mem::smoke_test(&mut t);
    paging::smoke_test(&mut t);
    // The IDT is already loaded (before mem::init, so faults there are
    // visible). Demand paging is what needs the PMM, and that is only exercised
    // here.
    idt::smoke_test(&mut t);
    // The user-copy fault recovery, after demand paging is known-good: it is
    // the one path on which a kernel-mode #PF is not fatal, and the test takes
    // three of them on purpose.
    idt::user_copy_smoke_test(&mut t);
    uaccess::smoke_test(&mut t, smap);

    if t.check("lapic: initialised", lapic::init()) {
        lapic::smoke_test(&mut t);
        // Restart the timer the smoke test stopped: the scheduler wants a live
        // tick to drive NEED_RESCHED.
        lapic::start_timer();
        sched::smoke_test(&mut t);
        lapic::stop_timer();
    }

    pci::smoke_test(&mut t);
    reboot::smoke_test(&mut t);
    blk::smoke_test(&mut t, have_disk);
    fs::smoke_test(&mut t, have_fs);
    fd::smoke_test(&mut t, have_fs);
    mm::smoke_test(&mut t);
    net::smoke_test(&mut t, have_net);
    sock::smoke_test(&mut t, have_net);

    fd::init_console();
    usermode::init_syscall();
    usermode::smoke_test(&mut t);
    usermode::preempt_test(&mut t);

    // The other cores. After the two ring-3 tests above, which assert an exact
    // interleaving that only one core produces, and before the ELF, spawn,
    // busybox, execve and fork tests below, which then run with every core
    // picking up processes — the best stress the BKL gets short of a shell.
    // The BSP's timer is stopped here (`preempt_test` stops it), which
    // `start_secondaries` needs for its INIT delay.
    let expected_aps = machine
        .madt
        .as_ref()
        .map_or(0, |m| m.cpus().len().saturating_sub(1).min(smp::MAX_CPUS - 1));
    // The VMM's boot structures all sit below the trampoline page on both
    // machines (`smp::AP_TRAMPOLINE_PA`); this is the check that says so rather
    // than the comment that assumes it.
    let si = &machine.start_info;
    let keep_out = [
        (si.addr, si.addr + 4096),
        (si.cmdline_paddr, si.cmdline_paddr + 4096),
        (si.memmap_paddr, si.memmap_paddr + 4096),
    ];
    let started = if smp::trampoline_page_available(&machine, &keep_out) {
        smp::start_secondaries(machine.madt.as_ref())
    } else {
        serial::puts("  smp:  trampoline page is not free RAM — single core\n");
        0
    };
    smp::smoke_test(&mut t, expected_aps, started);
    usermode::smp_parallel_test(&mut t);
    // Last, because it is the only test whose program the kernel did not
    // assemble: everything before it has to work for a loader failure to be
    // readable as a loader failure.
    lapic::start_timer();
    usermode::elf_test(&mut t);
    usermode::fdprobe_test(&mut t);
    usermode::spawn_test(&mut t);
    usermode::busybox_test(&mut t);
    usermode::execve_test(&mut t);
    // `strace` on the command line traces the fork test's syscalls too — the one
    // self-test whose failure mode under SMP was a silent hang, where the trace
    // is the only evidence of which core did what.
    let trace_fork = machine::flag(hvm_start_info, "strace");
    if trace_fork {
        usermode::SYSCALL_TRACE.store(true, core::sync::atomic::Ordering::Relaxed);
    }
    usermode::fork_test(&mut t);
    if trace_fork {
        usermode::SYSCALL_TRACE.store(false, core::sync::atomic::Ordering::Relaxed);
    }
    lapic::stop_timer();

    // The netpoll drain half, then (in the gap before the daemon task
    // exists) the wall clock, then the spawn half — see `net::
    // netpoll_selftest`'s doc for exactly why the clock sync has to run
    // between these two rather than after both, and `clock.rs`'s own header
    // for why this is best-effort (no `t.check`) rather than something to
    // fail the boot over. The daemon is left running once spawned (that is
    // the point; `run_init` needs it), so all of this must come after the
    // leak and preemption checks above.
    if net::netpoll_drain_selftest(&mut t, have_net) {
        if have_net {
            // `start_timer`/`stop_timer`, same idiom as every other phase
            // above that needs real elapsed time: `net::uptime_us` reads
            // `lapic::ticks()`, which does not advance while the timer is
            // stopped (true here since line 227), and `akuma_sntp::boot::
            // bootstrap_over_udp`'s own timeout depends on it moving —
            // without a live tick, an unanswered request would spin forever
            // instead of giving up.
            lapic::start_timer();
            clock::sync_via_sntp();
            lapic::stop_timer();
        }
        net::netpoll_spawn_selftest(&mut t);
    }

    drop(user_ptr_bypass);

    // The verdict is `#[must_use]`, and this is why: before the harness existed
    // a `[FAIL]` printed and the boot went on to announce success.
    let passed = t.report();
    if passed {
        serial::puts("Akuma/amd64 — all self-tests passed\n");
    } else {
        serial::puts("Akuma/amd64 — SELF-TESTS FAILED\n");
    }

    // Hand the console to a shell, if one is on the disk. After the verdict, so
    // an interactive session never hides a failing boot — and only on a passing
    // one, because a shell on a kernel whose own tests failed is a way to spend
    // an hour debugging the wrong layer.
    if passed && have_fs {
        let mut init_buf = [0u8; 128];
        let mut args_buf = [0u8; 256];
        // Copy the path out first: `init_path` and `init_args` both borrow a
        // fresh parse of the command line, so they cannot both be live.
        let mut path_store = [0u8; 128];
        let path_len = machine::init_path(hvm_start_info, &mut init_buf)
            .map(|p| {
                let n = p.len().min(path_store.len());
                path_store[..n].copy_from_slice(&p.as_bytes()[..n]);
                n
            });
        if let Some(path_len) = path_len {
            let path = core::str::from_utf8(&path_store[..path_len]).unwrap_or("");
            let args_str = machine::init_args(hvm_start_info, &mut args_buf).unwrap_or("");
            let args: alloc::vec::Vec<&str> =
                args_str.split(',').filter(|s| !s.is_empty()).collect();
            // Leave the timer running for the init program: it drives preemption
            // (so a busy server cannot starve the netpoll daemon) and advances
            // the clock `akuma-net`'s wait deadlines are measured against. The
            // self-tests stop it between stages; a shell or server wants it on.
            if have_net {
                lapic::start_timer();
            }
            if machine::flag(hvm_start_info, "strace") {
                usermode::SYSCALL_TRACE.store(true, core::sync::atomic::Ordering::Relaxed);
            }
            usermode::run_init(path, &args);
        }
    }

    halt();
}

/// Park the core forever with interrupts masked.
///
/// `hlt` in a loop rather than a bare spin: it is the x86 counterpart of the
/// `wfi` that `akuma_cpu::park_core` emits on AArch64, and burning a host core
/// at 100% is how a QEMU run gets mistaken for a hang.
pub fn halt() -> ! {
    // A core that stops must not take the Big Kernel Lock with it: the others
    // keep running (a fault on one core is reported, not spread), and after a
    // failed verdict they idle quietly instead of spinning in their tick
    // handlers. Before `smp::init_bsp` the lock is free and this is a no-op —
    // but it reads `gs:`, so it is gated on the block being installed.
    #[cfg(target_arch = "x86_64")]
    if smp::percpu_installed() {
        smp::bkl_abandon();
    }
    loop {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: `cli` and `hlt` are unconditionally safe to execute at ring 0
        // and this function never returns, so masking interrupts permanently is
        // the intent rather than a leaked side effect.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
        #[cfg(not(target_arch = "x86_64"))]
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    #[cfg(target_arch = "x86_64")]
    {
        serial::puts("\n[PANIC] ");
        if let Some(loc) = info.location() {
            serial::puts(loc.file());
            serial::puts(":");
            serial::put_dec(u64::from(loc.line()));
        } else {
            serial::puts("<no location>");
        }
        serial::puts("\n");
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = info;
    halt();
}
