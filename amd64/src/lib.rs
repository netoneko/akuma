//! The amd64 kernel: everything but its two entry symbols.
//!
//! The bin (`amd64/bin`, package `akuma-amd64`) is `akuma_kernel::entry!()`, which
//! defines `kmain` (PVH) and `kmain_mb2` (multiboot2), the symbols `akuma-boot-amd64`'s
//! `boot.s` jumps to, and calls [`kmain`] / [`kmain_mb2`] here. The rest of the kernel (the
//! allocator, the panic handler, every subsystem) is this library, which `akuma-kernel`
//! selects on `x86_64-unknown-none` the way `akuma-tokio-kernel` selects its machine
//! halves (`docs/archive/AKUMA_ARCH_CONVERGENCE_RESEARCH.md` §4, option C).

#![no_std]
#![feature(alloc_error_handler)]
#![feature(abi_x86_interrupt)]
// A `no-tests` build has no boot suite, and the suite is the **only** caller of
// a number of ordinary kernel accessors — `paging::translate`, `unmap_page`,
// `lapic::stop_timer`, `sched::{preemptions, blocks, wakes, is_blocked}`,
// `fs::mount_count` and friends. Those are the kernel's own surface, not test
// code: gating them behind `not(feature = "no-tests")` would file them in the
// test bucket, which is a lie about what they are, and deleting them would
// throw away API the tested build exercises on every boot.
//
// So: allow them to be unreferenced in this configuration, and gate only what
// exists *for* the suite (the hand-assembled user programs, the embedded probe
// ELFs, the test-process helpers, the independent ELF re-reader). That split is
// what makes `scripts/cloc_akuma.py` honest here rather than merely flattering.
#![cfg_attr(feature = "no-tests", allow(dead_code, unused_imports))]

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
    "akuma-kernel-amd64 is an x86_64 target: build the kernel with \
     `cargo build -p akuma-amd64 --target x86_64-unknown-none` (or amd64/run.sh)"
);

#[cfg(target_arch = "x86_64")]
mod banner;
#[cfg(target_arch = "x86_64")]
mod blk;
mod boot;
#[cfg(target_arch = "x86_64")]
mod clock;
/// The serial console as a `ProcessChannel` — the producer `akuma-syscalls-glue`'s
/// `Stdin`/`DevTty` arm reads, and the drain for the echo it writes back.
#[cfg(target_arch = "x86_64")]
mod console;
/// The `akuma-exec` runtime + config tables this target registers — the
/// prerequisite for any syscall served by `akuma-syscalls-glue` (C1 step 3).
#[cfg(target_arch = "x86_64")]
mod exec_runtime;
#[cfg(target_arch = "x86_64")]
mod fd;
/// `/dev/fb0`: the boot framebuffer as a Linux fbdev device.
#[cfg(target_arch = "x86_64")]
mod fbdev;
#[cfg(target_arch = "x86_64")]
mod fs;
/// `futex(2)`: the effects half, over `akuma-syscalls-sync`'s decisions.
mod futex;
#[cfg(target_arch = "x86_64")]
mod gdt;
/// The per-class driver seam and the services drivers may use.
///
/// `crates/akuma-drivers-amd64` registers into it; the machine half never names a driver.
#[cfg(target_arch = "x86_64")]
pub mod drivers;
#[cfg(target_arch = "x86_64")]
mod usermode;
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
mod nvme;
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
mod cpuinfo;
mod power;
#[cfg(target_arch = "x86_64")]
mod pipe;
mod polltime;
#[cfg(target_arch = "x86_64")]
mod sched;
#[cfg(target_arch = "x86_64")]
mod serial;
#[cfg(target_arch = "x86_64")]
mod signal;
#[cfg(target_arch = "x86_64")]
mod smp;
#[cfg(target_arch = "x86_64")]
mod shootdown;
/// One frame per page of a writable `MAP_SHARED` file mapping, shared by every
/// mapper — `akuma-fpcache-rw`'s table, locked and refcounted.
#[cfg(target_arch = "x86_64")]
mod shmpages;
#[cfg(target_arch = "x86_64")]
mod sock;
/// `clone(CLONE_VM|CLONE_THREAD)`: threads sharing one address space.
mod thread;
mod uaccess;
#[cfg(target_arch = "x86_64")]
mod splash;
/// The boot suite's view of this crate (`crates/akuma-kernel-tests-amd64`): one
/// re-export per item it reaches. Not an API — see the module.
#[cfg(all(target_arch = "x86_64", not(feature = "no-tests")))]
#[doc(hidden)]
pub mod testing;
mod watchdog;
mod xhci;

