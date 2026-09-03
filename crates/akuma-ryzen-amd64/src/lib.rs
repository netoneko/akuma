//! The amd64 machine description, derived at run time from what the machine says.
//!
//! The x86_64 counterpart of `akuma-firecracker`, and the shape is the same: a
//! `no_std`, allocation-free parser that turns the VMM's own description into a
//! [`MachineDescription`], plus host tests over **measured bytes** so the
//! parsing can be exercised without a VM.
//!
//! What differs is what there is to parse. On aarch64 the machine hands over a
//! device tree and `akuma-fdt` reads it. **x86_64 Firecracker passes no device
//! tree at all.** Its description is three unrelated things:
//!
//! | | where | what it carries |
//! |---|---|---|
//! | [`startinfo`] | PVH block, address in `%ebx` at entry | E820 memory map, command line pointer |
//! | [`cmdline`] | a string that block points at | every virtio-MMIO transport: base, size, IRQ |
//! | [`acpi`] | found by scanning the BIOS window | interrupt controllers, via the MADT |
//!
//! # Why every address here is read and none is a constant
//!
//! This is the finding the reference dumps in
//! `docs/reference/firecracker-amd64/` exist to record, and it is the amd64 twin
//! of the aarch64 GIC redistributor bug (`docs/archive/GICD_IROUTER_ALIASING.md`,
//! where a base pinned to one vCPU made the boot core drive another core's
//! frames). Measured on the Ryzen host at 1/2/4/8 vCPUs, **every ACPI table
//! address moves with the vCPU count** — the MADT grows by one 8-byte entry per
//! CPU and everything packed around it slides:
//!
//! ```text
//!   vCPUs     1         2         4         8
//!   RSDP    0xE0000   0xE0000   0xE0000   0xE0000     <- the only fixed one
//!   XSDT    0xA00A7   0xA00C3   0xA00FB   0xA016B
//!   FACP    0x9FF17   0x9FF2B   0x9FF53   0x9FFA3
//!   APIC    0xA002B   0xA003F   0xA0067   0xA00B7
//! ```
//!
//! A kernel that pinned any of those to a literal would read the right table at
//! one vCPU count and a neighbour's bytes at another, with no error — the
//! signature check would be the only thing between it and garbage. The virtio
//! transport base moves too, and between *machines* rather than vCPU counts:
//! `0xc0001000` on Firecracker, `0xfeb00000` on QEMU `microvm`.
//!
//! # About the name
//!
//! `akuma-ryzen-amd64` is named for the machine this was measured on — an AMD
//! Ryzen 7 8845HS running Firecracker v1.16.1. Nothing in it is Ryzen-specific
//! or even AMD-specific: it parses the PVH handoff, the Linux `virtio_mmio.device`
//! grammar and ACPI, all of which are architectural. It is validated against
//! **two** VMMs on purpose (Firecracker and QEMU `microvm`), because a parser
//! checked against one machine is a description of that machine.
//!
//! # Design
//!
//! * **`no_std`, no allocation, no dependencies.** [`describe`] runs before the
//!   heap exists — deliberately, since the heap is placed using the memory map
//!   it returns. Everything is fixed-size.
//! * **Reads go through [`mem::PhysMem`].** The description is scattered across
//!   physical memory rather than being one blob, so a `&[u8]` cannot express it.
//!   A trait can, and it is what makes the host tests possible.
//! * **VMM input is never trusted.** Every length is bounds-checked, every
//!   arithmetic is `checked_`, and a malformed record is a refusal rather than a
//!   guess. A base address off by a nibble is a device mapping pointed at
//!   someone else's memory.

#![no_std]
// Zero `unsafe`, and it can stay that way. Every dangerous operation this crate
// would otherwise need — dereferencing a VMM-supplied physical address — is on
// the far side of [`mem::PhysMem`], which the *caller* implements. The kernel's
// impl is three lines with one bounds check (`amd64/src/machine.rs`); the tests'
// impl is a list of byte spans. That split is what makes a parser of hostile,
// attacker-adjacent input host-testable and provably memory-safe at the same
// time. `forbid`, not `deny`, so no module can opt back in.
#![forbid(unsafe_code)]

pub mod acpi;
pub mod cmdline;
pub mod mem;
pub mod startinfo;

pub use acpi::{IoApic, Madt, Rsdp, TableHeader};
pub use cmdline::{MmioDevice, MmioDevices};
pub use mem::PhysMem;
pub use startinfo::{MemRegion, StartInfo};

