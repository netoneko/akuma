//! Host tests, over bytes measured on real machines.
//!
//! The point of this crate is that none of this needs a VM. Every fixture below
//! is either a **byte-for-byte reconstruction of a measured machine** — the
//! addresses, lengths and OEM ids come from a boot log, not from imagination —
//! or a hostile input that must be refused.
//!
//! Where the numbers came from:
//!
//! * Firecracker v1.16.1 on an AMD Ryzen 7 8845HS: `docs/reference/firecracker-amd64/`,
//!   captured by `amd64/dump-machine.sh` at 1/2/4/8 vCPUs, plus this kernel's own
//!   boot log.
//! * QEMU `microvm`: `amd64/run.sh`, and `info mtree` / `info qtree` for the
//!   device layout.

use super::*;
use alloc::vec::Vec;

extern crate alloc;
extern crate std;

/// Physical memory as a sparse list of `(base, bytes)` spans.
///
/// Refuses any read that is not wholly inside one span, which is the contract
/// [`PhysMem`] states: a partial fill would leave a parser working on half a
/// structure and half a stale buffer.
#[derive(Default)]
struct FakeMem {
    spans: Vec<(u64, Vec<u8>)>,
}

impl FakeMem {
    fn put(&mut self, base: u64, bytes: &[u8]) -> &mut Self {
        self.spans.push((base, bytes.to_vec()));
        self
    }
}

impl PhysMem for FakeMem {
    fn read(&self, pa: u64, buf: &mut [u8]) -> bool {
        for (base, bytes) in &self.spans {
            if pa >= *base {
                let off = (pa - base) as usize;
                if let Some(src) = bytes.get(off..off + buf.len()) {
                    buf.copy_from_slice(src);
                    return true;
                }
            }
        }
        false
    }
}

/// Build an `hvm_start_info` block.
fn start_info_bytes(cmdline_paddr: u64, rsdp: u64, memmap_paddr: u64, entries: u32) -> Vec<u8> {
    let mut b = alloc::vec![0u8; 0x38];
    b[0x00..0x04].copy_from_slice(&0x336e_c578u32.to_le_bytes());
    b[0x04..0x08].copy_from_slice(&1u32.to_le_bytes()); // version
    b[0x0c..0x10].copy_from_slice(&0u32.to_le_bytes()); // nr_modules
    b[0x18..0x20].copy_from_slice(&cmdline_paddr.to_le_bytes());
    b[0x20..0x28].copy_from_slice(&rsdp.to_le_bytes());
    b[0x28..0x30].copy_from_slice(&memmap_paddr.to_le_bytes());
    b[0x30..0x34].copy_from_slice(&entries.to_le_bytes());
    b
}

fn memmap_bytes(regions: &[(u64, u64, u32)]) -> Vec<u8> {
    let mut b = Vec::new();
    for (addr, size, kind) in regions {
        b.extend_from_slice(&addr.to_le_bytes());
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&kind.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
    }
    b
}

/// Set the last byte so the structure's bytes sum to zero, as ACPI requires.
fn fix_checksum(b: &mut [u8], range: core::ops::Range<usize>, at: usize) {
    b[at] = 0;
    let sum = b[range].iter().fold(0u8, |a, &x| a.wrapping_add(x));
    b[at] = (0u8).wrapping_sub(sum);
}

/// An ACPI 2.0 RSDP with the given OEM id and XSDT pointer.
fn rsdp_bytes(oem: &[u8; 6], xsdt: u64) -> Vec<u8> {
    let mut b = alloc::vec![0u8; 36];
    b[0..8].copy_from_slice(b"RSD PTR ");
    b[9..15].copy_from_slice(oem);
    b[15] = 2; // revision
    b[16..20].copy_from_slice(&0u32.to_le_bytes()); // rsdt
    b[20..24].copy_from_slice(&36u32.to_le_bytes()); // length
    b[24..32].copy_from_slice(&xsdt.to_le_bytes());
    fix_checksum(&mut b, 0..20, 8);
    fix_checksum(&mut b, 0..36, 32);
    b
}

