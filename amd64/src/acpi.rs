//! Finding the machine's ACPI tables, when it has any.
//!
//! # Why this is a *scan* and not a field read
//!
//! The PVH handoff block has an `rsdp_paddr` field designed to answer exactly
//! this question, and `amd64/src/hvm.rs` reads it. It is **0 on both machines** —
//! QEMU and Firecracker v1.16.1, measured — so the field exists in the ABI and
//! neither VMM fills it in. That measurement is recorded in
//! `docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.6 with the instruction "do not
//! build on that field", and this module is what happens when you take that
//! seriously: the root pointer has to be found the way a BIOS-era kernel finds
//! it, by looking for a signature in two well-known places.
//!
//! # Where to look, per ACPI 6.x §5.2.5.1
//!
//! 1. The first KiB of the Extended BIOS Data Area, whose segment address is a
//!    16-bit value at physical `0x40E` (shifted left 4).
//! 2. `0x000E_0000 .. 0x000F_FFFF`, the BIOS read-only area.
//!
//! In both, the structure is 16-byte aligned and starts `"RSD PTR "`. Both
//! windows are inside the first MiB and therefore inside the physmap, so this
//! needs no mapping of its own.
//!
//! # What it is for
//!
//! Nothing yet, and that is deliberate — this reports rather than configures.
//! The consumer is the **IOAPIC**: device interrupts on x86_64 need one, finding
//! it needs the MADT, and the MADT is reached through here. The block driver
//! polls precisely because that chain is not built, so knowing whether the chain
//! is even *possible* on this machine is the thing worth measuring first.

use crate::phys::{PHYSMAP_LIMIT, phys_ptr};

/// `"RSD PTR "` — the Root System Description Pointer signature.
const RSDP_SIG: [u8; 8] = *b"RSD PTR ";

/// Physical address of the 16-bit EBDA segment pointer.
const EBDA_SEGMENT_PTR: u64 = 0x40E;
/// The BIOS read-only search window.
const BIOS_SEARCH: (u64, u64) = (0x000E_0000, 0x0010_0000);

/// What a successful scan found.
#[derive(Copy, Clone)]
pub struct Rsdp {
    /// Physical address of the RSDP structure itself.
    pub addr: u64,
    /// 0 for ACPI 1.0 (32-bit RSDT only), 2+ for 2.0+ (64-bit XSDT present).
    pub revision: u8,
    /// 32-bit Root System Description Table pointer.
    pub rsdt: u32,
    /// 64-bit Extended System Description Table pointer; 0 when revision is 0.
    pub xsdt: u64,
    /// The six-byte OEM id, as written.
    pub oem: [u8; 6],
}

/// Read `N` bytes at a physical address inside the physmap.
fn read<const N: usize>(pa: u64) -> Option<[u8; N]> {
    if pa.checked_add(N as u64)? > PHYSMAP_LIMIT {
        return None;
    }
    let mut buf = [0u8; N];
    for (i, slot) in buf.iter_mut().enumerate() {
        // SAFETY: the whole range was just proved to be inside the physmap.
        *slot = unsafe { phys_ptr::<u8>(pa + i as u64).read_volatile() };
    }
    Some(buf)
}

/// Sum of `len` bytes at `pa`, as ACPI defines a checksum: the low byte of the
/// sum of every byte in the structure must be zero.
fn checksum_ok(pa: u64, len: usize) -> bool {
    let mut sum: u8 = 0;
    for i in 0..len {
        let Some([b]) = read::<1>(pa + i as u64) else {
            return false;
        };
        sum = sum.wrapping_add(b);
    }
    sum == 0
}

