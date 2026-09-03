//! Akuma's machine description, derived from the device tree at run time.
//!
//! # Why this exists
//!
//! Akuma's Firecracker device map is a table of compile-time literals
//! (`src/platform.rs`), read out of Firecracker's source. That works until the
//! machine describes something a constant cannot: **the GIC redistributor base
//! moves with the vCPU count.**
//!
//! ```text
//! GICD      = 0x3fff_0000                  fixed
//! GICR base = 0x3fff_0000 - n * 0x2_0000   n = vcpu_count
//! ```
//!
//! Measured, at 1/2/4/8 vCPUs, in `docs/reference/firecracker/fdt/`. Pin the
//! literals to one vCPU and booting with two makes the boot core drive *CPU 1's*
//! redistributor: it clears the wrong `GICR_WAKER`, enables the timer on the
//! wrong frame, and loses its scheduler tick — with no build error and no boot
//! error. Since `SMP=N` is chosen at run time in this tree, no build-time
//! constant can express that address. The device map has to be read from the
//! machine, which is what this crate does.
//!
//! Background: `docs/archive/FIRECRACKER_PORT.md` §2.1 and §5.
//!
//! # Design
//!
//! * **`no_std`, and no allocation.** `detect_memory` and `probe_dtb` run
//!   *before* the heap is initialized — deliberately, because the allocator may
//!   be placed exactly where the DTB sits. A device map that needed a `Vec`
//!   could not be built in that window. Everything here is fixed-size and
//!   `Copy`.
//! * **Reads, never computes.** Given the formula above it would be tempting to
//!   take `vcpu_count` and calculate the redistributor base. The FDT states it
//!   outright; re-deriving it would reintroduce exactly the assumption this crate
//!   exists to remove.
//! * **Platform-neutral, and tested that way.** Nothing here knows a Firecracker
//!   address. The test fixtures are real device trees from *both* machines, and
//!   `no_address_is_hardcoded` asserts they disagree on every field this crate
//!   reports — RAM base, distributor, redistributor, UART address, UART INTID,
//!   virtio base, virtio INTID, even the virtio stride. One machine description,
//!   not two literal tables to keep in sync.
//!
//! # The two machines, side by side
//!
//! Measured, from the fixtures:
//!
//! | | Firecracker v1.16.1 | QEMU virt (gic-version=3) |
//! |---|---|---|
//! | memory node | `0x8020_0000` (DRAM + 2 MiB reserved) | `0x4000_0000` |
//! | GICD | `0x3fff_0000`, 64 KiB | `0x0800_0000`, 64 KiB |
//! | GICR | `GICD - n * 0x2_0000` — **moves with `n`** | `0x080a_0000`, span `0xf6_0000` — **fixed** |
//! | console | `0x4000_2000`, `ns16550a`, INTID 35 | `0x0900_0000`, `arm,pl011`, INTID 33 |
//! | virtio | 3 slots, stride `0x1000`, INTID 32+ | 32 slots, stride `0x200`, INTID 48+ |
//!
//! The GICR row is the reason this crate exists, and the QEMU column is the
//! reason the bug hid for so long: a literal redistributor address is *correct*
//! on QEMU virt at every `SMP=N`, so nothing complained until a second machine
//! showed up.
//!
//! # `fdt` 0.1.5 API traps
//!
//! Measured against the fixtures, not inferred — both of these look like the
//! obvious way to write this and both silently do the wrong thing:
//!
//! * **`Fdt::all_nodes()` stops early.** On the Firecracker fixtures it yields
//!   7 of 14 root children and gives up after `chosen` — so `intc`, the UART and
//!   every virtio node are invisible. Iterating it returns "no interrupt
//!   controller" for a tree that plainly has one.
//! * **`Fdt::find_compatible()` returns `None`** for compatible strings that are
//!   present (`arm,gic-v3`, `virtio,mmio`). Presumably built on `all_nodes()`.
//!
//! What does work, and what this crate uses: `find_node("/")` then `.children()`,
//! and `find_all_nodes("/prefix")`.
//!
//! # What it does not do
//!
//! Install anything. This crate answers "what is this machine?"; mapping the
//! result into page tables stays in `akuma-exec`'s MMU code, and the boot-time
//! UART address stays a compile-time constant because something has to print
//! before the FDT can be parsed.

#![no_std]
// Unsafe-free by design, and `forbid` so no module can opt back in with a local
// `allow`. Same reasoning as `akuma-net-yarn` and `akuma-syscalls-sync`. See
// `describe_fdt` for the one `unsafe` this crate used to have and where it went.
#![forbid(unsafe_code)]

use fdt::Fdt;

/// Physical address span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Region {
    pub base: u64,
    pub size: u64,
}