/// A table header plus body.
fn table_bytes(sig: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let len = 36 + body.len();
    let mut b = alloc::vec![0u8; 36];
    b[0..4].copy_from_slice(sig);
    b[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    b.extend_from_slice(body);
    b
}

/// An XSDT naming `tables`.
fn xsdt_bytes(tables: &[u64]) -> Vec<u8> {
    let mut body = Vec::new();
    for t in tables {
        body.extend_from_slice(&t.to_le_bytes());
    }
    table_bytes(b"XSDT", &body)
}

/// A MADT with `cpus` enabled local APICs and one I/O APIC at `ioapic_addr`.
///
/// Sized the way the real one is: `0x38` of header and fixed fields plus 8 bytes
/// per CPU, which is exactly the growth measured at 1/2/4/8 vCPUs (0x40, 0x48,
/// 0x58, 0x78) once the 12-byte I/O APIC record is included.
fn madt_bytes(cpus: u8, ioapic_addr: u32, gsi_base: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0xfee0_0000u32.to_le_bytes()); // local APIC address
    body.extend_from_slice(&1u32.to_le_bytes()); // flags: PCAT_COMPAT
    for id in 0..cpus {
        body.extend_from_slice(&[0, 8, id, id]); // type, len, acpi_id, apic_id
        body.extend_from_slice(&1u32.to_le_bytes()); // flags: enabled
    }
    body.extend_from_slice(&[1, 12, 0, 0]); // type, len, id, reserved
    body.extend_from_slice(&ioapic_addr.to_le_bytes());
    body.extend_from_slice(&gsi_base.to_le_bytes());
    table_bytes(b"APIC", &body)
}

/// The Firecracker machine, reconstructed from its measured boot log.
fn firecracker(cpus: u8) -> FakeMem {
    const RSDP_PA: u64 = 0x000E_0000;
    const XSDT_PA: u64 = 0x000A_00A7;
    const MADT_PA: u64 = 0x000A_002B;
    const CMDLINE_PA: u64 = 0x0002_0000;
    const MEMMAP_PA: u64 = 0x0000_7000;

    let mut m = FakeMem::default();
    m.put(0x6000, &start_info_bytes(CMDLINE_PA, 0, MEMMAP_PA, 4));
    m.put(
        MEMMAP_PA,
        &memmap_bytes(&[
            (0x0000_0000, 0x0009_fc00, 1),
            (0x0009_fc00, 0x0004_0400, 2),
            (0xeec0_0000, 0x1000_0000, 2),
            (0x0010_0000, 0x1ff0_0000, 1),
        ]),
    );
    m.put(CMDLINE_PA, b"pci=off virtio_mmio.device=4K@0xc0001000:5\0");
    m.put(RSDP_PA, &rsdp_bytes(b"FIRECK", XSDT_PA));
    m.put(XSDT_PA, &xsdt_bytes(&[MADT_PA]));
    m.put(MADT_PA, &madt_bytes(cpus, 0xfec0_0000, 0));
    // No EBDA pointer: Firecracker's RSDP is in the BIOS window, and the scan
    // must reach it there.
    m.put(0x40E, &[0u8, 0u8]);
    m
}

#[test]
fn firecracker_start_info_matches_the_measured_boot() {
    let m = firecracker(1);
    let mut buf = [0u8; 256];
    let d = describe(&m, 0x6000, &mut buf).expect("PVH block");

    assert_eq!(d.start_info.version, 1);
    // The field exists and Firecracker leaves it zero. This assertion is the
    // reason `acpi` scans; if it ever starts being filled in, this fails and
    // the scan becomes an optimisation rather than a necessity.
    assert_eq!(d.start_info.rsdp_paddr, 0);
    assert_eq!(d.regions().len(), 4);
    // 0x9fc00 + 0x1ff00000 — the two type-1 regions, and not the MMIO hole.
    assert_eq!(d.usable_ram(), 0x0009_fc00 + 0x1ff0_0000);
}

#[test]
fn a_kernel_at_1mib_lands_in_the_right_region() {
    let m = firecracker(1);
    let mut buf = [0u8; 256];
    let d = describe(&m, 0x6000, &mut buf).unwrap();

    // Containment, not size: the region holding the kernel is the one to
    // allocate from.
    let r = d.region_containing(0x0010_0000).expect("kernel region");
    assert_eq!(r.addr, 0x0010_0000);
    assert_eq!(r.size, 0x1ff0_0000);

    // The MMIO hole is reserved and must never be handed out, even though it is
    // the largest span in the map by address range.
    assert!(d.region_containing(0xeec0_0000).is_none());
}

