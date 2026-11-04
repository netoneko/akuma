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

mod exec;
mod fs;
mod machine;
mod mm;
mod net;
mod proc;
mod sched;
mod signal;
mod syscall;
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
    #[cfg(feature = "disk")]
    disk: bool,
    #[cfg(feature = "net")]
    net: bool,
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
            #[cfg(feature = "disk")]
            disk: false,
            #[cfg(feature = "net")]
            net: false,
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

    /// ext2 on the first virtio-blk device, mounted at `/`.
    #[cfg(feature = "disk")]
    pub const fn disk(self) -> Self {
        Self { disk: true, ..self }
    }

    /// The first virtio-net device with DHCP; without it, loopback only.
    #[cfg(feature = "net")]
    pub const fn net(self) -> Self {
        Self { net: true, ..self }
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
    akuma_boot_aarch64::user::init();
    if !mm::self_test() {
        safe_print!(64, "  [FATAL] the memory self-test failed\n");
        halt();
    }
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
    anchor_wall_clock(m.cmdline.as_str());

    // The process runtime and the tokio surface, then the root.
    exec::init();
    sched::init();
    fs::install_surface();
    #[cfg(feature = "net")]
    let nic = k.net && m.virtio.is_some();
    #[cfg(not(feature = "net"))]
    let nic = false;
    net::init(nic);
    #[cfg(feature = "disk")]
    let root = if k.disk && m.virtio.is_some() { fs::disk_root() } else { None };
    #[cfg(not(feature = "disk"))]
    let root: Option<alloc::sync::Arc<dyn akuma_vfs::Filesystem>> = None;
    let Some(root) = root else {
        safe_print!(96, "  [FATAL] no root filesystem\n");
        power_off();
    };
    if !fs::mount_root(root) {
        power_off();
    }
    // Evidence the mount table resolves: what `/` holds, and the init image.
    if let Ok(entries) = akuma_vfs_glue::list_dir("/") {
        safe_print!(32, "  /:");
        for e in entries.iter().take(24) {
            safe_print!(80, " {}", e.name.as_str());
        }
        safe_print!(4, "\n");
    }
    akuma_primitives::console::print_str(id.art);
    safe_print!(96, "\n  {} {} (akuma-tokio-kernel, aarch64)\n\n", id.name, id.version);
    run_init(m.cmdline.as_str())
}

/// The wall clock: `epoch=<unix seconds>` on the command line, anchored at
/// the uptime sampled here (the amd64 half's rule; the rig passes the host's
/// time). Without it `CLOCK_REALTIME` counts from 1970 at boot and TLS refuses
/// every certificate as not yet valid.
fn anchor_wall_clock(cmdline: &str) {
    let Some(e) = akuma_tokio_kernel_core::cmdline::token(cmdline, "epoch=") else {
        return;
    };
    let Ok(secs) = e.parse::<u64>() else {
        safe_print!(64, "  [clock] bad epoch= token, ignored\n");
        return;
    };
    let at = fs::uptime_us();
    akuma_primitives::clock::set_utc_time_us(secs.saturating_mul(1_000_000), at);
    safe_print!(96, "  [clock] epoch={} anchored at uptime={}us\n", secs, at);
}

/// Load `init=` (with `initargs=`/`initenv=`, `akuma_tokio_kernel_core::init::Prog`)
/// into the boot record and start it as pid 1. Never returns.
fn run_init(cmdline: &str) -> ! {
    use akuma_tokio_kernel_core::init::{MAX_ARGV, MAX_ENV, Prog};
    let mut prog = Prog::new();
    prog.parse(cmdline);
    let path = prog.path_str();
    safe_print!(160, "{}: init spawn -- init={}\n", akuma_tokio_kernel_core::identity().name, path);
    let Ok(image) = akuma_tokio_kernel_core::loader::read_image::<mm::Aarch64>(path) else {
        safe_print!(160, "  init: {} is not on the root\n", path);
        power_off();
    };
    let mut argv: [&str; MAX_ARGV + 1] = [path; MAX_ARGV + 1];
    for k in 0..prog.n_args() {
        argv[k + 1] = prog.arg_str(k);
    }
    let mut env: [&str; MAX_ENV] = [""; MAX_ENV];
    for (k, slot) in env.iter_mut().enumerate().take(prog.n_env()) {
        *slot = prog.env_str(k);
    }
    mm::set_task_slot(0);
    let Some(ti) = akuma_tokio_kernel_core::loader::load_task::<mm::Aarch64>(
        &argv[..=prog.n_args()],
        &env[..prog.n_env()],
        &image,
        path,
    ) else {
        safe_print!(96, "  init: task load failed\n");
        power_off();
    };
    drop(image);
    proc::start_init(path, &ti)
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

/// One byte of a process's stdio to the console.
pub(crate) fn console_putb(b: u8) {
    #[cfg(feature = "console")]
    {
        if b == b'\n' {
            akuma_uart::write_byte(b'\r');
        }
        akuma_uart::write_byte(b);
    }
    #[cfg(not(feature = "console"))]
    let _ = b;
}

/// PSCI `SYSTEM_OFF`, then park the core if the call came back.
pub(crate) fn power_off() -> ! {
    let _ = akuma_psci::call(akuma_psci::SYSTEM_OFF, 0, 0, 0);
    halt()
}

/// A panic names where, on the console, and parks.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    safe_print!(256, "\n[PANIC] {}\n", info);
    halt()
}
