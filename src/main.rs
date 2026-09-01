#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
// Kernel-specific: MMIO and error-code paths require these casts intentionally.
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::wrong_self_convention)] // kernel types don't follow std naming
#![allow(clippy::inline_always)] // used for hot syscall paths
#![allow(clippy::needless_pass_by_value)] // trait bounds often require owned types
// Rump-only build (devbox: smoltcp compiled out). AF_INET socket syscalls route
// only through rump here, so the smoltcp-specific socket/net-syscall internals
// (e.g. `get_socket_from_fd`, `alloc_net_bounce`/`net_bounce_size_plan`,
// `epoll_on_fd_drained`, `EINPROGRESS`) go unreachable but stay compiled — they
// aren't individually `cfg`-gated, only reachable through call chains that are.
// Silence dead-code for this config only; the default/size/extreme builds
// (smoltcp on) keep dead-code denied.
#![cfg_attr(not(feature = "smoltcp"), allow(dead_code))]

extern crate alloc;

/// …and this puts it in the crate root's *path* namespace, for the call sites
/// that spell it `crate::safe_print!`. Both spellings are in use across `src/`
/// and both worked before, because `#[macro_export]` on a crate-root
/// `macro_rules!` provides each. The `#[macro_use] extern crate` half is gone
/// (2026-09-01): `src/exceptions.rs` — its last textual user — imports the
/// macro directly now, on its way to `akuma-exceptions`.
pub use akuma_primitives::{safe_print, tprint};

pub use akuma_kernel_core::akuma;
// The kernel heap moved to `crates/akuma-alloc` on 2026-08-31 to quarantine its
// 18 `unsafe` sites out of the bin crate (that crate's header explains why it
// cannot and should not `forbid`). Aliased rather than renamed at ~40 call sites,
// which also keeps `crate::allocator::` reading the same in the boot tests.
pub use akuma_alloc as allocator;

/// Install the kernel heap.
///
/// `#[global_allocator]` and `#[alloc_error_handler]` are binary-level
/// declarations, so they stay here rather than in `akuma-alloc`: a library that
/// makes them decides the allocator for everything linking it, including a host
/// test binary where it would fight std. The crate exports the implementation;
/// this is the one place that installs it.
#[global_allocator]
static ALLOCATOR: akuma_alloc::KernelAllocator = akuma_alloc::KernelAllocator;

/// OOM: kill the faulting userspace process instead of panicking the kernel.
///
/// This is policy, not allocation, which is why it lives in the bin — it needs
/// the process table and the syscall counters, neither of which the heap should
/// know about. With no current process (pure kernel context, early boot) there
/// is nothing to kill and panicking is correct.
#[alloc_error_handler]
fn alloc_error_handler(layout: core::alloc::Layout) -> ! {
    let stats = allocator::stats();
    safe_print!(256,
        "\n[OOM] allocation of {} bytes failed (heap {}MB / {}MB used) — killing process\n",
        layout.size(),
        stats.allocated / 1024 / 1024,
        stats.heap_size / 1024 / 1024,
    );
    // Whole-kernel diagnostics belong on this path, not inside `alloc`: a failed
    // allocation returns null and arrives here immediately.
    syscall::syscall_counters::dump();
    if akuma_exec::process::current_process_shared().is_some() {
        akuma_exec::process::return_to_kernel(-12); // ENOMEM
    }
    panic!("kernel OOM: allocation of {} bytes failed", layout.size());
}
// mod async_net;
#[cfg(kernel_tests)]
mod async_tests;
// The unsafe half of the bin crate moved to `akuma-kernel-glue` (2026-09-01),
// on top of `akuma-kernel-core`'s unsafe-free half; these re-exports keep
// every `crate::x::y` call site in this file — and in the test modules below,
// which are staying in `src/` — spelled as it was. Only the test modules
// (`#[cfg(kernel_tests)] mod x_tests;`) stay as real `mod` declarations here.
#[cfg(kernel_bkl_profile)]
pub use akuma_kernel_core::bkl_profile;
pub use akuma_kernel_glue::boot;
/// `linker.ld`'s absolute image/boot-stack symbols, named once in `akuma-entry`.
/// `process_tests.rs` is the only `src/` consumer.
#[cfg(kernel_tests)]
pub use akuma_kernel_glue::linker_syms;
pub use akuma_kernel_core::config;
pub use akuma_kernel_glue::console;
#[cfg(kernel_tests)]
mod daif_tests;
// mod embassy_net_driver;
// mod embassy_time_driver; // replaced by akuma_exec::alarms
// mod embassy_virtio_driver;
pub use akuma_kernel_core::file_page_cache;
pub use akuma_kernel_core::fs;
#[cfg(kernel_tests)]
mod fs_tests;
pub use akuma_kernel_core::irq;
#[cfg(all(kernel_tests, feature = "smoltcp"))]
mod network_tests;
// Device-level NIC counters' console half (`net-profile`). Measurement builds
// only; see the module docs and `crates/akuma-net/src/nicstat.rs`.
#[cfg(feature = "net-profile")]
pub use akuma_kernel_core::nic_profile;
pub use akuma_kernel_core::klog;
pub use akuma_kernel_core::ntp_boot;
pub use akuma_kernel_glue::platform;
pub use akuma_kernel_core::pmm;
#[cfg(kernel_tests)]
mod process_tests;
#[cfg(kernel_tests)]
mod pthread_tests;
#[cfg(feature = "rump")]
pub use akuma_kernel_glue::rump_proxy;
#[cfg(all(
    not(kernel_profile_extreme),
    feature = "rump",
    any(not(feature = "no-tests"), feature = "rump-tests"),
))]
mod rump_tests;
// Real (shared-kernel) SMP: one shared kernel across all cores; see
// docs/reference/subsystems/smp-shared.md. The experimental one-kernel-per-core
// multikernel was removed 2026-08-10 — docs/archive/TRIM_FAT_MULTIKERNEL.md.
#[cfg(kernel_smp_shared)]
pub use akuma_kernel_glue::smp_shared;
#[cfg(kernel_tests)]
mod sync_tests;
pub use akuma_kernel_glue::syscall;
#[cfg(kernel_tests)]
mod tests;
pub use akuma_kernel_core::timer;

