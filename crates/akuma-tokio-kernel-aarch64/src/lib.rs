//! The aarch64 machine half of `akuma-tokio-kernel` (B21 A5.2).
//!
//! What exists so far is the boot: `_boot` (`akuma-boot-aarch64`, the same
//! instructions the main AArch64 kernel runs) builds the early page tables,
//! turns the MMU on and calls `rust_start(dtb)`, which `akuma-tokio-kernel`'s
//! `entry!` defines and which hands a [`Kernel`] to the root's `main`. The
//! builder takes the same calls as the amd64 half's, so a root's `main.rs`
//! builds unchanged for both; [`Kernel::run_init`] prints the banner on the
//! PL011 and powers the machine off, because the traps, the memory manager and
//! the devices that would load `init` are the later B21 steps
//! (`proposals/B21_TOKIO_KERNEL_AARCH64.md`).
//!
//! The console is `safe_print!` over a stack buffer into the UART, so nothing
//! on the boot path allocates. The heap is installed first anyway: the shared
//! crates below link `alloc`, and a binary that links it must name an
//! allocator.

#![no_std]
#![forbid(unsafe_code)]

use akuma_primitives::safe_print;
use akuma_tokio_kernel_core::Identity;

/// The heap. `#[global_allocator]` is a binary-level declaration; the crate
/// exports the implementation, this kernel installs it.
#[cfg(target_os = "none")]
#[global_allocator]
static ALLOCATOR: akuma_alloc::KernelAllocator = akuma_alloc::KernelAllocator;

/// The boot heap: 16 MiB from the first 2 MiB boundary above the boot stack.
/// Inside the 256 MiB `cargo_runner.sh` gives a VM by default; RAM is not read
/// from the device tree until a later step parses it.
#[cfg(target_os = "none")]
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

    /// Boot. On aarch64 this is the banner and a power-off until init can load.
    pub fn run_init(self) -> ! {
        #[cfg(target_os = "none")]
        let heap = {
            let start = akuma_boot_aarch64::linker_syms::stack_top().next_multiple_of(2 << 20);
            (start, akuma_alloc::init(start, HEAP_BYTES))
        };
        #[cfg(feature = "console")]
        if self.console {
            akuma_primitives::console::set_print_hook(uart_print);
        }
        akuma_tokio_kernel_core::set_identity(self.identity);
        let id = akuma_tokio_kernel_core::identity();
        akuma_primitives::console::print_str(id.art);
        safe_print!(160, "{} {} on aarch64, dtb at {:#x}\n", id.name, id.version, self.dtb);
        #[cfg(target_os = "none")]
        match heap {
            (at, Ok(())) => safe_print!(96, "[{}] heap {:#x}+{} MiB\n", id.name, at, HEAP_BYTES >> 20),
            (_, Err(e)) => safe_print!(96, "[{}] heap: {}\n", id.name, e),
        }
        safe_print!(160, "[{}] no traps, memory or devices on aarch64 yet (B21 A5.3+): powering off\n", id.name);
        power_off()
    }
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

/// PSCI `SYSTEM_OFF`, then park the core if the call came back.
fn power_off() -> ! {
    let _ = akuma_psci::call(akuma_psci::SYSTEM_OFF, 0, 0, 0);
    loop {
        akuma_cpu::park::wfe();
    }
}

/// A panic names where, on the console, and powers off.
#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    safe_print!(256, "[panic] {}\n", info);
    power_off()
}
