//! Compile-time machine description.
//!
//! Akuma targets two AArch64 machines with completely different physical device
//! maps: QEMU's `virt` (the default) and Firecracker's aarch64 microVM
//! (`platform-firecracker`). This module is the **only** place either map is
//! written down.
//!
//! # Why some of this is compile-time and some is not
//!
//! The kernel-side *virtual* addresses are ours and live in
//! `akuma_primitives::addr`; they never change between machines. The *physical*
//! side is the machine's, and splits in two:
//!
//! - Addresses needed **before Rust runs** — really just the UART, so
//!   `safe_print!` works from the first line of `rust_start`. Those must be
//!   compile-time literals because `boot.rs` builds its page tables in assembly,
//!   before any FDT parsing is possible. They live here and are injected into
//!   the boot assembly as `global_asm!` `const` parameters.
//! - Everything else, which is **discovered from the FDT** at runtime and
//!   installed with [`akuma_exec::mmu::set_device_map`].
//!
//! The split is not stylistic. Firecracker places the GIC redistributors at
//! `0x3FFF_0000 - vcpu_count * 0x2_0000`, so CPU0's frames move depending on how
//! many vCPUs the microVM was configured with — and `SMP=N` is a runtime choice
//! in this tree. A build-time constant cannot express that address. The
//! constants below are therefore a *bootstrap* map (correct for the common
//! single-vCPU case, enough to print and to survive until the FDT is read), not
//! the authority. See `proposals/FIRECRACKER_PORT.md` §2.1 and §5.
//!
//! # Adding a third machine
//!
//! Add a `machine` module with the same constants and a `feature` gate. Nothing
//! outside this file should need a `#[cfg]` for it.

#![allow(dead_code)]

use akuma_exec::mmu::{self, DevRegion};
use akuma_primitives::addr;

#[cfg(not(feature = "platform-firecracker"))]
pub use qemu_virt as machine;

#[cfg(feature = "platform-firecracker")]
pub use firecracker as machine;

/// QEMU `virt` with `gic-version=3` — the default target.
///
/// Addresses from `hw/arm/virt.c`'s `base_memmap`.
#[cfg(not(feature = "platform-firecracker"))]
pub mod qemu_virt {
    pub const NAME: &str = "qemu-virt";

    /// Base of RAM. Also the fallback when the FDT has no usable `memory` node.
    pub const RAM_BASE: usize = 0x4000_0000;

    /// GIC distributor. 64 KiB on QEMU virt.
    pub const GICD_PA: usize = 0x0800_0000;
    /// GICv2 CPU interface. Does not exist under `gic-version=3`, but the page is
    /// harmless and `src/gic.rs`'s GICv2 path still references the VA.
    pub const GICC_PA: usize = 0x0801_0000;
    /// GICv3 redistributor region base. CPU *n*'s RD frame is
    /// `GICR_PA + n * GICR_STRIDE`; its SGI frame is that plus `GICR_SGI_OFFSET`.
    pub const GICR_PA: usize = 0x080A_0000;
    /// PL011 UART — the console. Needed before the FDT is parsed.
    pub const UART_PA: usize = 0x0900_0000;
    /// QEMU `fw_cfg`. Only `src/ramfb.rs` uses it.
    pub const FW_CFG_PA: Option<usize> = Some(0x0902_0000);
    /// Base of the virtio-mmio slot array.
    pub const VIRTIO_PA: usize = 0x0A00_0000;
    /// Bytes between virtio-mmio slots. QEMU virt packs eight slots 0x200 apart,
    /// so all eight live inside a single 4 KiB page.
    pub const VIRTIO_STRIDE: usize = 0x200;
    /// Number of virtio-mmio slots the machine exposes.
    pub const VIRTIO_SLOTS: usize = 8;
    /// INTID of virtio-mmio slot 0. QEMU virt wires the slots to SPI 16..23, and
    /// SPI *n* is INTID *n* + 32.
    pub const VIRTIO_INTID_BASE: u32 = 48;