#[cfg(target_arch = "x86_64")]
pub use multiboot2::kmain_mb2;
/// For `akuma_kernel::entry!`, which wraps `kmain_mb2`'s raw argument.
#[doc(hidden)]
pub use akuma_primitives::addr::PhysAddr;

/// The kernel heap.
///
/// One `#[global_allocator]` per linked image: `akuma-alloc` exports the
/// implementation and the kernel installs it, here rather than in the bin
/// because the bin is only the entry symbols.
#[cfg(target_arch = "x86_64")]
#[global_allocator]
static ALLOCATOR: akuma_alloc::KernelAllocator = akuma_alloc::KernelAllocator;

/// Out of memory.
///
/// The aarch64 kernel kills the faulting process here. This target cannot yet
/// (its allocation failures come from `fd.rs`'s whole-file cache, which C2
/// deletes), so the honest response is to stop this core — but never with the
/// Big Kernel Lock held: `halt()`'s abandon is owner-checked behind the
/// `percpu_installed` gate, so release first, unconditionally. The allocation
/// that OOMs here ran while this core held the lock (every fd write path does),
/// and a halted owner leaves every peer spinning in `[BKL] stuck` forever —
/// the bare-metal signature-B ssh lockout
/// (`docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md`). Releasing it degrades the machine
/// to N-1 cores instead of stopping it.
#[cfg(target_arch = "x86_64")]
#[alloc_error_handler]
fn alloc_error_handler(layout: core::alloc::Layout) -> ! {
    serial::puts("\n[OOM] allocation of ");
    serial::put_dec(layout.size() as u64);
    serial::puts(" bytes failed\n");
    smp::bkl_abandon();
    halt();
}

