//! The aarch64 machine half of `akuma-tokio-kernel` (B21 A5).
//!
//! Boot goes through `_boot` (`akuma-boot-aarch64`, the instructions the main
//! AArch64 kernel runs): early page tables, MMU on, `rust_start(dtb)`, which
//! `akuma-tokio-kernel`'s `entry!` defines and which hands a [`Kernel`] to the
//! root's `main`. The builder takes the same calls as the amd64 half's, so a
//! root's `main.rs` builds unchanged for both.
//!
//! # The boot order, and why
//!
//! console (the PL011 `_boot` already mapped) → heap (above the boot stack:
//! the shared crates allocate, and the device tree is copied onto it before the
//! PMM can hand its RAM out) → device tree (RAM, GIC, UART, virtio; installs
//! the device window) → PMM over the rest of RAM → vectors → GIC and the tick.
//! Then what is not here yet: the root filesystem, EL0, init
//! (`proposals/B21_TOKIO_KERNEL_AARCH64.md`, `docs/archive/TAOXICHUAN_B21_AARCH64.md`).
//!
//! Console output is `safe_print!` over a stack buffer into the UART: nothing
//! on a print path allocates.

// The kernel's own target only: under the host's `cargo clippy`/`cargo test` this crate is empty,
// as the amd64 half is on a non-x86 host.
#![cfg(all(target_arch = "aarch64", target_os = "none"))]
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod machine;
mod trap;

use akuma_primitives::safe_print;
use akuma_tokio_kernel_core::Identity;

/// The heap. `#[global_allocator]` is a binary-level declaration; the crate
/// exports the implementation, this kernel installs it.
#[global_allocator]
static ALLOCATOR: akuma_alloc::KernelAllocator = akuma_alloc::KernelAllocator;

/// The boot heap: 16 MiB from the first 2 MiB boundary above the boot stack.
/// It grows from the PMM once that is up (`akuma_alloc::mark_pmm_ready`).
const HEAP_BYTES: usize = 16 << 20;

/// A kernel being composed: what a root's `main.rs` chains before [`Kernel::run_init`].
#[must_use = "a Kernel does nothing until `run_init`"]
pub struct Kernel {
    dtb: u64,
    identity: Identity,
    #[cfg(feature = "console")]
    console: bool,
}

impl Kernel {
    /// The kernel `_boot` handed `dtb` (the device tree's physical address),
    /// named `name` (`uname -n`) at `version` (`uname -r`).
    pub const fn new(dtb: u64, name: &'static str, version: &'static str) -> Self {
        Self {
            dtb,
            identity: Identity { name, nodename: name, version, art: "" },
            #[cfg(feature = "console")]
            console: false,
        }
    }

    /// The name on the console and in the banner (`uname -n` stays the package name).
    pub const fn name(mut self, name: &'static str) -> Self {
        self.identity.name = name;
        self
    }

    /// Art printed above the banner line.
    pub const fn banner(mut self, art: &'static str) -> Self {
        self.identity.art = art;
        self
    }

    /// The PL011 UART: boot lines and every `safe_print!`.
    #[cfg(feature = "console")]
    pub const fn console(self) -> Self {
        Self { console: true, ..self }
    }

    /// ext2 on the first virtio-blk device (not wired on aarch64 yet).
    #[cfg(feature = "disk")]
    pub const fn disk(self) -> Self {
        self
    }

    /// The first virtio-net device (not wired on aarch64 yet).
    #[cfg(feature = "net")]
    pub const fn net(self) -> Self {
        self
    }

    /// The secondary cores (not wired on aarch64 yet).
    #[cfg(feature = "smp")]
    pub const fn smp(self) -> Self {
        self
    }

    /// Boot. On aarch64 this ends after the machine is up, until init can load.
    pub fn run_init(self) -> ! {
        boot(&self)
    }
}

fn boot(k: &Kernel) -> ! {
    #[cfg(feature = "console")]
    if k.console {
        akuma_primitives::console::set_print_hook(uart_print);
    }
    akuma_tokio_kernel_core::set_identity(k.identity);
    let id = akuma_tokio_kernel_core::identity();
    safe_print!(96, "\n{}/aarch64 -- EL1, MMU on, dtb at {:#x}\n", id.name, k.dtb);

    // The heap, above the boot stack.
    let heap = akuma_boot_aarch64::linker_syms::stack_top().next_multiple_of(2 << 20);
    if let Err(e) = akuma_alloc::init(heap, HEAP_BYTES) {
        safe_print!(96, "  [FATAL] heap: {}\n", e);
        halt();
    }

    // The machine, from its device tree (copied onto the heap first).
    let Some(m) = machine::probe() else {
        safe_print!(96, "  [FATAL] no usable device tree\n");
        halt();
    };
    let (ram_base, ram_size) = m.ram;
    safe_print!(
        160,
        "  machine: {} MiB RAM at {:#x}, {} cpu(s), {} virtio slot(s)\n",
        ram_size >> 20,
        ram_base,
        m.cpus,
        m.virtio.map_or(0, |v| v.2)
    );

    // The PMM over every page above the heap.
    akuma_pmm::register_config(akuma_pmm::PmmConfig {
        cow_ref_ledger: false,
        pmm_uaf_quarantine: false,
        pmm_premature_free_check: false,
    });
    akuma_pmm::register_hooks(akuma_pmm::PmmHooks {
        heap_reclaim: akuma_alloc::reclaim_to_pmm,
        drain_retired: || 0,
        evict_clean_file_pages: |_| 0,
        shrink_page_cache: |_| 0,
    });
    akuma_pmm::init(ram_base, ram_size, heap + HEAP_BYTES);
    akuma_alloc::mark_pmm_ready();
    safe_print!(
        96,
        "  pmm: {} MiB free, heap {:#x}+{} MiB\n",
        (akuma_pmm::free_count() * 4096) >> 20,
        heap,
        HEAP_BYTES >> 20
    );

    trap::init();
    trap::start_tick();
    // Let the tick run for a while in idle windows: the evidence that the GIC,
    // the timer and the EL1 IRQ vector all work.
    let start = akuma_timer::uptime_us();
    while akuma_timer::uptime_us() - start < 100_000 {
        akuma_cpu::park::irq_window();
        akuma_cpu::park::wfi();
    }
    safe_print!(
        96,
        "  tick: {} ticks in 100 ms (CNTV, {} us)\n",
        trap::ticks(),
        trap::TICK_US
    );
    safe_print!(160, "  cmdline: {}\n", m.cmdline.as_str());

    akuma_primitives::console::print_str(id.art);
    safe_print!(96, "\n  {} {} (akuma-tokio-kernel, aarch64)\n\n", id.name, id.version);
    safe_print!(160, "[{}] no root filesystem, EL0 or init on aarch64 yet (B21 A5.4+): powering off\n", id.name);
    power_off()
}

/// The console sink: every byte to the PL011, `\n` as `\r\n`.
#[cfg(feature = "console")]
fn uart_print(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            akuma_uart::write_byte(b'\r');
        }
        akuma_uart::write_byte(b);
    }
}

/// Park the core for good, interrupts masked.
pub fn halt() -> ! {
    akuma_cpu::daif::mask_irq();
    loop {
        akuma_cpu::park::wfe();
    }
}

/// PSCI `SYSTEM_OFF`, then park the core if the call came back.
fn power_off() -> ! {
    let _ = akuma_psci::call(akuma_psci::SYSTEM_OFF, 0, 0, 0);
    halt()
}

/// A panic names where, on the console, and parks.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    safe_print!(256, "\n[PANIC] {}\n", info);
    halt()
}