#[test]
fn firecracker_announces_its_disk_on_the_command_line() {
    let m = firecracker(1);
    let mut buf = [0u8; 256];
    let d = describe(&m, 0x6000, &mut buf).unwrap();

    assert_eq!(d.virtio.len(), 1);
    let dev = d.virtio.as_slice()[0];
    assert_eq!(dev.base, 0xc000_1000);
    assert_eq!(dev.len, 4096);
    assert_eq!(dev.irq, 5);
    assert_eq!(d.virtio.geometry(), Some((0xc000_1000, 4096, 1)));
}

#[test]
fn the_rsdp_is_found_by_scanning_the_bios_window() {
    let m = firecracker(1);
    let mut buf = [0u8; 256];
    let d = describe(&m, 0x6000, &mut buf).unwrap();

    let rsdp = d.rsdp.expect("RSDP");
    assert_eq!(rsdp.addr, 0x000E_0000);
    assert_eq!(rsdp.revision, 2);
    assert_eq!(&rsdp.oem, b"FIRECK");
}

#[test]
fn the_ioapic_address_comes_from_the_madt() {
    let m = firecracker(1);
    let mut buf = [0u8; 256];
    let d = describe(&m, 0x6000, &mut buf).unwrap();

    // 0xfec00000 is the conventional address and Linux reports it on this
    // machine — but it is *read*, not assumed. A machine that moved it would be
    // followed rather than mis-driven.
    assert_eq!(d.ioapic_addr(), Some(0xfec0_0000));
    let madt = d.madt.expect("MADT");
    assert_eq!(madt.local_apic_addr, 0xfee0_0000);
    assert_eq!(madt.ioapics().len(), 1);
    assert_eq!(madt.ioapics()[0].gsi_base, 0);
}

#[test]
fn the_cpu_count_is_the_thing_that_moves() {
    // The measured behaviour: the MADT grows by one 8-byte local-APIC entry per
    // vCPU, which is why every table packed after it slides and no address here
    // can be a constant.
    for cpus in [1u8, 2, 4, 8] {
        let m = firecracker(cpus);
        let mut buf = [0u8; 256];
        let d = describe(&m, 0x6000, &mut buf).unwrap();
        assert_eq!(d.cpu_count(), Some(cpus as usize), "vcpus={cpus}");
        // The IOAPIC does not move with the count. Asserted so that a change
        // would be noticed rather than absorbed.
        assert_eq!(d.ioapic_addr(), Some(0xfec0_0000), "vcpus={cpus}");
    }
}

#[test]
fn qemu_microvm_parses_through_the_same_code() {
    // A different VMM, a different RSDP location (the BIOS window at a
    // different offset), a different OEM id, and a 0x200 virtio stride. One
    // parser: a description validated against a single machine is a description
    // of that machine.
    const RSDP_PA: u64 = 0x000F_5590;
    const XSDT_PA: u64 = 0x1fff_ffb2;
    const MADT_PA: u64 = 0x1fff_ff60;
    let mut m = FakeMem::default();
    m.put(0x1580, &start_info_bytes(0x560, 0, 0x600, 1));
    m.put(0x600, &memmap_bytes(&[(0x0010_0000, 0x1fef_f000, 1)]));
    m.put(0x560, b"virtio_mmio.device=512@0xfeb00000:5\0");
    m.put(RSDP_PA, &rsdp_bytes(b"BOCHS ", XSDT_PA));
    m.put(XSDT_PA, &xsdt_bytes(&[MADT_PA]));
    m.put(MADT_PA, &madt_bytes(1, 0xfec0_0000, 0));
    m.put(0x40E, &[0u8, 0u8]);

    let mut buf = [0u8; 256];
    let d = describe(&m, 0x1580, &mut buf).unwrap();
    assert_eq!(&d.rsdp.unwrap().oem, b"BOCHS ");
    assert_eq!(d.virtio.geometry(), Some((0xfeb0_0000, 512, 1)));
    assert_eq!(d.ioapic_addr(), Some(0xfec0_0000));
}

#[test]
fn a_machine_without_acpi_still_describes_itself() {
    // Not hypothetical: this is every boot of this target before ACPI was
    // looked for, and a caller must be able to come up without it.
    let mut m = FakeMem::default();
    m.put(0x6000, &start_info_bytes(0x2_0000, 0, 0x7000, 1));
    m.put(0x7000, &memmap_bytes(&[(0x0010_0000, 0x1ff0_0000, 1)]));
    m.put(0x2_0000, b"pci=off\0");
    m.put(0x40E, &[0u8, 0u8]);

    let mut buf = [0u8; 256];
    let d = describe(&m, 0x6000, &mut buf).unwrap();
    assert!(d.rsdp.is_none());
    assert!(d.madt.is_none());
    assert_eq!(d.cpu_count(), None);
    assert_eq!(d.usable_ram(), 0x1ff0_0000);
}

