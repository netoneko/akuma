//! Stage M: a block device.
//!
//! The first hardware this target drives that is not the console, the timer or
//! the CPU itself — and the first time it runs a **driver from the shared
//! crates** rather than arch code written here. `akuma-virtio` and the
//! `virtio-drivers` transport underneath it are used unmodified; what this
//! module supplies is the three machine facts they need and cannot discover:
//! where the transport is, how the slots are spaced, and how to translate an
//! address.
//!
//! # What had to change elsewhere, and why it was not much
//!
//! `akuma-virtio` already took the slot **stride** and **count** at run time —
//! QEMU virt packs eight slots 0x200 apart, Firecracker gives each device a
//! 0x1000 page — so the driver never had an opinion to correct. Three seams
//! needed widening, each named by a document before this stage existed:
//!
//! 1. **The window base became a runtime value too**
//!    (`akuma_primitives::addr::set_virtio_window`). On AArch64 it is a fixed
//!    slot in the L0[1] device map; here the VMM chooses it and announces it on
//!    the command line, so no constant can express it.
//! 2. **`virt_to_phys` stopped being the identity.** That module's own header
//!    predicted this and prescribed the fix — a compile-time offset constant,
//!    not a hook — because the kernel now reaches RAM through a physmap.
//! 3. **MMIO translation split from RAM translation.** They are the same
//!    function on AArch64 and different windows here, with different
//!    cacheability. A device reached through the RAM translation would be a
//!    cached alias of a register file.
//!
//! # Polled, not interrupt-driven
//!
//! `virtio-drivers`' blocking `read_blocks` spins on the used ring. That is a
//! deliberate stopping point rather than an oversight: taking the device's
//! interrupt needs an **IOAPIC**, finding the IOAPIC needs ACPI's MADT, and
//! `hvm_start_info.rsdp_paddr` is 0 on both machines (measured), so ACPI would
//! have to start with a scan of the BIOS area. None of that is needed to read a
//! sector, and a stage that bundled it would be two stages wearing a coat.
//!
//! The interrupt number is parsed and recorded (`cmdline::MmioDevice::irq`)
//! rather than discarded, so the stage that does want it does not have to
//! re-derive it.

use akuma_selftest::Suite;

use akuma_ryzen_amd64::MmioDevices;
use crate::paging::{self, MemAttr, Prot};
use crate::phys::DEVMAP_BASE;
use crate::serial;

const PAGE_SIZE: u64 = 4096;

/// Map one device's register file into the device window, uncached.
///
/// The window is direct — `DEVMAP_BASE + pa` — so a physical address has one
/// device-window address and `akuma_primitives::addr::mmio_phys_to_virt` agrees
/// with what is mapped here by construction rather than by convention.
fn map_device(base: u64, len: u64) -> bool {
    let first = base & !(PAGE_SIZE - 1);
    let last = (base + len).div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let mut pa = first;
    while pa < last {
        if !paging::map_page(
            (DEVMAP_BASE + pa) as usize,
            pa,
            Prot::KERNEL_RW,
            MemAttr::Device,
        ) {
            return false;
        }
        pa += PAGE_SIZE;
    }
    true
}

/// Map the machine's virtio transports and hand their geometry to the drivers.
///
/// Returns false when the machine announced no virtio device — which is not an
/// error. It is what `"drives": []` produces, and what every boot before this
/// stage did.
pub fn init(devices: &MmioDevices) -> bool {
    let devs = devices.as_slice();
    let Some(first) = devs.first() else {
        return false;
    };

    for d in devs {
        if !map_device(d.base, d.len) {
            serial::puts("  [blk] could not map a virtio transport\n");
            return false;
        }
    }

    // Slot geometry: `base + i * stride`. The evenly-spaced check lives in
    // `akuma-ryzen-amd64`, where it is host-tested — believing a stride that does
    // not hold would point slot 1 at nothing and hand `VirtIOBlk::new` a page of
    // zeroes.
    let Some((base, stride, slots)) = devices.geometry() else {
        return false;
    };
    if slots < devs.len() {
        serial::puts("  [blk] transports are not evenly spaced; using the first only\n");
    }

    akuma_primitives::addr::set_virtio_window(
        (DEVMAP_BASE + base) as usize,
        stride as usize,
        slots,
    );

    serial::puts("  blk:  ");
    serial::put_dec(slots as u64);
    serial::puts(" virtio slot(s) at pa 0x");
    serial::put_hex(base);
    serial::puts(" stride 0x");
    serial::put_hex(stride);
    serial::puts(" irq ");
    serial::put_dec(u64::from(first.irq));
    serial::puts("\n");

    akuma_virtio::block::init().is_ok()
}

