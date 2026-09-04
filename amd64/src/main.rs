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
mod lapic;
#[cfg(target_arch = "x86_64")]
mod machine;
#[cfg(target_arch = "x86_64")]
mod loader;
#[cfg(target_arch = "x86_64")]
mod mem;
#[cfg(target_arch = "x86_64")]
mod mm;
#[cfg(target_arch = "x86_64")]
mod net;
#[cfg(target_arch = "x86_64")]
mod paging;
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
mod sock;

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

    // `idt::init` needs nothing but its own static table, so it goes here rather
    // than after the memory subsystem: a fault during memory bring-up then
    // prints a diagnosis instead of vanishing.
    idt::init();

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

    mem::smoke_test(&mut t);
    paging::smoke_test(&mut t);
    // The IDT is already loaded (before mem::init, so faults there are
    // visible). Demand paging is what needs the PMM, and that is only exercised
    // here.
    idt::smoke_test(&mut t);

    if t.check("lapic: initialised", lapic::init()) {
        lapic::smoke_test(&mut t);
        // Restart the timer the smoke test stopped: the scheduler wants a live
        // tick to drive NEED_RESCHED.
        lapic::start_timer();
        sched::smoke_test(&mut t);
        lapic::stop_timer();
    }

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
    // Last, because it is the only test whose program the kernel did not
    // assemble: everything before it has to work for a loader failure to be
    // readable as a loader failure.
    lapic::start_timer();
    usermode::elf_test(&mut t);
    usermode::fdprobe_test(&mut t);
    lapic::stop_timer();

    // Last: it spawns the netpoll daemon and leaves it running (which is the
    // point — `run_init` needs it), so it must come after the leak and
    // preemption checks above.
    net::netpoll_selftest(&mut t, have_net);

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
        if let Some(path) = machine::init_path(hvm_start_info, &mut init_buf) {
            // Leave the timer running for the init program: it drives preemption
            // (so a busy server cannot starve the netpoll daemon) and advances
            // the clock `akuma-net`'s wait deadlines are measured against. The
            // self-tests stop it between stages; a shell or server wants it on.
            if have_net {
                lapic::start_timer();
            }
            usermode::run_init(path);
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