    /// Is the 1 GiB identity block at `0x4000_0000` device memory?
    ///
    /// No — on QEMU virt that block *is* RAM. See the Firecracker arm, where the
    /// same range is the MMIO window and getting this wrong creates a
    /// mismatched-attribute alias over live device registers.
    pub const MMIO_WINDOW_IS_DEVICE: bool = false;
}

/// Firecracker's aarch64 microVM.
///
/// Constants read from `firecracker` **v1.16.1** (`src/vmm/src/arch/aarch64/`:
/// `layout.rs`, `gic/gicv3/mod.rs`, `mod.rs`) and
/// `device_manager/mmio.rs`'s `MMIO_LEN`. Verified identical on `main` at the
/// same date. Re-read them if the pinned Firecracker version moves.
#[cfg(feature = "platform-firecracker")]
pub mod firecracker {
    pub const NAME: &str = "firecracker";

    /// `layout::DRAM_MEM_START`. Firecracker puts guest RAM at 2 GiB; the
    /// 1 GiB..2 GiB range that holds RAM on QEMU virt is the MMIO window here.
    pub const RAM_BASE: usize = 0x8000_0000;

    /// `GICv3::get_dist_addr()` = `MMIO32_MEM_START - KVM_VGIC_V3_DIST_SIZE`
    /// = `0x4000_0000 - 0x1_0000`. Fixed, unlike the redistributors.
    pub const GICD_PA: usize = 0x3FFF_0000;
    /// No GICv2 CPU interface exists; Firecracker's in-kernel GIC is v3 and the
    /// CPU interface is system-register only. Mapped to the distributor page so
    /// the VA is never a dangling translation — nothing reads it under GICv3.
    pub const GICC_PA: usize = 0x3FFF_0000;
    /// `GICv3::get_redists_addr(vcpu_count)` = `get_dist_addr() - vcpu_count *
    /// 0x2_0000`, i.e. the redistributors are stacked **downward** from the
    /// distributor.
    ///
    /// This literal assumes `vcpu_count == 1`. **It is a bootstrap value only.**
    /// At `vcpu_count == 2` the correct base is `0x3FFB_0000`, at 4 it is
    /// `0x3FF7_0000`, and using this constant instead would point CPU0 at another
    /// core's frames — silently costing the boot core its timer interrupt. The
    /// FDT-derived map replaces it before any GIC access.
    pub const GICR_PA: usize = 0x3FFD_0000;
    /// `layout::SERIAL_MEM_START` = `RTC_MEM_START + MMIO_LEN`
    /// = `0x4000_0000 + 0x1000 + 0x1000`. PL011-compatible.
    pub const UART_PA: usize = 0x4000_2000;
    /// Firecracker has no `fw_cfg` device.
    pub const FW_CFG_PA: Option<usize> = None;
    /// `layout::MEM_32BIT_DEVICES_START` = `SERIAL_MEM_START + MMIO_LEN`.
    pub const VIRTIO_PA: usize = 0x4000_3000;
    /// `device_manager::mmio::MMIO_LEN`. Each virtio device gets its own page,
    /// unlike QEMU virt's 0x200 packing.
    pub const VIRTIO_STRIDE: usize = 0x1000;
    /// Firecracker only instantiates configured devices, so this is an upper
    /// bound for the bootstrap map; the FDT gives the real count.
    pub const VIRTIO_SLOTS: usize = 8;
    /// Firecracker allocates device IRQs from `GSI_LEGACY_START = 0`, and GSI 0
    /// is SPI 32 — so slot 0 is INTID 32, not 48 as on QEMU virt.
    pub const VIRTIO_INTID_BASE: u32 = 32;

    /// Yes — `0x4000_0000..0x8000_0000` is `MMIO32`, holding the RTC, the serial
    /// port and every virtio slot. Mapping it Normal-cacheable (as the QEMU path
    /// does, where it is RAM) would alias live device registers with mismatched
    /// memory attributes, which the ARM ARM leaves CONSTRAINED UNPREDICTABLE.
    pub const MMIO_WINDOW_IS_DEVICE: bool = true;
}

