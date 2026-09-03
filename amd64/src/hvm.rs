//! The PVH `hvm_start_info` block: what the machine tells us at handoff.
//!
//! This is the amd64 answer to the question `akuma-fdt` answers on aarch64, and
//! it is a much smaller question. x86_64 Firecracker passes **no device tree**,
//! but PVH does not leave the guest to discover RAM on its own either: the block
//! carries an E820-shaped memory map outright, so there is no BIOS `int 15h`
//! call to make, no `e820` table to scavenge, and — for the memory map at least
//! — **no ACPI to parse**.
//!
//! It also hands over `rsdp_paddr`. When ACPI does become necessary (IOAPIC
//! discovery, SMP via MADT, PCI via MCFG) the root pointer arrives as a field,
//! so even then the usual "scan the BIOS area for `RSD PTR `" step does not
//! exist on this machine.
//!
//! # Layout
//!
//! Fixed by the x86 HVM direct boot ABI; mirrored from rust-vmm `linux-loader`'s
//! `start_info.rs`, which is the definition Firecracker actually writes.
//!
//! ```text
//! 0x00 magic          u32   0x336e_c578 ("xEn3" little-endian)
//! 0x04 version        u32   0 ends at rsdp_paddr; 1+ has the memmap fields
//! 0x08 flags          u32
//! 0x0c nr_modules     u32
//! 0x10 modlist_paddr  u64
//! 0x18 cmdline_paddr  u64
//! 0x20 rsdp_paddr     u64
//! 0x28 memmap_paddr   u64   (v1+)
//! 0x30 memmap_entries u32   (v1+)
//! 0x34 reserved       u32
//! ```
//!
//! Entries are 24 bytes: `addr: u64`, `size: u64`, `type: u32`, `reserved: u32`.
//!
//! # Reading physical addresses
//!
//! Every address in this block is physical, and the kernel reaches physical
//! memory through the physmap (`crate::phys`). Each read is bounds-checked
//! against that window rather than trusted, because these values come from
//! outside the kernel: a pointer past the physmap is a page fault with no IDT
//! installed, i.e. an immediate triple-fault and a guest that vanishes with no
//! output. Refusing to read is recoverable; faulting is not.

/// `"xEn3"` little-endian. A block that does not start with this is not one.
const HVM_MAGIC: u32 = 0x336e_c578;

use crate::phys::{PHYSMAP_LIMIT, phys_ptr};

const MEMMAP_ENTRY_SIZE: u64 = 24;

/// One E820-shaped region.
#[derive(Copy, Clone)]
pub struct MemRegion {
    pub addr: u64,
    pub size: u64,
    pub kind: u32,
}

impl MemRegion {
    /// Type 1 is the only kind that is ours to allocate from.
    pub const fn is_ram(self) -> bool {
        self.kind == 1
    }

    /// Human-readable type, for the boot log.
    ///
    /// A fixed table rather than a number: the whole reason to print the map is
    /// to eyeball it, and "3" does not tell you that a region is where the ACPI
    /// tables live.
    pub const fn kind_str(self) -> &'static str {
        match self.kind {
            1 => "RAM",
            2 => "reserved",
            3 => "ACPI",
            4 => "NVS",
            5 => "unusable",
            6 => "disabled",
            7 => "pmem",
            _ => "unknown",
        }
    }
}

/// Read `len` bytes at a physical address, if it is inside the physmap.
///
/// # Safety
/// The caller must not assume the value is meaningful — only that reading it did
/// not fault. Every field read here is attacker-adjacent in the sense that it
/// comes from the VMM, not from this kernel.
unsafe fn read_phys<const N: usize>(pa: u64) -> Option<[u8; N]> {
    let n = N as u64;
    if pa.checked_add(n)? > PHYSMAP_LIMIT {
        return None;
    }
    let mut buf = [0u8; N];
    for (i, slot) in buf.iter_mut().enumerate() {
        // SAFETY: `pa + N` was just proved to be inside the physmap, so the
        // translated address is mapped and readable.
        *slot = unsafe { phys_ptr::<u8>(pa + i as u64).read_volatile() };
    }
    Some(buf)
}

unsafe fn read_u32(pa: u64) -> Option<u32> {
    // SAFETY: delegated to `read_phys`, which bounds-checks.
    unsafe { read_phys::<4>(pa) }.map(u32::from_le_bytes)
}

unsafe fn read_u64(pa: u64) -> Option<u64> {
    // SAFETY: delegated to `read_phys`, which bounds-checks.
    unsafe { read_phys::<8>(pa) }.map(u64::from_le_bytes)
}

/// The parsed handoff block.
pub struct StartInfo {
    pub version: u32,
    pub nr_modules: u32,
    pub cmdline_paddr: u64,
    pub rsdp_paddr: u64,
    pub memmap_paddr: u64,
    pub memmap_entries: u32,
}

impl StartInfo {
    /// Parse the block at `pa`, or `None` if the magic does not match.
    ///
    /// # Safety
    /// `pa` must be the value the PVH entry ABI delivered in `%ebx`.
    pub unsafe fn read(pa: u64) -> Option<Self> {
        // SAFETY: bounds-checked inside; a bad `pa` yields None, not a fault.
        unsafe {
            if read_u32(pa)? != HVM_MAGIC {
                return None;
            }
            let version = read_u32(pa + 0x04)?;
            // v0 stops after rsdp_paddr. Reading the memmap fields anyway would
            // be reading whatever the VMM left there.
            let (memmap_paddr, memmap_entries) = if version >= 1 {
                (read_u64(pa + 0x28)?, read_u32(pa + 0x30)?)
            } else {
                (0, 0)
            };
            Some(Self {
                version,
                nr_modules: read_u32(pa + 0x0c)?,
                cmdline_paddr: read_u64(pa + 0x18)?,
                rsdp_paddr: read_u64(pa + 0x20)?,
                memmap_paddr,
                memmap_entries,
            })
        }
    }

    /// One memory-map entry, or `None` if out of range or unreadable.
    ///
    /// # Safety
    /// Reads the map this block points at; see the module note on physical reads.
    pub unsafe fn memmap_entry(&self, i: u32) -> Option<MemRegion> {
        if i >= self.memmap_entries {
            return None;
        }
        let base = self
            .memmap_paddr
            .checked_add(u64::from(i) * MEMMAP_ENTRY_SIZE)?;
        // SAFETY: bounds-checked inside each read.
        unsafe {
            Some(MemRegion {
                addr: read_u64(base)?,
                size: read_u64(base + 0x08)?,
                kind: read_u32(base + 0x10)?,
            })
        }
    }
}
