//! ACPI: finding the tables, and reading the one that matters.
//!
//! # Why this is a scan and not a field read
//!
//! The PVH handoff block has an `rsdp_paddr` field designed to answer exactly
//! this question. It is **0 on both machines** — QEMU and Firecracker v1.16.1,
//! measured — so the field exists in the ABI and neither VMM fills it in. The
//! root pointer has to be found the way a BIOS-era kernel finds it, by looking
//! for a signature in the two places ACPI 6.x §5.2.5.1 names:
//!
//! 1. the first KiB of the Extended BIOS Data Area, whose segment address is a
//!    16-bit value at physical `0x40E`, shifted left 4;
//! 2. `0x000E_0000 .. 0x000F_FFFF`, the BIOS read-only area.
//!
//! Measured, on the Ryzen host: Firecracker puts it at exactly `0x000E_0000`,
//! the first address of the second window, with OEM id `FIRECK`. QEMU `microvm`
//! puts it at `0x000F_5590`, OEM `BOCHS`.
//!
//! # Nothing here may be a constant
//!
//! This is the finding the reference dumps exist to record, and it is the amd64
//! twin of the aarch64 GIC redistributor bug
//! (`docs/archive/GICD_IROUTER_ALIASING.md`): **every table address moves with
//! the vCPU count.** Measured on Firecracker at 1/2/4/8 vCPUs:
//!
//! ```text
//!   vCPUs     1         2         4         8
//!   RSDP    0xE0000   0xE0000   0xE0000   0xE0000     <- the only fixed one
//!   XSDT    0xA00A7   0xA00C3   0xA00FB   0xA016B
//!   FACP    0x9FF17   0x9FF2B   0x9FF53   0x9FFA3
//!   APIC    0xA002B   0xA003F   0xA0067   0xA00B7
//!   MADT len   0x40      0x48      0x58      0x78     <- 0x38 + 8 per vCPU
//! ```
//!
//! The MADT grows by one 8-byte Local APIC entry per vCPU and everything packed
//! around it slides. A kernel that pinned any of these to a literal would read
//! the right table at one vCPU count and a neighbouring table's bytes at
//! another — with no error, because the signature check would be the only thing
//! standing between it and garbage. Start at the RSDP, walk, match on signature.

use crate::mem::{PhysMem, read_n, read_u16, read_u32, read_u64};

/// `"RSD PTR "` — the Root System Description Pointer signature.
const RSDP_SIG: [u8; 8] = *b"RSD PTR ";
/// Physical address of the 16-bit EBDA segment pointer.
const EBDA_SEGMENT_PTR: u64 = 0x40E;
/// The BIOS read-only search window, `[start, end)`.
const BIOS_SEARCH: (u64, u64) = (0x000E_0000, 0x0010_0000);
/// Smallest legal system description table: the 36-byte common header.
const TABLE_HEADER_LEN: u32 = 36;

/// The Root System Description Pointer.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Rsdp {
    /// Physical address of the structure itself.
    pub addr: u64,
    /// 0 for ACPI 1.0 (32-bit RSDT only), 2+ for 2.0+ (64-bit XSDT present).
    pub revision: u8,
    /// 32-bit Root System Description Table pointer.
    pub rsdt: u32,
    /// 64-bit Extended System Description Table pointer; 0 when revision is 0
    /// or the extended checksum failed.
    pub xsdt: u64,
    /// The six-byte OEM id, as written. `FIRECK` on Firecracker, `BOCHS ` on
    /// QEMU — the cheapest way to tell which machine a dump came from.
    pub oem: [u8; 6],
}

/// A system description table header.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct TableHeader {
    pub signature: [u8; 4],
    pub length: u32,
    pub addr: u64,
}

/// Sum of `len` bytes at `pa`; ACPI requires the low byte to be zero.
///
/// Not decoration. `"RSD PTR "` is eight bytes of ASCII that can occur in a
/// BIOS blob by coincidence, and following a bogus pointer means walking a table
/// array built from whatever happened to follow it.
fn checksum_ok<M: PhysMem + ?Sized>(m: &M, pa: u64, len: usize) -> bool {
    let mut sum: u8 = 0;
    let mut i = 0usize;
    // A byte at a time rather than a big buffer: this crate allocates nothing,
    // and the largest structure checked here is a few dozen bytes.
    while i < len {
        let Some([b]) = read_n::<1, M>(m, pa.wrapping_add(i as u64)) else {
            return false;
        };
        sum = sum.wrapping_add(b);
        i += 1;
    }
    sum == 0
}