/// The kernel-heap-sizing pure functions, `pub(crate)` in this file until
/// `kernel_main` moved to `akuma-kernel-glue` (2026-09-01) — crate-root
/// privacy made them visible to the test modules below for free. Now `pub` in
/// the glue crate (`pub(crate)` there means "private to `akuma-kernel-glue`",
/// not "private to the bin"), and re-exported here so
/// `crate::compute_heap_size` &c. in `tests.rs` still resolve.
pub use akuma_kernel_glue::{
    compute_heap_size, compute_memory_layout, compute_thread_limit, reserve_calc_ram,
    MemoryLayout,
};

// The virtio drivers moved to `akuma-virtio` together with the `Hal` and the
// MMIO probe loop they shared (docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md
// Phase 3). Re-bound here so the existing `crate::rng::…` path still resolves
// — `process_tests.rs` is the ONLY remaining `src/` caller (`block::init()`
// itself moved to `akuma-kernel-glue` with `kernel_main`, which reaches it
// through its own copy of this re-export), hence the `kernel_tests` gate: a
// `no-tests` build has no consumer left in the bin crate.
#[cfg(kernel_tests)]
pub(crate) use akuma_virtio::rng;

/// virtio-sound. Real driver or inert stub — `akuma-virtio` decides.
///
/// `process_tests.rs`'s `crate::audio::…` is the ONLY remaining `src/` caller
/// (see the `rng` re-export above for why this is a second, separate binding
/// rather than routed through `akuma_kernel_glue::audio`, and for the
/// `kernel_tests` gate).
#[cfg(kernel_tests)]
pub(crate) use akuma_virtio::audio;

pub use akuma_kernel_glue::vfs;

/// `akuma_exec::mmu`, kept accessible as `crate::mmu` for `tests.rs` &c. —
/// crate-root privacy makes a private `use` here visible to every descendant
/// module without needing `pub`. `process`/`threading` moved out fully: only
/// `kernel_main`/`run_async_main` (now in `akuma-kernel-glue`) used them.
/// `kernel_tests`-gated: `tests.rs` is the only consumer, so a `no-tests`
/// build has nothing left to reach it.
#[cfg(kernel_tests)]
use akuma_exec::mmu;
use core::panic::PanicInfo;

/// Global poll step counter for debugging hangs, used by the timer watchdog to
/// report which step is blocking. Moved to `akuma_kernel_core::timer` with the
/// watchdog itself (2026-09-01); re-exported so the bare name still resolves
/// at every existing call site in this file.
pub use akuma_kernel_core::timer::GLOBAL_POLL_STEP;