impl Region {
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.base + self.size
    }

    #[must_use]
    pub const fn contains(&self, addr: u64) -> bool {
        addr >= self.base && addr < self.base + self.size
    }
}

/// An MMIO device: where it is, and which GIC INTID it raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Device {
    pub region: Region,
    /// GIC INTID, already resolved from the `interrupts` cells — not the raw SPI
    /// or PPI number. See [`Interrupt`].
    pub intid: u32,
}

/// GIC architecture version, from the interrupt controller's `compatible`.
///
/// It decides what the *second* `reg` entry means: a redistributor span under
/// GICv3, the CPU interface under GICv2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GicVersion {
    V2,
    V3,
}

/// The interrupt controller as the machine describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gic {
    pub version: GicVersion,
    /// Distributor. 64 KiB on both machines — worth stating because Akuma's
    /// device VA map once reserved a single 4 KiB page for it, so
    /// `GICD_IROUTER` writes at offset 0x6000 aliased onto the redistributor
    /// (`docs/archive/GICD_IROUTER_ALIASING.md`).
    pub distributor: Region,
    /// GICv3 redistributors: `n * 0x2_0000`, based *below* the distributor.
    /// `None` on GICv2, where the second `reg` entry is the CPU interface.
    pub redistributors: Option<Region>,
}

/// Upper bound on virtio-mmio slots recorded.
///
/// Eight, to match what `akuma-virtio`'s probe actually walks
/// (`crates/akuma-virtio/src/probe.rs`) — a ninth slot reported here would be a
/// device the kernel never looks at.
///
/// The two machines are nothing alike here, which is why truncation needs a
/// policy rather than a cap alone:
///
/// | Machine | slots advertised | stride |
/// |---|---|---|
/// | Firecracker | 3 — only configured devices | `0x1000` |
/// | QEMU virt | **32**, mostly empty | `0x200` |
///
/// So QEMU virt *is* truncated, by 24 slots. [`MachineDescription::virtio_seen`]
/// reports how many the machine described, so a caller can tell "this machine has
/// eight devices" from "we kept eight of thirty-two".
pub const MAX_VIRTIO_SLOTS: usize = 8;

/// Everything the kernel needs to build its device map.
#[derive(Debug, Clone, Copy)]
pub struct MachineDescription {
    /// First region of the `memory` node.
    ///
    /// **This is not necessarily where DRAM begins.** Firecracker reserves the
    /// first 2 MiB of DRAM (`SYSTEM_MEM_SIZE`) and the node describes only what
    /// follows, so a 1024 MiB microVM reports base `0x8020_0000`, size
    /// `0x3fe0_0000` — 1022 MiB — while `DRAM_MEM_START` is `0x8000_0000`.
    /// Anything reading this node sees the former.
    pub ram: Region,
    pub gic: Gic,
    /// Primary serial port. `None` if the machine has none, which no supported
    /// machine does.
    pub uart: Option<Device>,
    virtio: [Device; MAX_VIRTIO_SLOTS],
    virtio_count: usize,
    virtio_seen: usize,
    /// `cpu@N` nodes present.
    ///
    /// Both machines describe **every** configured CPU: the measured sweep has
    /// 1/2/4/8 `cpu@N` nodes for `vcpu_count` 1/2/4/8
    /// (`docs/reference/firecracker/fdt/`). Being *described* is not being
    /// *running* — Firecracker's secondaries are powered off awaiting a PSCI
    /// `CPU_ON` — so this counts configured CPUs, not online ones.
    ///
    /// Do not derive the redistributor span from it. `cpu_count * 0x2_0000`
    /// happens to equal that span on Firecracker, so code that computed it would
    /// pass every test here and still be an address inferred from an unrelated
    /// property — the same mistake as the compile-time literal. Read
    /// [`Gic::redistributors`], which this crate takes from `intc`'s second `reg`
    /// entry.
    pub cpu_count: usize,
    /// `totalsize` from the FDT header.
    pub fdt_size: usize,
}

impl MachineDescription {
    /// virtio-mmio slots, ascending by physical address.
    ///
    /// Address order is the contract, not discovery order: Firecracker assigns
    /// MMIO addresses in device-creation order, Akuma's block driver takes the
    /// *first* virtio-blk it probes, and `akuma-rump` binds the *second*
    /// virtio-net by index. A stable order is what makes "slot k" mean anything.
    #[must_use]
    pub fn virtio_slots(&self) -> &[Device] {
        &self.virtio[..self.virtio_count]
    }

    /// How many virtio-mmio nodes the machine described, before the
    /// [`MAX_VIRTIO_SLOTS`] cap. Greater than `virtio_slots().len()` means slots
    /// were dropped — 32 described vs 8 kept on QEMU virt.
    #[must_use]
    pub const fn virtio_seen(&self) -> usize {
        self.virtio_seen
    }
}