/// Try to read an RSDP at `pa`, validating signature and both checksums.
fn rsdp_at<M: PhysMem + ?Sized>(m: &M, pa: u64) -> Option<Rsdp> {
    let head = read_n::<20, M>(m, pa)?;
    if head[..8] != RSDP_SIG {
        return None;
    }
    // The ACPI 1.0 checksum covers the first 20 bytes, whatever the revision.
    if !checksum_ok(m, pa, 20) {
        return None;
    }
    let revision = head[15];
    let rsdt = u32::from_le_bytes([head[16], head[17], head[18], head[19]]);
    let mut oem = [0u8; 6];
    oem.copy_from_slice(&head[9..15]);

    // Revision 2+ extends the structure, and only its own checksum makes the
    // XSDT pointer trustworthy. A failure here degrades to the RSDT rather than
    // rejecting the RSDP: a 32-bit pointer to the tables is still a working
    // answer on a machine whose tables are all below 4 GiB, which both of these
    // are.
    let xsdt = if revision >= 2 {
        let length = read_u32(m, pa + 20)? as usize;
        if length >= 33 && checksum_ok(m, pa, length) {
            read_u64(m, pa + 24)?
        } else {
            0
        }
    } else {
        0
    };

    Some(Rsdp { addr: pa, revision, rsdt, xsdt, oem })
}

/// Scan the two architectural windows for an RSDP.
///
/// Returns `None` on a machine with no ACPI, which is a legitimate
/// configuration rather than a failure — a caller must be able to boot without
/// it, because everything this kernel does today it does without it.
#[must_use]
pub fn find_rsdp<M: PhysMem + ?Sized>(m: &M) -> Option<Rsdp> {
    if let Some(seg) = read_u16(m, EBDA_SEGMENT_PTR) {
        let base = u64::from(seg) << 4;
        // Below 0x400 is the interrupt vector table and the BIOS data area, not
        // an EBDA; a zero or tiny segment value means "no EBDA", not "look at
        // address 0".
        if base >= 0x400 {
            let mut pa = base;
            while pa < base + 1024 {
                if let Some(r) = rsdp_at(m, pa) {
                    return Some(r);
                }
                pa += 16;
            }
        }
    }

    let mut pa = BIOS_SEARCH.0;
    while pa < BIOS_SEARCH.1 {
        if let Some(r) = rsdp_at(m, pa) {
            return Some(r);
        }
        pa += 16;
    }
    None
}

/// Read one table header, if it is readable and self-consistent.
#[must_use]
pub fn table_at<M: PhysMem + ?Sized>(m: &M, pa: u64) -> Option<TableHeader> {
    let head = read_n::<8, M>(m, pa)?;
    let length = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    // A table shorter than its own header is not a table.
    if length < TABLE_HEADER_LEN {
        return None;
    }
    Some(TableHeader { signature: [head[0], head[1], head[2], head[3]], length, addr: pa })
}

/// Visit every table the XSDT (or RSDT) points at.
///
/// Prefers the XSDT when one is advertised: its entries are 64-bit, and a table
/// above 4 GiB cannot be named by the RSDT at all. Both are walked identically
/// otherwise, which is why this is one function with an entry stride rather than
/// two.
pub fn for_each_table<M: PhysMem + ?Sized>(m: &M, rsdp: &Rsdp, mut f: impl FnMut(&TableHeader)) {
    let (root, entry_size) = if rsdp.xsdt != 0 {
        (rsdp.xsdt, 8usize)
    } else {
        (u64::from(rsdp.rsdt), 4usize)
    };
    if root == 0 {
        return;
    }
    let Some(header) = table_at(m, root) else {
        return;
    };
    let entries = (header.length as usize).saturating_sub(TABLE_HEADER_LEN as usize) / entry_size;
    for i in 0..entries {
        let at = root + u64::from(TABLE_HEADER_LEN) + (i * entry_size) as u64;
        let pa = if entry_size == 8 {
            read_u64(m, at)
        } else {
            read_u32(m, at).map(u64::from)
        };
        let Some(pa) = pa else { continue };
        if pa == 0 {
            continue;
        }
        if let Some(t) = table_at(m, pa) {
            f(&t);
        }
    }
}