/// The PVH boot, from long mode on, with the `hvm_start_info` pointer.
///
/// The bin's `kmain` symbol, which `boot.s` calls by name, calls this.
#[cfg(target_arch = "x86_64")]
pub fn kmain(hvm_start_info: u64, before_main: fn()) -> ! {
    // The bin's hook (`akuma_kernel::entry!`'s argument), as on the multiboot2 path: it
    // registers the boot suite (`akuma-kernel-tests-amd64`) and touches nothing else.
    before_main();
    serial::init();

    // The print hook, before anything prints through a shared crate:
    // `akuma-boot-amd64`'s mem bring-up (this kernel's `mem::init`) reports
    // through `safe_print!`, so the hook must be live by then. Same sink as
    // `boot::install_shared_sinks` registers (idempotent re-store later).
    akuma_primitives::console::set_print_hook(serial::puts);

    serial::puts("\n");
    serial::puts("Akuma/amd64 — long mode reached\n");
    serial::puts("  uart: ");
    serial::puts(if serial::present() { "present" } else { "absent (reads report no data)" });
    serial::puts("  kbd: ");
    serial::puts(if kbd::init() { "i8042 present\n" } else { "no i8042\n" });
    serial::puts("  hvm_start_info @ 0x");
    serial::put_hex(hvm_start_info);
    serial::puts("\n");

    // Descriptor tables, per-CPU block, IDT, SMAP/SMEP/WP, the scheduler, and
    // dropping the identity map — six steps whose ORDER IS LOAD-BEARING, shared
    // with the multiboot2 entry point. `boot::early_init` has the reason for
    // each one; they were written out twice until 2026-09-07, which is how
    // `sched::init()` came to need adding by hand in two places.
    let smap = boot::early_init();
    serial::puts("  smap: ");
    serial::puts(if smap.cpuid_smap { "on" } else { "off (CPUID lacks SMAP)" });
    serial::puts("  smep: ");
    serial::puts(if smap.cpuid_smep { "on\n" } else { "off (CPUID lacks SMEP)\n" });

    // The command line, copied once into a frame-local buffer. Everything that
    // reads a boot flag below — and inside `boot::self_tests` — reads it from
    // here, so this path and the multiboot2 one answer the same flag the same
    // way rather than each parsing the line its own way.
    let mut cmdline_buf = [0u8; 512];
    let cmdline = machine::cmdline(hvm_start_info, &mut cmdline_buf);

    // Read the machine's description of itself. After `drop_identity_map`
    // because every read goes through the physmap, and after `idt::init` because
    // a VMM-supplied pointer that escapes the bounds check should fault
    // reportably rather than triple-fault.
    let machine = machine::describe(hvm_start_info);
    machine::report(&machine);
    // The neutral view (multiarch M2), built once: memory bring-up, the block
    // driver and the AP trampoline check read this, not the PVH description.
    let facts = machine.facts(akuma_machine_facts::Provenance::Pvh);
    machine::report_facts(&facts);

    // No PCI enumeration on this path. PVH means a VMM, and the VMMs this
    // target runs under (Firecracker, QEMU `microvm`) present every device as
    // virtio-MMIO — there is no PCI bus to walk. Worse, Firecracker does not
    // emulate the `0xCF8`/`0xCFC` config ports at all: reads return garbage
    // rather than the all-ones an absent bus gives on real hardware, so a scan
    // there invents devices. Enumeration lives on the bare-metal
    // (`multiboot2.rs`) path only.

    if !mem::init(&facts) {
        serial::puts("\nAkuma/amd64 — memory bring-up FAILED\n");
        halt();
    }
    // `/dev/wifi0` (`wifisim`): on this path too, so a VMM guest can test the
    // tool and the device without hardware.
    crate::drivers::wireless_init(cmdline);
    // `/proc/power`: after `mem::init`, because mapping the EC window allocates
    // page-table frames.
    power::init(&machine, cmdline);
    // `smp=N` and the idle wake budget: one call, shared with the multiboot2 path.
    boot::apply_cmdline_policies(cmdline);

    // The console hook and the `akuma-exec` runtime, shared with the multiboot2
    // entry point. One call because the two used to be two, and the second of
    // them reached only this path — see `boot::install_shared_sinks`.
    boot::install_shared_sinks();

    // PCI, on request only — the note above says why it is not automatic here.
    // `pci` on the command line is a promise from whoever booted this kernel
    // that the config ports are real, which under QEMU `-M q35` they are.
    //
    // That combination is the point: q35 gives a PCI bus, `-device qemu-xhci
    // -device usb-storage` gives a controller and a disk, and the xHCI driver
    // becomes something that can be iterated in seconds instead of by cold
    // reboots of a machine that crash-loops when the driver is wrong. It must
    // stay opt-in: on Firecracker the same scan invents devices out of garbage.
    let have_pci = machine::flag(hvm_start_info, "pci");
    if have_pci {
        pci::scan();
        pci::report();
        xhci::quiesce_all();
        drivers::sound_init(machine::flag(hvm_start_info, "hdatest"));
    }

    // Block devices, after the heap (the virtio HAL allocates DMA buffers from
    // it) and after the IDT (a bad transport address should fault reportably).
    let have_disk = blk::init(&facts);
    // The filesystem, on top of that disk. Both are best-effort: a machine with
    // no drive still boots, which is what `DISK=none` and every stage before
    // Stage M did.
    //
    // `usbroot` (with `pci`): mount `/dev/sda1` from the xHCI USB disk, as the
    // bare-metal path does for `root=/dev/sda1` — so a `q35` guest with
    // `-device qemu-xhci -device usb-storage` boots the same way the box does,
    // which is the only place the USB keyboard driver can be tried off the metal.
    let have_fs = if have_pci && machine::flag(hvm_start_info, "usbroot") {
        multiboot2::try_usb_root()
    } else {
        have_disk && fs::mount_root()
    };

    // Networking, after the heap (the stack allocates) and after the virtio
    // window is set (the NIC is another slot in the same array the disk came
    // from). DHCP on: both machines run a server — QEMU's user-mode stack, and
    // dnsmasq on the Firecracker host (`amd64/net-setup.sh`).
    let have_net = net::init(true);

    // No suite in this build: `boot::late_init` is the bring-up the suite would
    // otherwise have performed on the way past, and the same call the
    // multiboot2 path's `skiptests` lever makes. The secondaries' `keep_out` is
    // the PVH start-info block and its command line — what might be sitting on
    // the trampoline page differs by boot protocol, which is why it is a hook.
    #[cfg(feature = "no-tests")]
    {
        let si = &machine.start_info;
        boot::late_init(|| {
            let keep_out = [
                (si.addr, si.addr + 4096),
                (si.cmdline_paddr, si.cmdline_paddr + 4096),
            ];
            // The SVM hypervisor rehearsal, on request only (`kvmctl` arms it for
            // `/dev/kvm0` and runs nothing). `kvmtest` on the
            // command line runs here the rungs the multiboot2 path runs on the
            // metal, so they can be rehearsed under QEMU TCG
            // (`-cpu EPYC-Milan,svm=on`) without a reboot of the machine. It
            // must follow `late_init`'s `lapic::init`: the rungs unmask IRQs
            // around each VMRUN loop, and before the LAPIC is up the
            // microvm's unremapped legacy PIC delivers its pending serial and
            // PIT lines as vectors 4 and 0 — a `#DE`. Before
            // `start_secondaries` so the APs enable SVM too. Opt-in like `pci`:
            // a plain PVH boot (Firecracker) has no SVM and prints nothing new.
            if machine::flag(hvm_start_info, "kvmtest") || machine::flag(hvm_start_info, "kvmctl") {
                crate::drivers::hypervisor_init(cmdline);
                // After the root mount (above): the Linux rung reads its kernel
                // off the filesystem.
                crate::drivers::hypervisor_late(cmdline);
            }
            boot::bring_up_secondaries(machine.madt.as_ref(), &facts, cmdline, &keep_out);
        });
        // The network's bring-up the suite would otherwise have done on the way
        // past, the same block `multiboot2::boot_to_init` runs: DHCP, then the
        // wall clock, then the netpoll daemon (which carries the SNTP retry,
        // the memory watchdog and the RX pump). Until 2026-10-13 this path had
        // none of it — a `FEATURES=no-tests sh amd64/run.sh` guest got an
        // address from the socket waits' own polling and never an SNTP
        // attempt, found when the CMOS write-back could not be exercised here.
        if have_net {
            boot::dhcp_then_clock();
            net::spawn_netpoll();
            // A service guest (`kvmserve`) needs the host's network: start it now.
            crate::drivers::hypervisor_spawn_service(cmdline);
        }
    }

    // The whole suite, shared with the multiboot2 entry point. It was written
    // out separately in both until 2026-09-07, and the two lists had drifted —
    // the bare-metal path was not running `fork`, `execve`, `spawn`, busybox,
    // `blk` or the scheduler's park tests at all. See `boot::self_tests`.
    #[cfg(not(feature = "no-tests"))]
    let si = &machine.start_info;
    #[cfg(not(feature = "no-tests"))]
    let verdict = boot::self_tests(
        &boot::SuiteCtx {
            machine: &machine,
            facts: &facts,
            cmdline,
            smap,
            have_pci,
            // On a VMM, test xHCI whenever PCI was scanned and a controller is
            // there. Bare metal asks first — see the field's own note.
            want_xhci: have_pci,
            have_disk,
            have_fs,
            have_net,
            keep_out: [
                (si.addr, si.addr + 4096),
                (si.cmdline_paddr, si.cmdline_paddr + 4096),
            ],
        },
    );
    #[cfg(not(feature = "no-tests"))]
    if verdict.passed {
        serial::puts("Akuma/amd64 — all self-tests passed\n");
    } else {
        serial::puts("Akuma/amd64 — SELF-TESTS FAILED\n");
    }

    // Hand the console to a shell, if one is on the disk. After the verdict, so
    // an interactive session never hides a failing boot — and only on a passing
    // one, because a shell on a kernel whose own tests failed is a way to spend
    // an hour debugging the wrong layer.
    // `init` runs whether or not the suite passed — the same reversal the
    // multiboot2 path documents at length. A failed check is a line in `dmesg`,
    // not a reason to make the machine unreachable.
    if have_fs {
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
            boot::apply_runtime_flags(cmdline);
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
    // A halt is a failed boot: if a quiet boot's splash is up, show the log instead.
    splash::crash();
    // A core that stops must not take the Big Kernel Lock with it: the others
    // keep running (a fault on one core is reported, not spread), and after a
    // failed verdict they idle quietly instead of spinning in their tick
    // handlers. Before `smp::init_bsp` the lock is free and this is a no-op —
    // but it reads `gs:`, so it is gated on the block being installed.
    #[cfg(target_arch = "x86_64")]
    if smp::percpu_installed() {
        smp::bkl_abandon();
    }
    // `cli; hlt` in a loop (an NMI wake re-masks and re-halts); never returns.
    #[cfg(target_arch = "x86_64")]
    akuma_cpu::park::halt();
    #[cfg(not(target_arch = "x86_64"))]
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    #[cfg(target_arch = "x86_64")]
    {
        // A panic is for the screen: give the framebuffer back to every writer
        // before saying anything (`serial::set_fb_quiet`).
        splash::crash();
        serial::set_fb_quiet(false);
        serial::puts("\n[PANIC] ");
        if let Some(loc) = info.location() {
            serial::puts(loc.file());
            serial::puts(":");
            serial::put_dec(u64::from(loc.line()));
        } else {
            serial::puts("<no location>");
        }
        // The message, which this handler discarded until 2026-09-07.
        //
        // A location alone is not a diagnosis, and the tree is full of panics
        // whose whole value is the string: `akuma_primitives::Registered` exists
        // to carry one ("… not registered — call akuma_exec::init() first"), and
        // every one of those arrived here as a bare `lib.rs:208`. Found by
        // hitting exactly that panic while folding the first syscall into
        // `akuma-syscalls-glue` and having to bisect for a message the kernel
        // already had in hand.
        //
        // `safe_print!` per the console rules: a fixed stack buffer, no
        // allocation — the console is what survives when the allocator is what
        // broke, and a panic handler is that path by definition.
        serial::puts("\n        ");
        akuma_primitives::safe_print!(256, "{}", info.message());
        serial::puts("\n");
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = info;
    halt();
}