#[test]
fn a_bad_rsdp_checksum_is_not_followed() {
    // "RSD PTR " is eight bytes of ASCII that can occur in a BIOS blob by
    // coincidence. Following one would mean walking a table array built from
    // whatever happened to follow it.
    let mut bad = rsdp_bytes(b"FIRECK", 0x000A_00A7);
    bad[8] = bad[8].wrapping_add(1);

    let mut m = FakeMem::default();
    m.put(0x6000, &start_info_bytes(0, 0, 0x7000, 0));
    m.put(0x000E_0000, &bad);
    m.put(0x40E, &[0u8, 0u8]);

    let mut buf = [0u8; 256];
    assert!(describe(&m, 0x6000, &mut buf).unwrap().rsdp.is_none());
}

#[test]
fn a_zero_length_madt_record_terminates_rather_than_spins() {
    // VMM-supplied bytes. A record claiming length 0 would advance the walk by
    // nothing, forever, inside a kernel with no way to report it.
    let mut body = Vec::new();
    body.extend_from_slice(&0xfee0_0000u32.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // type 0, length 0
    let table = table_bytes(b"APIC", &body);

    let mut m = FakeMem::default();
    m.put(0x1000, &table);
    let hdr = acpi::table_at(&m, 0x1000).expect("header");
    let madt = acpi::parse_madt(&m, &hdr).expect("parse terminates");
    assert_eq!(madt.cpus().len(), 0);
}

#[test]
fn not_a_pvh_block_is_an_error_not_a_guess() {
    let mut m = FakeMem::default();
    m.put(0x6000, &[0u8; 64]);
    let mut buf = [0u8; 256];
    assert_eq!(describe(&m, 0x6000, &mut buf).err(), Some(Error::NotPvh));
}

#[test]
fn a_short_read_is_refused_rather_than_partially_filled() {
    // The `PhysMem` contract. A parser that saw half a structure would carry on
    // with the other half stale.
    let mut m = FakeMem::default();
    m.put(0x1000, &[1, 2, 3, 4]);
    let mut buf = [0u8; 8];
    assert!(!m.read(0x1000, &mut buf));
    assert_eq!(buf, [0u8; 8]);
}

#[test]
fn hostile_command_lines_are_refused() {
    for line in [
        "virtio_mmio.device=4K",
        "virtio_mmio.device=4K@0xc0001000",
        "virtio_mmio.device=4K@0x0:5",
        "virtio_mmio.device=0@0xc0001000:5",
        "virtio_mmio.device=4K@0xzzzz:5",
        "virtio_mmio.device=4K@0x:5",
        "virtio_mmio.device=4K@99999999999999999999:5",
        "virtio_mmio.device=",
    ] {
        assert_eq!(cmdline::parse(line).len(), 0, "{line}");
    }
}

#[test]
fn unevenly_spaced_transports_fall_back_to_one_slot() {
    // Drivers address slots as base + i*stride. Believing a stride that does not
    // hold would point slot 1 at nothing.
    let d = cmdline::parse(
        "virtio_mmio.device=4K@0xd0000000:5 virtio_mmio.device=4K@0xd0009000:6",
    );
    assert_eq!(d.len(), 2);
    assert_eq!(d.geometry(), Some((0xd000_0000, 4096, 1)));

    let dense = cmdline::parse(
        "virtio_mmio.device=4K@0xd0000000:5 virtio_mmio.device=4K@0xd0001000:6",
    );
    assert_eq!(dense.geometry(), Some((0xd000_0000, 4096, 2)));
}

#[test]
fn size_suffixes_are_one_field_spelled_three_ways() {
    assert_eq!(cmdline::parse_size("4K"), Some(4096));
    assert_eq!(cmdline::parse_size("4k"), Some(4096));
    assert_eq!(cmdline::parse_size("512"), Some(512));
    assert_eq!(cmdline::parse_size("1M"), Some(1 << 20));
    assert_eq!(cmdline::parse_size("2G"), Some(2 << 30));
    assert_eq!(cmdline::parse_size("0"), None);
}