/// Stop the machine.
///
/// **PSCI `SYSTEM_OFF`, and nothing else.** QEMU implements it under every
/// accelerator, Firecracker implements it, and real hardware implements it —
/// measured 2026-09-01 on `HVF=1` (the default on Apple silicon) and `HVF=0`,
/// both clean exits. It is the only mechanism that stops the VM on the default
/// accelerator.
///
/// This used to try ARM semihosting (`hlt #0xf000` with `SYS_EXIT_EXTENDED`)
/// first, because it is the only mechanism that can hand an exit *code* back to
/// the shell. **Do not put it back without reading
/// `docs/archive/SRC_BOOT_ENTRY_UNSAFE_CLEANUP.md` §5**: under HVF the `hlt`
/// does not fall through to the next instruction, it wedges the vCPU, so a
/// panic on the default accelerator hung forever rather than stopping — and any
/// PSCI fallback placed after it was unreachable. Nothing in `scripts/` reads
/// QEMU's exit status anyway; harnesses detect a panic by grepping the log for
/// `[PANIC]`, which is why the trade is worth taking.
///
/// The `wfi` loop is what makes the `-> !` true: `akuma_psci::call` returns if
/// there is no conduit at all.
///
/// Moved to `akuma-kernel-glue` with `kernel_main` (2026-09-01) — it is called
/// far more often from there than from `panic` below — and re-exported so the
/// bare name still resolves here.
pub use akuma_kernel_glue::halt;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    console::print("\n\n!!! PANIC !!!\n");
    if let Some(location) = info.location() {
        console::print("Location: ");
        console::print(location.file());
        console::print(":");
        console::print_dec(location.line() as usize);
        console::print("\n");
    }
    // Use stack-based formatting to avoid heap allocation during panic
    // This prevents double-panic if the heap is corrupted
    console::print("Message: ");
    crate::safe_print!(256, "{}\n", info.message());
    halt()
}

/// The C ABI entry point the boot assembly `bl`s.
///
/// Immediately delegates to `kernel_main` (in `akuma-kernel-glue`). It holds
/// no `unsafe` operations of its own any more; the `#[unsafe(no_mangle)]`
/// attribute is the only thing here the `unsafe_code` lint objects to, and the
/// boot assembly needs the symbol name.
#[unsafe(no_mangle)]
pub extern "C" fn rust_start(dtb_ptr: usize) -> ! {
    // FIRST statement in the kernel's Rust entry, before any output at all: point
    // the shared `safe_print!` at the PL011.
    //
    // Why here and not in `akuma_exec::init` (which also registers it, from the
    // same `ExecRuntime::print_str = console::print`): that call is at
    // `kernel_main`'s line ~754, and everything between here and there —
    // DTB scan, memory detection, the MMU and heap bring-up, the layout
    // assertions — prints. Registering there would have silently swallowed all
    // of it, because the shared macro discards when unregistered. `console::print`
    // needs no initialisation (a const MMIO base and a volatile store), so there
    // is nothing to order this after. `OnceCopy::set` ignores the later duplicate.
    akuma_primitives::console::set_print_hook(console::print);

    // Early debug: print raw DTB pointer before anything else
    console::print("DTB ptr from boot (x0 arg): 0x");
    console::print_hex(dtb_ptr as u64);
    console::print("\n");

    // Also print what was stored at very first instruction
    let x0_at_entry = boot::x0_at_entry();
    console::print("x0 at _boot entry: 0x");
    console::print_hex(x0_at_entry);
    console::print("\n");

    // The boot self-test suite stays in `src/` (this bin crate), but
    // `kernel_main` — which runs it — is in `akuma-kernel-glue`, which cannot
    // depend back on its own dependent. Register the entry points as hooks
    // before handing off; see `akuma_kernel_glue::BootTestHooks`.
    #[cfg(kernel_tests)]
    akuma_kernel_glue::set_boot_test_hooks(akuma_kernel_glue::BootTestHooks {
        daif_tests: daif_tests::run_all_tests,
        memory_tests: tests::run_memory_tests,
        async_tests: async_tests::run_all,
        fs_tests: fs_tests::run_all_tests,
        threading_tests: tests::run_threading_tests,
        sync_tests: sync_tests::run_all_tests,
        pthread_tests: pthread_tests::run_all_tests,
        process_tests: process_tests::run_all_tests,
        benchmarks: tests::run_benchmarks,
        #[cfg(feature = "smoltcp")]
        network_tests: network_tests::run_tests,
        #[cfg(feature = "smoltcp")]
        process_network_tests: process_tests::run_network_tests,
    });
    #[cfg(all(
        not(kernel_profile_extreme),
        feature = "rump",
        any(not(feature = "no-tests"), feature = "rump-tests"),
    ))]
    akuma_kernel_glue::set_rump_tests_hook(rump_tests::run_all_tests);

    akuma_kernel_glue::kernel_main(dtb_ptr)
}
