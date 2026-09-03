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
//! `proposals/REDUCING_PLATFORM_DEPENDENCY.md`: that 81.7% of the tree's
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
mod hvm;
#[cfg(target_arch = "x86_64")]
mod idt;
#[cfg(target_arch = "x86_64")]
mod lapic;
#[cfg(target_arch = "x86_64")]
mod mem;
#[cfg(target_arch = "x86_64")]
mod paging;
#[cfg(target_arch = "x86_64")]
mod port;
#[cfg(target_arch = "x86_64")]
mod sched;
#[cfg(target_arch = "x86_64")]
mod serial;

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

    // SAFETY: this is the value the PVH entry ABI delivered in %ebx; every read
    // inside is bounds-checked against the identity map.
    let Some(info) = (unsafe { hvm::StartInfo::read(hvm_start_info) }) else {
        serial::puts("  [FATAL] bad or missing hvm_start_info magic\n");
        halt();
    };

    serial::puts("  version=");
    serial::put_dec(u64::from(info.version));
    serial::puts(" modules=");
    serial::put_dec(u64::from(info.nr_modules));
    serial::puts(" rsdp=0x");
    serial::put_hex(info.rsdp_paddr);
    serial::puts(" cmdline=0x");
    serial::put_hex(info.cmdline_paddr);
    serial::puts("\n");

    serial::puts("  memmap: ");
    serial::put_dec(u64::from(info.memmap_entries));
    serial::puts(" entries\n");

    let mut usable = 0u64;
    for i in 0..info.memmap_entries {
        // SAFETY: index is bounds-checked and every field read is range-checked.
        let Some(r) = (unsafe { info.memmap_entry(i) }) else {
            serial::puts("    [unreadable entry]\n");
            continue;
        };
        serial::puts("    0x");
        serial::put_hex(r.addr);
        serial::puts(" + 0x");
        serial::put_hex(r.size);
        serial::puts("  ");
        serial::puts(r.kind_str());
        serial::puts("\n");
        if r.is_ram() {
            usable += r.size;
        }
    }

    serial::puts("  usable RAM: ");
    serial::put_dec(usable / 1024 / 1024);
    serial::puts(" MiB\n");

    if mem::init(&info) {
        mem::smoke_test();
        paging::smoke_test();
        // Only after the PMM and paging are up: the #PF handler allocates a
        // frame and maps it, so installing the IDT earlier would give us a
        // handler that cannot service the fault it exists to service.
        idt::init();
        idt::smoke_test();
        if lapic::init() {
            lapic::smoke_test();
            // Restart the timer the smoke test stopped: the scheduler wants a
            // live tick to drive NEED_RESCHED.
            lapic::start_timer();
            sched::smoke_test();
            lapic::stop_timer();
        }
        serial::puts("\nAkuma/amd64 — memory subsystem up\n");
    } else {
        serial::puts("\nAkuma/amd64 — memory bring-up FAILED\n");
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