/// Most memory regions recorded. Measured: Firecracker reports 4, QEMU
/// `microvm` 6. Eight is headroom without being a guess at a bigger machine.
pub const MAX_REGIONS: usize = 16;

/// Everything the machine said about itself.
pub struct MachineDescription {
    /// The PVH handoff block.
    pub start_info: StartInfo,
    regions: [MemRegion; MAX_REGIONS],
    region_count: usize,
    /// virtio-MMIO transports, from the command line.
    pub virtio: MmioDevices,
    /// The ACPI root pointer, if this machine has ACPI at all.
    pub rsdp: Option<Rsdp>,
    /// The MADT's contents: local APIC address, I/O APICs, enabled CPUs.
    pub madt: Option<Madt>,
}

impl MachineDescription {
    /// Every memory region the machine reported, in the order it reported them.
    #[must_use]
    pub fn regions(&self) -> &[MemRegion] {
        &self.regions[..self.region_count]
    }

    /// Total usable RAM.
    #[must_use]
    pub fn usable_ram(&self) -> u64 {
        self.regions()
            .iter()
            .filter(|r| r.is_ram())
            .fold(0u64, |acc, r| acc.saturating_add(r.size))
    }

    /// The RAM region containing `pa`, chosen by **containment** rather than by
    /// size.
    ///
    /// The largest usable region is very nearly always the right one, but "the
    /// region holding the kernel" is right by construction — picking any other
    /// would hand a frame allocator memory while the kernel image sits somewhere
    /// it has never heard of.
    #[must_use]
    pub fn region_containing(&self, pa: u64) -> Option<MemRegion> {
        self.regions()
            .iter()
            .copied()
            .find(|r| r.is_ram() && r.addr <= pa && pa < r.end())
    }

    /// The vCPU count, as the MADT reports it. `None` without ACPI.
    #[must_use]
    pub fn cpu_count(&self) -> Option<usize> {
        self.madt.as_ref().map(|m| m.cpus().len())
    }

    /// The first I/O APIC's MMIO address, which is what device interrupts need.
    #[must_use]
    pub fn ioapic_addr(&self) -> Option<u32> {
        self.madt.as_ref()?.ioapics().first().map(|io| io.addr)
    }
}

/// Why a description could not be built.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Error {
    /// No `hvm_start_info` magic at the given address. Either the pointer is
    /// wrong or the machine did not boot us through PVH.
    NotPvh,
}

/// Read the machine's description.
///
/// `start_info_pa` is the value the PVH entry ABI delivered in `%ebx`.
/// `cmdline_buf` is scratch for the command-line copy; the parsed devices are
/// kept, the string is not, so the buffer may be reused immediately.
///
/// ACPI absence is not an error — a machine with no tables still has RAM, a
/// command line and virtio devices, and everything this kernel does today it
/// does without ACPI.
pub fn describe<M: PhysMem + ?Sized>(
    m: &M,
    start_info_pa: u64,
    cmdline_buf: &mut [u8],
) -> Result<MachineDescription, Error> {
    let start_info = StartInfo::parse(m, start_info_pa).ok_or(Error::NotPvh)?;

    let mut regions = [MemRegion { addr: 0, size: 0, kind: 0 }; MAX_REGIONS];
    let mut region_count = 0;
    for i in 0..start_info.memmap_entries {
        if region_count >= MAX_REGIONS {
            break;
        }
        // A single unreadable entry is skipped rather than aborting the whole
        // description: the rest of the map is still usable, and refusing to boot
        // over one bad row would be worse than booting with less memory.
        if let Some(r) = start_info.memmap_entry(m, i) {
            regions[region_count] = r;
            region_count += 1;
        }
    }

    let virtio = start_info
        .cmdline(m, cmdline_buf)
        .map_or_else(MmioDevices::new, cmdline::parse);

    // Scanned, not read from `start_info.rsdp_paddr` — that field is 0 on both
    // machines. See `acpi`.
    let rsdp = acpi::find_rsdp(m);
    let madt = rsdp
        .as_ref()
        .and_then(|r| acpi::find_table(m, r, b"APIC"))
        .and_then(|t| acpi::parse_madt(m, &t));

    Ok(MachineDescription { start_info, regions, region_count, virtio, rsdp, madt })
}

#[cfg(test)]
mod tests;