/// Why a device tree could not be turned into a machine description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Not a device tree, or a truncated one.
    BadFdt,
    /// No `memory` node, or one with no usable region.
    NoMemory,
    /// No interrupt-controller node. Without the GIC there is no point
    /// continuing: nothing can be routed.
    NoInterruptController,
    /// The interrupt controller has no `reg`, or a distributor entry with no size.
    BadInterruptController,
}

/// One entry of an `interrupts` property, under the standard ARM GIC binding.
///
/// Three cells: kind, number, flags. The number is **relative to its kind**, so
/// resolving it to an INTID is where the platform difference everyone trips over
/// actually lives:
///
/// | Machine | virtio `interrupts` | SPI | INTID |
/// |---|---|---|---|
/// | Firecracker | `<0 0 1>` | 0 | **32** |
/// | QEMU virt | `<0 16 1>` | 16 | **48** |
///
/// Same binding, different allocation base. So `VIRTIO_MMIO_SPI_BASE` was never
/// a property of the *kernel* — it was a property of the machine, readable here,
/// and it stops being a platform constant once this is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupt {
    pub intid: u32,
}

impl Interrupt {
    /// GIC INTID space: SGIs 0-15, PPIs 16-31, SPIs 32+.
    const PPI_BASE: u32 = 16;
    const SPI_BASE: u32 = 32;

    /// Parse the first entry of an `interrupts` value.
    ///
    /// `None` for a malformed property or an unknown kind, so a device with an
    /// unreadable interrupt is skipped rather than silently wired to INTID 32.
    #[must_use]
    pub fn first(value: &[u8]) -> Option<Self> {
        let cells = value.get(..12)?;
        let kind = be32(&cells[0..4])?;
        let number = be32(&cells[4..8])?;
        let intid = match kind {
            0 => Self::SPI_BASE.checked_add(number)?,
            1 => Self::PPI_BASE.checked_add(number)?,
            _ => return None,
        };
        Some(Self { intid })
    }
}

fn be32(bytes: &[u8]) -> Option<u32> {
    let b: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(u32::from_be_bytes(b))
}

/// Parse a device tree already in memory.
///
/// This is the host-testable entry point, which is why the fixtures in
/// `docs/reference/firecracker/fdt/` can be unit tests rather than a devbox boot.
pub fn describe(blob: &[u8]) -> Result<MachineDescription, Error> {
    let fdt = Fdt::new(blob).map_err(|_| Error::BadFdt)?;
    describe_fdt(&fdt)
}

/// Parse a device tree the caller has already materialised.
///
/// The boot path used to reach this crate through a `describe_ptr(*const u8)`
/// that called `Fdt::from_ptr` internally — the crate's only `unsafe`, and the
/// only reason it could not be `#![forbid(unsafe_code)]`. It moved to the caller
/// on 2026-08-30, which was a relocation with three things going for it: the one
/// caller (`platform::install_fdt_device_map`) is itself an `unsafe fn`
/// carrying the identical contract, the kernel already makes the same
/// `Fdt::from_ptr` call thirty lines away in `smp_shared::probe_dtb`, and it
/// already depends on the same `fdt` version.
///
/// What that buys is not a badge: the 470 lines of offset arithmetic in this
/// crate — the part with real bug potential, and the reason the FDT fixtures in
/// `docs/reference/firecracker/fdt/` are unit tests — are now compiler-checked,
/// and the one-line pointer materialisation sits beside its twin in the kernel.
pub fn describe_fdt(fdt: &Fdt<'_>) -> Result<MachineDescription, Error> {
    let ram = fdt
        .memory()
        .regions()
        .find_map(|r| {
            let size = r.size?;
            (size > 0).then_some(Region {
                base: r.starting_address as usize as u64,
                size: size as u64,
            })
        })
        .ok_or(Error::NoMemory)?;

    let gic = describe_gic(fdt)?;
    let uart = find_uart(fdt);
    let (virtio, virtio_count, virtio_seen) = find_virtio(fdt);

    Ok(MachineDescription {
        ram,
        gic,
        uart,
        virtio,
        virtio_count,
        virtio_seen,
        cpu_count: fdt.cpus().count(),
        fdt_size: fdt.total_size(),
    })
}

/// Does this node's `compatible` list contain `want`?
///
/// `compatible` is a NUL-separated list, so a substring test would match
/// `arm,gic-v3-its` when asked for `arm,gic-v3` — and the ITS is a different
/// device at a different address. Compare whole entries.
fn compatible_with(node: &fdt::node::FdtNode<'_, '_>, want: &str) -> bool {
    node.property("compatible")
        .is_some_and(|p| p.value.split(|b| *b == 0).any(|s| s == want.as_bytes()))
}

