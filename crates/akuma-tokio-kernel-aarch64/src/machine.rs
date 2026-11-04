//! The machine as its device tree describes it: RAM, the GIC, the console, the
//! virtio-mmio slots, the command line. Read once at boot from a heap copy of
//! the tree (`akuma_boot_aarch64::read_dtb`), so nothing here holds a borrow
//! of guest RAM the PMM is about to hand out.

use alloc::vec::Vec;

use akuma_mmu::DevRegion;
use akuma_primitives::{addr, safe_print};

/// Offset from a redistributor's RD frame to its SGI frame (GICv3).
const GICR_SGI_OFFSET: usize = 0x1_0000;

/// What boot needs from the tree, copied out of it.
pub struct Machine {
    /// RAM: base and size of the first `memory` region.
    pub ram: (usize, usize),
    /// The `/chosen/bootargs` line (empty when there is none).
    pub cmdline: alloc::string::String,
    /// virtio-mmio slots: base of the first, stride, how many.
    pub virtio: Option<(usize, usize, usize)>,
    /// CPUs the tree describes.
    pub cpus: usize,
}

/// Copy the tree out of RAM, describe the machine, and install the device map
/// the GIC, console and virtio drivers reach their registers through. `None`
/// when there is no readable tree (no machine this kernel targets lacks one).
pub fn probe() -> Option<Machine> {
    let len = akuma_boot_aarch64::read_dtb(&mut [])?;
    let mut blob: Vec<u8> = Vec::new();
    blob.try_reserve_exact(len).ok()?;
    blob.resize(len, 0);
    akuma_boot_aarch64::read_dtb(&mut blob)?;
    let desc = match akuma_firecracker::describe(&blob) {
        Ok(d) => d,
        Err(e) => {
            safe_print!(96, "  [dtb] unusable device tree: {:?}\n", e);
            return None;
        }
    };
    let cmdline = fdt::Fdt::new(&blob)
        .ok()
        .and_then(|f| f.chosen().bootargs().map(alloc::string::String::from))
        .unwrap_or_default();
    let redist = desc.gic.redistributors?;

    let mut map = [DevRegion { va: 0, pa: 0, size: 0 }; akuma_mmu::MAX_DEV_REGIONS];
    let mut n = 0;
    let mut push = |va: usize, pa: usize, size: usize| {
        if n < map.len() {
            map[n] = DevRegion { va, pa, size };
            n += 1;
        }
    };
    let gicd = desc.gic.distributor.base as usize;
    push(addr::DEV_GIC_DIST_VA, gicd, addr::DEV_GIC_DIST_SIZE);
    push(addr::DEV_GIC_CPU_VA, gicd, 0x1000);
    if let Some(u) = desc.uart {
        push(addr::DEV_UART_VA, u.region.base as usize, 0x1000);
    }
    push(addr::DEV_GICR_RD_VA, redist.base as usize, 0x1000);
    push(addr::DEV_GICR_SGI_VA, redist.base as usize + GICR_SGI_OFFSET, 0x1000);
    let slots = desc.virtio_slots();
    let virtio = match slots {
        [] => None,
        [only] => Some((only.region.base as usize, 0x200, 1)),
        [first, second, ..] => {
            Some((first.region.base as usize, (second.region.base - first.region.base) as usize, slots.len()))
        }
    };
    if let Some((base, stride, count)) = virtio {
        // As many slots as the reserved VA span holds.
        let cap = addr::DEV_VIRTIO_SIZE / stride;
        let count = count.min(cap.max(1));
        let span = (count * stride).div_ceil(0x1000) * 0x1000;
        push(addr::DEV_VIRTIO_VA, base, span.min(addr::DEV_VIRTIO_SIZE));
        addr::set_virtio_geometry(stride, count);
    }
    akuma_mmu::set_device_map(&map[..n]);
    akuma_mmu::rebuild_boot_device_table();

    Some(Machine {
        ram: (desc.ram.base as usize, desc.ram.size as usize),
        cmdline,
        virtio: virtio.map(|(b, s, c)| (b, s, c.min((addr::DEV_VIRTIO_SIZE / s).max(1)))),
        cpus: desc.cpu_count,
    })
}
