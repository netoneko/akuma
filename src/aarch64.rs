//! The AArch64 kernel's bin half: the heap, the OOM policy, the panic handler, and the entry.
//!
//! Everything else is a crate. `akuma_kernel::entry!` defines `rust_start`, which `_boot`
//! (`akuma-boot-aarch64`) calls, and hands it to `akuma_kernel_glue::start`. The boot
//! self-test suite is `akuma-kernel-tests-aarch64`, registered before `kernel_main` runs.

use akuma_alloc as allocator;
use akuma_kernel_glue::{console, halt, syscall};
use akuma_primitives::safe_print;
use core::panic::PanicInfo;

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
    safe_print!(256, "{}\n", info.message());
    halt()
}

akuma_kernel::entry!(akuma_kernel_tests_aarch64::register);