/// Bytes between one CPU's GICv3 redistributor RD frame and the next's.
/// Architectural: two 64 KiB frames per PE.
pub const GICR_STRIDE: usize = 0x2_0000;
/// Offset of the SGI_base frame within a PE's redistributor.
pub const GICR_SGI_OFFSET: usize = 0x1_0000;

/// The bootstrap device map: what `boot.rs` mapped, expressed as regions.
///
/// Installed before the FDT is parsed so that the console works and so a very
/// early fault has a chance of printing. Replaced wholesale by
/// [`crate::fdt_devices`]-derived regions as soon as the FDT is available.
#[must_use]
pub fn bootstrap_device_map() -> ([DevRegion; mmu::MAX_DEV_REGIONS], usize) {
    let mut out = [DevRegion { va: 0, pa: 0, size: 0 }; mmu::MAX_DEV_REGIONS];
    let mut n = 0;

    let mut push = |va: usize, pa: usize, size: usize| {
        if n < mmu::MAX_DEV_REGIONS {
            out[n] = DevRegion { va, pa, size };
            n += 1;
        }
    };

    push(addr::DEV_GIC_DIST_VA, machine::GICD_PA, addr::DEV_GIC_DIST_SIZE);
    push(addr::DEV_GIC_CPU_VA, machine::GICC_PA, 0x1000);
    push(addr::DEV_UART_VA, machine::UART_PA, 0x1000);
    if let Some(fw_cfg) = machine::FW_CFG_PA {
        push(addr::DEV_FW_CFG_VA, fw_cfg, 0x1000);
    }
    push(addr::DEV_GICR_RD_VA, machine::GICR_PA, 0x1000);
    push(addr::DEV_GICR_SGI_VA, machine::GICR_PA + GICR_SGI_OFFSET, 0x1000);
    push(addr::DEV_VIRTIO_VA, machine::VIRTIO_PA, virtio_window_bytes());

    (out, n)
}

/// Bytes of VA the virtio slot array needs, rounded up to whole pages.
///
/// QEMU virt's eight 0x200-apart slots fit in one page; Firecracker's eight
/// 0x1000-apart slots need eight.
#[must_use]
pub const fn virtio_window_bytes() -> usize {
    let span = machine::VIRTIO_SLOTS * machine::VIRTIO_STRIDE;
    let rounded = (span + 0xFFF) & !0xFFF;
    if rounded > addr::DEV_VIRTIO_SIZE { addr::DEV_VIRTIO_SIZE } else { rounded }
}

/// Install the bootstrap device map and virtio geometry.
///
/// Call before `mmu::init_shared_device_tables` and before any driver probe.
pub fn install_bootstrap_device_map() {
    let (map, n) = bootstrap_device_map();
    mmu::set_device_map(&map[..n]);
    addr::set_virtio_geometry(machine::VIRTIO_STRIDE, machine::VIRTIO_SLOTS);
}

/// Physical base of the GICv3 redistributor region.
///
/// Reads the installed device map so that an FDT-discovered redistributor wins
/// over [`machine::GICR_PA`], which is only correct for a single-vCPU
/// Firecracker microVM. Falls back to the compile-time literal if the map has not
/// been installed yet — that only happens before
/// [`install_bootstrap_device_map`], which runs as the first thing in
/// `kernel_main`.
#[must_use]
pub fn gicr_base_pa() -> usize {
    let (map, n) = mmu::device_map();
    map.iter()
        .take(n)
        .find(|r| r.va == addr::DEV_GICR_RD_VA)
        .map_or(machine::GICR_PA, |r| r.pa)
}

/// Physical base of the GIC distributor, from the installed device map.
#[must_use]
pub fn gicd_base_pa() -> usize {
    let (map, n) = mmu::device_map();
    map.iter()
        .take(n)
        .find(|r| r.va == addr::DEV_GIC_DIST_VA)
        .map_or(machine::GICD_PA, |r| r.pa)
}
