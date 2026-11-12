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
//! interrupt needs an **IOAPIC**. Its address is now read from the MADT
//! (`akuma-ryzen-amd64`), but routing a GSI to a vector and taking the interrupt
//! is its own stage, and none of it is needed to read a sector.
//!
//! The interrupt number is parsed and recorded (`cmdline::MmioDevice::irq`)
//! rather than discarded, so the stage that does want it does not have to
//! re-derive it.

#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;

use crate::paging;
use crate::phys::DEVMAP_BASE;
use crate::serial;


/// The lowest-addressed transport's register window (slot 0), kept for the
/// boot self-test's raw probe.
static SLOT0: spinning_top::Spinlock<Option<akuma_primitives::mmio::MmioWindow>> = spinning_top::Spinlock::new(None);

/// Map one device's register file into the device window, uncached.
///
/// The window is direct — `DEVMAP_BASE + pa` — so a physical address has one
/// device-window address and `akuma_primitives::addr::mmio_phys_to_virt` agrees
/// with what is mapped here by construction rather than by convention.
fn map_device(base: u64, len: u64) -> Option<akuma_primitives::mmio::MmioWindow> {
    paging::map_device_window(base, len)
}

/// Map the machine's virtio transports and hand their geometry to the drivers.
///
/// Returns false when the machine announced no virtio device — which is not an
/// error. It is what `"drives": []` produces, and what every boot before this
/// stage did.
pub fn init(facts: &akuma_machine_facts::Facts) -> bool {
    let devs = facts.virtio();
    let Some(first) = devs.first() else {
        // No transports announced. Say so explicitly: the window defaults are
        // AArch64's, and a later probe that walks them faults on an address this
        // machine never mapped, long after the reason has scrolled away.
        akuma_primitives::addr::clear_virtio_window();
        return false;
    };

    let mut lowest: Option<(u64, akuma_primitives::mmio::MmioWindow)> = None;
    for d in devs {
        let Some(w) = map_device(d.window.base, d.window.size) else {
            serial::puts("  [blk] could not map a virtio transport\n");
            return false;
        };
        if lowest.is_none_or(|(b, _)| d.window.base < b) {
            lowest = Some((d.window.base, w));
        }
    }
    *SLOT0.lock() = lowest.map(|(_, w)| w);

    // Slot geometry: `base + i * stride`. The evenly-spaced check is
    // `akuma_machine_facts::virtio_geometry`, host-tested — believing a stride
    // that does not hold would point slot 1 at nothing and hand `VirtIOBlk::new`
    // a page of zeroes.
    let Some(akuma_machine_facts::VirtioGeometry { base, stride, slots }) = facts.virtio_geometry() else {
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

/// Read one 32-bit transport register of slot 0 (0 if nothing was mapped).
fn read_reg(off: usize) -> u32 {
    SLOT0.lock().and_then(|w| w.try_read::<u32>(off)).unwrap_or(0)
}

#[cfg(not(feature = "no-tests"))]
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

    t.check_eq("blk: transport magic is \"virt\"", u64::from(read_reg(reg::MAGIC)), 0x7472_6976);
    // Version 2 is the non-legacy interface. Both machines present it; a 1 here
    // would mean the legacy layout, which `virtio-drivers` handles differently
    // and this kernel has never seen.
    t.check_eq("blk: transport version is 2 (modern)", u64::from(read_reg(reg::VERSION)), 2);
    t.check_eq("blk: device id is 2 (block)", u64::from(read_reg(reg::DEVICE_ID)), 2);
    t.note("blk: vendor id", u64::from(read_reg(reg::VENDOR_ID)));

    if !t.check("blk: driver registered a device", akuma_virtio::block::is_initialized()) {
        return;
    }
    let sectors = akuma_virtio::block::with_device(|d| d.capacity_sectors());
    t.check("blk: capacity is non-zero", sectors.unwrap_or(0) > 0);
    t.note("blk: capacity in sectors", sectors.unwrap_or(0));

    // Read the **ext2 superblock**, which is a structure the next layer up is
    // about to parse rather than a pattern invented for this test.
    //
    // Until Stage N this read two known sectors from a raw disk built by
    // `mkdisk.py`. That disk existed to prove the DMA path before there was a
    // filesystem; the filesystem proves it better, and checking a real on-disk
    // structure means this test fails for the same reason `fs::mount_root`
    // would — which is what makes it a useful *first* failure rather than a
    // second opinion.
    //
    // The buffer is on the kernel stack, which `boot.s` rebased into the physmap
    // (`high_entry`), so `virt_to_phys` can translate it. A buffer anywhere else
    // in the upper half would trip that function's window assertion, which is
    // the diagnostic that assertion exists for.
    const SB_OFFSET: u64 = 1024;
    let mut sb = [0u8; 512];
    if !t.check(
        "blk: read the ext2 superblock",
        akuma_virtio::block::read_bytes(SB_OFFSET, &mut sb).is_ok(),
    ) {
        return;
    }

    // s_magic, at superblock offset 56.
    let magic = u16::from_le_bytes([sb[56], sb[57]]);
    t.check_eq("blk: superblock magic is 0xEF53", u64::from(magic), 0xEF53);

    // s_blocks_count (offset 4) and s_log_block_size (offset 24) must describe
    // a filesystem that fits the device the driver reported. A read that
    // returned the right magic from the wrong offset would not survive this.
    let blocks = u32::from_le_bytes([sb[4], sb[5], sb[6], sb[7]]);
    let log_bs = u32::from_le_bytes([sb[24], sb[25], sb[26], sb[27]]);
    let bytes = u64::from(blocks) << (10 + log_bs);
    t.check(
        "blk: the superblock describes a filesystem that fits the device",
        bytes > 0 && bytes <= sectors.unwrap_or(0) * 512,
    );
    t.note("blk: filesystem size in KiB", bytes / 1024);

    // The offset must actually reach the device: byte 0 is the boot block,
    // which `mkfs.ext2` leaves zeroed, so it cannot equal the superblock. A
    // driver that ignored the requested offset and returned block 0 for every
    // request would pass every check above and fail this one.
    let mut boot = [0u8; 512];
    if t.check(
        "blk: read the boot block at offset 0",
        akuma_virtio::block::read_bytes(0, &mut boot).is_ok(),
    ) {
        t.check("blk: offset 0 and offset 1024 differ", boot != sb);
        t.check("blk: the boot block is zeroed", boot.iter().all(|&b| b == 0));
    }
}