/// virtio-MMIO register offsets, for the raw probe below.
mod reg {
    /// `0x74726976` — "virt" little-endian.
    pub const MAGIC: usize = 0x000;
    pub const VERSION: usize = 0x004;
    pub const DEVICE_ID: usize = 0x008;
    pub const VENDOR_ID: usize = 0x00c;
}

/// Read one 32-bit transport register through the device window.
fn read_reg(slot_va: usize, off: usize) -> u32 {
    // SAFETY: `slot_va` is a device-window address `init` mapped, and every
    // offset here is inside the 0x100-byte virtio-MMIO header, which is inside
    // the smallest register file either machine reports (0x200).
    unsafe { ((slot_va + off) as *const u32).read_volatile() }
}

/// Prove the device is there, then read from it.
///
/// The raw register reads come first and deliberately do not go through
/// `virtio-drivers`: if the mapping or the address were wrong, `VirtIOBlk::new`
/// would fail with one opaque error, and "the driver did not initialise" does
/// not distinguish a bad base address from an unsupported feature negotiation.
/// A magic number that reads back as `0x74726976` says the address is right and
/// the mapping is live before anything else is attempted.
pub fn smoke_test(t: &mut Suite, expect_disk: bool) {
    if !expect_disk {
        t.note("blk: no virtio device announced; skipped", 0);
        return;
    }

    let slot = akuma_primitives::addr::virtio_slot_va(0);
    t.check_eq("blk: transport magic is \"virt\"", u64::from(read_reg(slot, reg::MAGIC)), 0x7472_6976);
    // Version 2 is the non-legacy interface. Both machines present it; a 1 here
    // would mean the legacy layout, which `virtio-drivers` handles differently
    // and this kernel has never seen.
    t.check_eq("blk: transport version is 2 (modern)", u64::from(read_reg(slot, reg::VERSION)), 2);
    t.check_eq("blk: device id is 2 (block)", u64::from(read_reg(slot, reg::DEVICE_ID)), 2);
    t.note("blk: vendor id", u64::from(read_reg(slot, reg::VENDOR_ID)));

    if !t.check("blk: driver registered a device", akuma_virtio::block::is_initialized()) {
        return;
    }
    let sectors = akuma_virtio::block::with_device(akuma_virtio::block::VirtioBlockDevice::capacity_sectors);
    t.check("blk: capacity is non-zero", sectors.unwrap_or(0) > 0);
    t.note("blk: capacity in sectors", sectors.unwrap_or(0));

    // The disk `amd64/mkdisk.py` wrote. Reading it back is what proves the whole
    // path — descriptor ring, DMA translation, the device's own view of memory —
    // rather than just that a driver object was constructed.
    //
    // The buffer is on the kernel stack, which `boot.s` rebased into the physmap
    // (`high_entry`), so `virt_to_phys` can translate it. A buffer anywhere else
    // in the upper half would trip that function's window assertion, which is
    // the diagnostic that assertion exists for.
    let mut buf = [0u8; 512];
    if !t.check(
        "blk: read sector 0",
        akuma_virtio::block::read_bytes(0, &mut buf).is_ok(),
    ) {
        return;
    }

    const SIG: &[u8] = b"AKUMA/amd64 blk probe";
    t.check("blk: sector 0 carries the probe signature", buf.starts_with(SIG));
    // Every byte after the signature is `(i * 7 + 3) & 0xff`, so a read that
    // returned the right first bytes and then garbage — a short DMA, a
    // descriptor length bug — fails here rather than passing.
    let patterned = (SIG.len()..512).all(|i| buf[i] == ((i * 7 + 3) & 0xff) as u8);
    t.check("blk: sector 0 pattern is intact to the end", patterned);

    // A sector 1 MiB in, to prove the offset reaches the device rather than
    // being ignored: a driver that returned sector 0 for every request would
    // pass every check above.
    let mut far = [0u8; 512];
    if t.check(
        "blk: read sector 2048 (1 MiB in)",
        akuma_virtio::block::read_bytes(2048 * 512, &mut far).is_ok(),
    ) {
        t.check(
            "blk: the far sector is a different sector",
            far.starts_with(b"AKUMA/amd64 far sector") && far[..32] != buf[..32],
        );
    }
}