/// Find one table by signature, e.g. `b"APIC"` for the MADT.
#[must_use]
pub fn find_table<M: PhysMem + ?Sized>(m: &M, rsdp: &Rsdp, sig: &[u8; 4]) -> Option<TableHeader> {
    let mut found = None;
    for_each_table(m, rsdp, |t| {
        if t.signature == *sig && found.is_none() {
            found = Some(*t);
        }
    });
    found
}

/// Most IOAPICs this will report. One is what both machines have.
pub const MAX_IOAPICS: usize = 4;
/// Most CPUs this will report, matching the largest vCPU count measured.
pub const MAX_CPUS: usize = 32;

/// An I/O APIC, from a MADT type-1 entry.
///
/// This is the reason to parse the MADT at all: device interrupts on x86_64 are
/// routed through one, and its MMIO address is written down nowhere else. Linux
/// reports `IOAPIC[0]: apic_id 0, version 17, address 0xfec00000, GSI 0-23` on
/// both machines — but that address is *read from here*, not assumed, and this
/// crate does the same.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct IoApic {
    pub id: u8,
    /// MMIO base address.
    pub addr: u32,
    /// First global system interrupt this IOAPIC handles.
    pub gsi_base: u32,
}

/// What the MADT says.
#[derive(Copy, Clone)]
pub struct Madt {
    /// Physical address of the local APIC register page, as the MADT reports it.
    pub local_apic_addr: u32,
    ioapics: [IoApic; MAX_IOAPICS],
    ioapic_count: usize,
    /// APIC ids of the processors marked enabled.
    cpu_ids: [u8; MAX_CPUS],
    cpu_count: usize,
}

impl Madt {
    #[must_use]
    pub fn ioapics(&self) -> &[IoApic] {
        &self.ioapics[..self.ioapic_count]
    }

    /// APIC ids of every enabled processor. Its length is the vCPU count, which
    /// is the one thing in this table that changes between boots of the same
    /// machine.
    #[must_use]
    pub fn cpus(&self) -> &[u8] {
        &self.cpu_ids[..self.cpu_count]
    }
}

/// Parse the MADT (`APIC`) table.
///
/// Entry format, ACPI 6.x §5.2.12: a 44-byte header (36 common + a 4-byte local
/// APIC address + 4-byte flags), then a list of `(type, length, ...)` records.
/// Type 0 is a Processor Local APIC, type 1 an I/O APIC.
///
/// A record whose `length` is 0 would make the walk spin forever on VMM-supplied
/// bytes, so that terminates the parse rather than being skipped.
#[must_use]
pub fn parse_madt<M: PhysMem + ?Sized>(m: &M, table: &TableHeader) -> Option<Madt> {
    const HEADER_LEN: u32 = 44;
    if table.length < HEADER_LEN {
        return None;
    }
    let local_apic_addr = read_u32(m, table.addr + 36)?;

    let mut out = Madt {
        local_apic_addr,
        ioapics: [IoApic { id: 0, addr: 0, gsi_base: 0 }; MAX_IOAPICS],
        ioapic_count: 0,
        cpu_ids: [0; MAX_CPUS],
        cpu_count: 0,
    };

    let mut off = HEADER_LEN;
    while off + 2 <= table.length {
        let rec = read_n::<2, M>(m, table.addr + u64::from(off))?;
        let (kind, len) = (rec[0], u32::from(rec[1]));
        if len < 2 || off + len > table.length {
            break;
        }
        let at = table.addr + u64::from(off);
        match kind {
            // Processor Local APIC: acpi_id, apic_id, flags(u32). Bit 0 of flags
            // is "enabled"; a disabled entry describes a socket, not a CPU.
            0 if len >= 8 => {
                let body = read_n::<6, M>(m, at + 2)?;
                let flags = u32::from_le_bytes([body[2], body[3], body[4], body[5]]);
                if flags & 1 != 0 && out.cpu_count < MAX_CPUS {
                    out.cpu_ids[out.cpu_count] = body[1];
                    out.cpu_count += 1;
                }
            }
            // I/O APIC: id, reserved, address(u32), gsi_base(u32).
            1 if len >= 12 => {
                let body = read_n::<10, M>(m, at + 2)?;
                if out.ioapic_count < MAX_IOAPICS {
                    out.ioapics[out.ioapic_count] = IoApic {
                        id: body[0],
                        addr: u32::from_le_bytes([body[2], body[3], body[4], body[5]]),
                        gsi_base: u32::from_le_bytes([body[6], body[7], body[8], body[9]]),
                    };
                    out.ioapic_count += 1;
                }
            }
            _ => {}
        }
        off += len;
    }
    Some(out)
}
