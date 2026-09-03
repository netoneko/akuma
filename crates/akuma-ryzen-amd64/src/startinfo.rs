//! The PVH `hvm_start_info` block: what the machine tells us at handoff.
//!
//! This is the amd64 answer to the question `akuma-fdt` answers on aarch64, and
//! it is a much smaller question. x86_64 Firecracker passes **no device tree**,
//! but PVH does not leave the guest to discover RAM on its own either: the block
//! carries an E820-shaped memory map outright, so there is no BIOS `int 15h`
//! call to make and no table to scavenge.
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
//! # Measured
//!
//! ```text
//!               block at    cmdline at   rsdp_paddr   entries
//!   Firecracker  0x6000      0x20000      0            4
//!   QEMU microvm 0x1580      0x560        0            6
//! ```
//!
//! `rsdp_paddr` is **0 on both**, which is why [`super::acpi`] scans instead of
//! reading it. The field exists in the ABI and neither VMM fills it in.

use crate::mem::{PhysMem, read_u32, read_u64};

/// `"xEn3"` little-endian. A block that does not start with this is not one.
const HVM_MAGIC: u32 = 0x336e_c578;
const MEMMAP_ENTRY_SIZE: u64 = 24;

/// One E820-shaped region.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct MemRegion {
    pub addr: u64,
    pub size: u64,
    pub kind: u32,
}

impl MemRegion {
    /// Type 1 is the only kind that is ours to allocate from.
    #[must_use]
    pub const fn is_ram(self) -> bool {
        self.kind == 1
    }

    /// End address, saturating rather than wrapping: a VMM-supplied
    /// `addr + size` that overflows must not become a region that appears to
    /// start after it ends.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.addr.saturating_add(self.size)
    }

    #[must_use]
    pub const fn kind_str(self) -> &'static str {
        match self.kind {
            1 => "RAM",
            2 => "reserved",
            3 => "ACPI reclaimable",
            4 => "ACPI NVS",
            5 => "unusable",
            _ => "unknown",
        }
    }
}

/// The parsed handoff block.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct StartInfo {
    /// Physical address of the block itself.
    pub addr: u64,
    pub version: u32,
    pub nr_modules: u32,
    pub cmdline_paddr: u64,
    /// Advertised RSDP address. Measured as 0 on both machines — see the module
    /// header; do not build on it.
    pub rsdp_paddr: u64,
    pub memmap_paddr: u64,
    pub memmap_entries: u32,
}

impl StartInfo {
    /// Parse the block at `pa`, or `None` if the magic does not match.
    #[must_use]
    pub fn parse<M: PhysMem + ?Sized>(m: &M, pa: u64) -> Option<Self> {
        if read_u32(m, pa)? != HVM_MAGIC {
            return None;
        }
        let version = read_u32(m, pa + 0x04)?;
        // v0 stops after rsdp_paddr. Reading the memmap fields anyway would be
        // reading whatever the VMM left there.
        let (memmap_paddr, memmap_entries) = if version >= 1 {
            (read_u64(m, pa + 0x28)?, read_u32(m, pa + 0x30)?)
        } else {
            (0, 0)
        };
        Some(Self {
            addr: pa,
            version,
            nr_modules: read_u32(m, pa + 0x0c)?,
            cmdline_paddr: read_u64(m, pa + 0x18)?,
            rsdp_paddr: read_u64(m, pa + 0x20)?,
            memmap_paddr,
            memmap_entries,
        })
    }

    /// One memory-map entry, or `None` if out of range or unreadable.
    #[must_use]
    pub fn memmap_entry<M: PhysMem + ?Sized>(&self, m: &M, i: u32) -> Option<MemRegion> {
        if i >= self.memmap_entries {
            return None;
        }
        let base = self.memmap_paddr.checked_add(u64::from(i) * MEMMAP_ENTRY_SIZE)?;
        Some(MemRegion {
            addr: read_u64(m, base)?,
            size: read_u64(m, base + 0x08)?,
            kind: read_u32(m, base + 0x10)?,
        })
    }

    /// Copy the boot command line into `buf`, returning it as a `str`.
    ///
    /// Bounded and copied rather than borrowed: the bytes live in VMM-written
    /// physical memory that nothing stops from being reused, and a `&str`
    /// pointing there would be a borrow of memory this kernel does not own.
    ///
    /// Returns `None` if there is no command line, if it is unreadable, or if it
    /// is not UTF-8. Invalid UTF-8 is a refusal rather than a lossy decode:
    /// every consumer matches ASCII tokens, and a replacement character in the
    /// middle of an address is worse than no address.
    #[must_use]
    pub fn cmdline<'a, M: PhysMem + ?Sized>(&self, m: &M, buf: &'a mut [u8]) -> Option<&'a str> {
        if self.cmdline_paddr == 0 || buf.is_empty() {
            return None;
        }
        let mut len = 0;
        while len < buf.len() {
            let pa = self.cmdline_paddr.checked_add(len as u64)?;
            let mut one = [0u8; 1];
            if !m.read(pa, &mut one) {
                return None;
            }
            if one[0] == 0 {
                break;
            }
            buf[len] = one[0];
            len += 1;
        }
        if len == 0 {
            return None;
        }
        core::str::from_utf8(&buf[..len]).ok()
    }
}