/// Try to read an RSDP at `pa`, validating signature and checksum.
///
/// The checksum is not decoration here. `"RSD PTR "` is eight bytes of ASCII
/// that can appear in a BIOS blob by coincidence, and following a bogus pointer
/// means walking a table array built from whatever followed it.
fn at(pa: u64) -> Option<Rsdp> {
    let head = read::<20>(pa)?;
    if head[..8] != RSDP_SIG {
        return None;
    }
    // ACPI 1.0 checksum covers the first 20 bytes.
    if !checksum_ok(pa, 20) {
        return None;
    }
    let revision = head[15];
    let rsdt = u32::from_le_bytes([head[16], head[17], head[18], head[19]]);

    let mut oem = [0u8; 6];
    oem.copy_from_slice(&head[9..15]);

    // Revision 2+ extends the structure; its own checksum covers `length` bytes,
    // and only then is the XSDT pointer trustworthy.
    let xsdt = if revision >= 2 {
        let ext = read::<16>(pa + 20)?;
        let length = u32::from_le_bytes([ext[0], ext[1], ext[2], ext[3]]) as usize;
        if length < 33 || !checksum_ok(pa, length) {
            0
        } else {
            u64::from_le_bytes([
                ext[4], ext[5], ext[6], ext[7], ext[8], ext[9], ext[10], ext[11],
            ])
        }
    } else {
        0
    };

    Some(Rsdp { addr: pa, revision, rsdt, xsdt, oem })
}

/// Scan the two architectural windows for an RSDP.
///
/// Returns `None` on a machine with no ACPI at all, which is a legitimate
/// configuration rather than a failure — and, as of this stage, the measured
/// answer on at least one of the two machines. A caller must be able to boot
/// without it.
#[must_use]
pub fn find_rsdp() -> Option<Rsdp> {
    // The EBDA, if the BIOS data area names one.
    if let Some(seg) = read::<2>(EBDA_SEGMENT_PTR).map(u16::from_le_bytes) {
        let base = u64::from(seg) << 4;
        if base >= 0x400 {
            let mut pa = base;
            while pa < base + 1024 {
                if let Some(r) = at(pa) {
                    return Some(r);
                }
                pa += 16;
            }
        }
    }

    let mut pa = BIOS_SEARCH.0;
    while pa < BIOS_SEARCH.1 {
        if let Some(r) = at(pa) {
            return Some(r);
        }
        pa += 16;
    }
    None
}

/// A system description table header: four-byte signature, then a length.
#[derive(Copy, Clone)]
pub struct TableHeader {
    pub signature: [u8; 4],
    pub length: u32,
    pub addr: u64,
}

/// Read one table header, if it is inside the physmap and self-consistent.
#[must_use]
pub fn table_at(pa: u64) -> Option<TableHeader> {
    let head = read::<8>(pa)?;
    let length = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    // A table shorter than its own header is not a table.
    if length < 36 {
        return None;
    }
    Some(TableHeader {
        signature: [head[0], head[1], head[2], head[3]],
        length,
        addr: pa,
    })
}

/// Visit every table the RSDT/XSDT points at.
///
/// Prefers the XSDT when the RSDP advertises one: its entries are 64-bit, and a
/// table above 4 GiB is unrepresentable in the RSDT. Both are walked the same
/// way otherwise, which is why this is one function with a stride.
pub fn for_each_table(rsdp: &Rsdp, mut f: impl FnMut(&TableHeader)) {
    let (root, entry_size) = if rsdp.xsdt != 0 { (rsdp.xsdt, 8) } else { (u64::from(rsdp.rsdt), 4) };
    if root == 0 {
        return;
    }
    let Some(header) = table_at(root) else {
        return;
    };
    let entries = (header.length as usize).saturating_sub(36) / entry_size;
    for i in 0..entries {
        let at = root + 36 + (i * entry_size) as u64;
        let pa = if entry_size == 8 {
            read::<8>(at).map(u64::from_le_bytes)
        } else {
            read::<4>(at).map(|b| u64::from(u32::from_le_bytes(b)))
        };
        let Some(pa) = pa else { continue };
        if let Some(t) = table_at(pa) {
            f(&t);
        }
    }
}