/// Root's direct children — every device node on both supported machines.
///
/// `all_nodes()` would be the obvious choice and is broken (see the crate docs),
/// so the walk is explicit. One level is enough: the only nested node either
/// machine puts deeper is the GIC ITS, which Akuma does not use.
fn root_children<'b, 'a>(
    fdt: &'b Fdt<'a>,
) -> impl Iterator<Item = fdt::node::FdtNode<'b, 'a>> + 'b {
    fdt.find_node("/")
        .into_iter()
        .flat_map(fdt::node::FdtNode::children)
}

fn describe_gic(fdt: &Fdt<'_>) -> Result<Gic, Error> {
    // By compatible string, not by node name: Firecracker calls it `intc`, QEMU
    // virt `intc@8000000`, and other machines something else again.
    let node = root_children(fdt)
        .find(|n| {
            compatible_with(n, "arm,gic-v3")
                || compatible_with(n, "arm,gic-400")
                || compatible_with(n, "arm,cortex-a15-gic")
        })
        .ok_or(Error::NoInterruptController)?;

    let version = if compatible_with(&node, "arm,gic-v3") {
        GicVersion::V3
    } else {
        GicVersion::V2
    };

    let mut regs = node.reg().ok_or(Error::BadInterruptController)?;

    let first = regs.next().ok_or(Error::BadInterruptController)?;
    let distributor = Region {
        base: first.starting_address as usize as u64,
        size: first.size.ok_or(Error::BadInterruptController)? as u64,
    };

    // Second entry: redistributors under GICv3, CPU interface under GICv2. Only
    // the former is a redistributor, and conflating them would point the
    // per-CPU register writes at the wrong thing entirely.
    let redistributors = match version {
        GicVersion::V3 => regs.next().and_then(|r| {
            Some(Region {
                base: r.starting_address as usize as u64,
                size: r.size? as u64,
            })
        }),
        GicVersion::V2 => None,
    };

    Ok(Gic {
        version,
        distributor,
        redistributors,
    })
}

fn device_from(node: &fdt::node::FdtNode<'_, '_>) -> Option<Device> {
    let reg = node.reg()?.next()?;
    let interrupt = Interrupt::first(node.property("interrupts")?.value)?;
    Some(Device {
        region: Region {
            base: reg.starting_address as usize as u64,
            size: reg.size? as u64,
        },
        intid: interrupt.intid,
    })
}

fn find_uart(fdt: &Fdt<'_>) -> Option<Device> {
    // Two different UARTs, so match both explicitly:
    //   Firecracker  compatible = "ns16550a"                 (a 16550, not a PL011)
    //   QEMU virt    compatible = "arm,pl011\0arm,primecell"
    //
    // Do NOT fall back to "arm,primecell": Firecracker's RTC at 0x4000_1000 is
    // `arm,pl031\0arm,primecell`, so a primecell search finds the *clock* and
    // reports it as the console. It is listed before the UART in the tree, so
    // that fallback would win every time.
    root_children(fdt)
        .find(|n| compatible_with(n, "ns16550a") || compatible_with(n, "arm,pl011"))
        .and_then(|n| device_from(&n))
}

/// Collect virtio-mmio slots: the lowest [`MAX_VIRTIO_SLOTS`] by address, in
/// address order, plus how many the machine actually described.
///
/// Returns `(slots, kept, seen)`.
fn find_virtio(fdt: &Fdt<'_>) -> ([Device; MAX_VIRTIO_SLOTS], usize, usize) {
    let mut slots = [Device::default(); MAX_VIRTIO_SLOTS];
    let mut kept = 0usize;
    let mut seen = 0usize;

    for node in root_children(fdt) {
        if !compatible_with(&node, "virtio,mmio") {
            continue;
        }
        let Some(dev) = device_from(&node) else {
            continue;
        };
        seen += 1;

        // Insertion sort by base address, so callers never depend on FDT
        // traversal order. At most eight elements.
        if kept < MAX_VIRTIO_SLOTS {
            let mut i = kept;
            while i > 0 && slots[i - 1].region.base > dev.region.base {
                slots[i] = slots[i - 1];
                i -= 1;
            }
            slots[i] = dev;
            kept += 1;
            continue;
        }

        // Full. Keep the lowest addresses rather than the first eight
        // encountered: QEMU virt describes 32 slots and Akuma's probe walks the
        // low eight, so "the eight the kernel will look at" must not depend on
        // the order this tree happens to list them in.
        if dev.region.base >= slots[MAX_VIRTIO_SLOTS - 1].region.base {
            continue;
        }
        let mut i = MAX_VIRTIO_SLOTS - 1;
        while i > 0 && slots[i - 1].region.base > dev.region.base {
            slots[i] = slots[i - 1];
            i -= 1;
        }
        slots[i] = dev;
    }

    (slots, kept, seen)
}

#[cfg(test)]
mod tests;
