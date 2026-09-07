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
mod banner;
#[cfg(target_arch = "x86_64")]
mod blk;
mod boot;
#[cfg(target_arch = "x86_64")]
mod clock;
#[cfg(target_arch = "x86_64")]
mod dns;
#[cfg(target_arch = "x86_64")]
mod fd;
#[cfg(target_arch = "x86_64")]
mod fs;
/// `futex(2)`: the effects half, over `akuma-syscalls-sync`'s decisions.
mod futex;
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
/// `clone(CLONE_VM|CLONE_THREAD)`: threads sharing one address space.
mod thread;
/// The shared `akuma_mmu::UserAddressSpace`, exercised at boot. It has no other
/// runtime caller on this target until item C1 folds `usermode.rs` in — which
/// is why B3's widening of it needs a test that is not a `cargo check`.
mod uas;
mod uaccess;
#[cfg(target_arch = "x86_64")]
mod xhci;

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

    // No PCI enumeration on this path. PVH means a VMM, and the VMMs this
    // target runs under (Firecracker, QEMU `microvm`) present every device as
    // virtio-MMIO — there is no PCI bus to walk. Worse, Firecracker does not
    // emulate the `0xCF8`/`0xCFC` config ports at all: reads return garbage
    // rather than the all-ones an absent bus gives on real hardware, so a scan
    // there invents devices. Enumeration lives on the bare-metal
    // (`multiboot2.rs`) path only.

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
    }

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

    // The whole suite, shared with the multiboot2 entry point. It was written
    // out separately in both until 2026-09-07, and the two lists had drifted —
    // the bare-metal path was not running `fork`, `execve`, `spawn`, busybox,
    // `blk` or the scheduler's park tests at all. See `boot::self_tests`.
    let si = &machine.start_info;
    let verdict = boot::self_tests(
        &mut t,
        &boot::SuiteCtx {
            machine: &machine,
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
    let passed = verdict.passed;
    if passed {
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
