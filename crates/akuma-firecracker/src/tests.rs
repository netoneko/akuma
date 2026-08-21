//! Tests against the device trees Firecracker actually emitted.
//!
//! The fixtures are the artifacts in `docs/reference/firecracker/fdt/`, included
//! directly rather than copied — so the blobs the reference doc cites and the
//! blobs the code is tested against cannot drift apart. They were captured on an
//! `m6g.metal` host from Firecracker v1.16.1, one per vCPU count, by booting
//! Alpine with `init=` pointed at a script that base64s `/sys/firmware/fdt` to
//! the serial console (`docs/runbooks/dump-firecracker-fdt.md`).
//!
//! This is the payoff of `Fdt::new(&[u8])`: the whole device-map derivation is
//! testable on the host in milliseconds, against real machine descriptions,
//! with no devbox boot in the loop.

use super::*;

const VCPU1: &[u8] = include_bytes!("../../../docs/reference/firecracker/fdt/fdt-vcpu1.dtb");
const VCPU2: &[u8] = include_bytes!("../../../docs/reference/firecracker/fdt/fdt-vcpu2.dtb");
const VCPU4: &[u8] = include_bytes!("../../../docs/reference/firecracker/fdt/fdt-vcpu4.dtb");
const VCPU8: &[u8] = include_bytes!("../../../docs/reference/firecracker/fdt/fdt-vcpu8.dtb");

/// Every fixture, with the vCPU count it was captured at.
const SWEEP: [(usize, &[u8]); 4] = [(1, VCPU1), (2, VCPU2), (4, VCPU4), (8, VCPU8)];

fn describe_ok(blob: &[u8]) -> MachineDescription {
    describe(blob).expect("fixture should parse")
}

// ---------------------------------------------------------------------------
// The reason this crate exists
// ---------------------------------------------------------------------------

/// `GICR base = GICD base - n * 0x2_0000`, and the span grows to match.
///
/// This is the whole argument for a runtime device map: the address depends on a
/// value chosen at boot. Four fixtures, four different answers, one formula —
/// and the formula is asserted here only to prove the fixtures really do disagree.
/// The crate itself reads the address rather than computing it.
#[test]
fn redistributor_base_moves_with_the_vcpu_count() {
    const REDIST_PER_CPU: u64 = 0x2_0000;

    for (vcpus, blob) in SWEEP {
        let m = describe_ok(blob);
        let gic = m.gic;
        assert_eq!(gic.version, GicVersion::V3, "vcpus={vcpus}");

        let redist = gic
            .redistributors
            .unwrap_or_else(|| panic!("vcpus={vcpus}: GICv3 must report redistributors"));

        assert_eq!(
            redist.base,
            gic.distributor.base - (vcpus as u64) * REDIST_PER_CPU,
            "vcpus={vcpus}: redistributor base"
        );
        assert_eq!(
            redist.size,
            (vcpus as u64) * REDIST_PER_CPU,
            "vcpus={vcpus}: redistributor span"
        );
        // Below the distributor, never overlapping it.
        assert!(redist.end() <= gic.distributor.base, "vcpus={vcpus}");
    }
}

/// The measured table, spelled out, so a Firecracker upgrade that moves the map
/// fails loudly here instead of silently at boot.
#[test]
fn measured_gic_addresses() {
    let expected = [
        (1usize, 0x3ffd_0000u64, 0x2_0000u64),
        (2, 0x3ffb_0000, 0x4_0000),
        (4, 0x3ff7_0000, 0x8_0000),
        (8, 0x3fef_0000, 0x10_0000),
    ];

    for ((vcpus, base, size), (sweep_vcpus, blob)) in expected.iter().zip(SWEEP) {
        assert_eq!(*vcpus, sweep_vcpus);
        let gic = describe_ok(blob).gic;

        assert_eq!(gic.distributor.base, 0x3fff_0000, "vcpus={vcpus}");
        // 64 KiB, not one 4 KiB page: the assumption behind the GICD_IROUTER
        // aliasing bug (docs/archive/GICD_IROUTER_ALIASING.md).
        assert_eq!(gic.distributor.size, 0x1_0000, "vcpus={vcpus}");

        let redist = gic.redistributors.unwrap();
        assert_eq!(redist.base, *base, "vcpus={vcpus}");
        assert_eq!(redist.size, *size, "vcpus={vcpus}");
    }
}

// ---------------------------------------------------------------------------
// The rest of the machine
// ---------------------------------------------------------------------------

/// The `memory` node starts at `0x8020_0000`, not `0x8000_0000` — Firecracker
/// reserves the first 2 MiB of DRAM and describes only what follows. Asserted
/// because reading `DRAM_MEM_START` from the memory map and expecting the node
/// to agree is the obvious mistake.
#[test]
fn memory_node_excludes_firecrackers_reserved_2mib() {
    for (vcpus, blob) in SWEEP {
        let ram = describe_ok(blob).ram;
        assert_eq!(ram.base, 0x8020_0000, "vcpus={vcpus}");
        // 1024 MiB configured, 2 MiB reserved -> 1022 MiB described.
        assert_eq!(ram.size, 0x3fe0_0000, "vcpus={vcpus}");
        assert_eq!(ram.size, 1022 * 1024 * 1024, "vcpus={vcpus}");
        assert_eq!(ram.end(), 0xc000_0000, "vcpus={vcpus}");
    }
}

/// Firecracker's console is at 0x4000_2000 — and advertises `ns16550a`, not
/// `arm,pl011`. Akuma drives it with its PL011 driver and transmit works, because
/// a PL011's `DR` and a 16550's `THR` are both at offset 0x00; the status
/// registers are not (`FR` 0x18 vs `LSR` 0x05).
#[test]
fn firecracker_console_is_at_0x40002000() {
    for (vcpus, blob) in SWEEP {
        let uart = describe_ok(blob).uart.expect("a console exists");
        assert_eq!(uart.region.base, 0x4000_2000, "vcpus={vcpus}");
        assert_eq!(uart.region.size, 0x1000, "vcpus={vcpus}");
        // SPI 3 -> INTID 35.
        assert_eq!(uart.intid, 35, "vcpus={vcpus}");
    }
}

/// Three configured devices (net, block, rng), `0x1000` apart, INTIDs from 32.
///
/// Both halves matter. QEMU virt advertises eight slots `0x200` apart starting
/// at INTID 48; Firecracker instantiates only what was configured, `0x1000`
/// apart, from INTID 32. A slot table cannot be a shared constant.
#[test]
fn virtio_slots_are_dense_from_0x40003000_and_intid_32() {
    for (vcpus, blob) in SWEEP {
        let m = describe_ok(blob);
        let slots = m.virtio_slots();
        assert_eq!(slots.len(), 3, "vcpus={vcpus}: net + block + rng");

        for (k, slot) in slots.iter().enumerate() {
            assert_eq!(
                slot.region.base,
                0x4000_3000 + (k as u64) * 0x1000,
                "vcpus={vcpus} slot={k}"
            );
            assert_eq!(slot.region.size, 0x1000, "vcpus={vcpus} slot={k}");
            assert_eq!(slot.intid, 32 + k as u32, "vcpus={vcpus} slot={k}");
        }
    }
}

/// Slots come back in address order whatever order the FDT lists them in.
///
/// Load-bearing: the block driver takes the first virtio-blk it probes and
/// `akuma-rump` binds the second virtio-net by index, so "slot k" has to mean
/// the same thing every boot.
#[test]
fn virtio_slots_are_sorted_by_address() {
    for (vcpus, blob) in SWEEP {
        let m = describe_ok(blob);
        let slots = m.virtio_slots();
        for pair in slots.windows(2) {
            assert!(
                pair[0].region.base < pair[1].region.base,
                "vcpus={vcpus}: slots must ascend"
            );
        }
    }
}

/// The FDT describes every configured vCPU — `cpu@0..n` — so the node count does
/// track `vcpu_count` here.
///
/// Kept as a test anyway, because the tempting shortcut it guards against is
/// real: `cpu_count * 0x2_0000` happens to equal the redistributor span on this
/// machine, so code could derive the span from the CPU list and pass. It would be
/// deriving a GIC address from an unrelated property, which is the same class of
/// mistake as the compile-time literal. [`Gic::redistributors`] is read from the
/// `intc` node's second `reg` entry, and this asserts the two agree rather than
/// substituting one for the other.
#[test]
fn cpu_nodes_track_the_vcpu_count() {
    for (vcpus, blob) in SWEEP {
        let m = describe_ok(blob);
        assert_eq!(m.cpu_count, vcpus, "vcpus={vcpus}");

        let redist = m.gic.redistributors.unwrap();
        assert_eq!(
            redist.size,
            (m.cpu_count as u64) * 0x2_0000,
            "vcpus={vcpus}: span and CPU count agree -- but the span is READ, not derived"
        );
    }
}

#[test]
fn fdt_size_matches_the_blob() {
    for (vcpus, blob) in SWEEP {
        let m = describe_ok(blob);
        assert_eq!(m.fdt_size, blob.len(), "vcpus={vcpus}");
        // Comfortably inside Firecracker's 2 MiB FDT_MAX_SIZE.
        assert!(m.fdt_size < 0x20_0000, "vcpus={vcpus}");
    }
}

// ---------------------------------------------------------------------------
// INTID resolution
// ---------------------------------------------------------------------------

/// The one piece of arithmetic here, and the one that makes
/// `VIRTIO_MMIO_SPI_BASE` stop being a platform constant.
#[test]
fn interrupt_cells_resolve_to_intids() {
    fn cells(kind: u32, number: u32, flags: u32) -> [u8; 12] {
        let mut out = [0u8; 12];
        out[0..4].copy_from_slice(&kind.to_be_bytes());
        out[4..8].copy_from_slice(&number.to_be_bytes());
        out[8..12].copy_from_slice(&flags.to_be_bytes());
        out
    }

    // Firecracker's first virtio device: SPI 0.
    assert_eq!(Interrupt::first(&cells(0, 0, 1)).unwrap().intid, 32);
    // QEMU virt's first virtio device: SPI 16. Same binding, different base --
    // which is exactly why 48 vs 32 was never a kernel constant.
    assert_eq!(Interrupt::first(&cells(0, 16, 1)).unwrap().intid, 48);
    // The virtual timer: PPI 11 -> INTID 27, the number Akuma enables at boot.
    assert_eq!(Interrupt::first(&cells(1, 11, 4)).unwrap().intid, 27);
    // PL011 on Firecracker.
    assert_eq!(Interrupt::first(&cells(0, 3, 1)).unwrap().intid, 35);
}

#[test]
fn malformed_interrupts_are_rejected_not_guessed() {
    // Too short to be three cells.
    assert!(Interrupt::first(&[0u8; 11]).is_none());
    assert!(Interrupt::first(&[]).is_none());
    // Unknown kind: skip the device rather than wire it to INTID 32 and have it
    // silently never fire.
    let mut unknown = [0u8; 12];
    unknown[3] = 7;
    assert!(Interrupt::first(&unknown).is_none());
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

#[test]
fn junk_is_not_a_device_tree() {
    assert_eq!(describe(&[]).unwrap_err(), Error::BadFdt);
    assert_eq!(describe(&[0u8; 64]).unwrap_err(), Error::BadFdt);
    assert_eq!(
        describe(b"not a device tree at all").unwrap_err(),
        Error::BadFdt
    );
}

#[test]
fn a_truncated_blob_does_not_panic() {
    // Header intact, body cut off. The parser must fail, not index out of bounds.
    for cut in [8usize, 40, 64, 128, 512] {
        if cut < VCPU1.len() {
            let _ = describe(&VCPU1[..cut]);
        }
    }
}

#[test]
fn regions_report_their_bounds() {
    let r = Region { base: 0x4000_3000, size: 0x1000 };
    assert_eq!(r.end(), 0x4000_4000);
    assert!(r.contains(0x4000_3000));
    assert!(r.contains(0x4000_3fff));
    assert!(!r.contains(0x4000_4000));
    assert!(!r.contains(0x4000_2fff));
}

// ---------------------------------------------------------------------------
// QEMU virt: the same code, a different machine
// ---------------------------------------------------------------------------
//
// The crate claims to be platform-neutral. Firecracker fixtures alone cannot
// show that — code with a Firecracker address baked in would pass every test
// above. These fixtures are QEMU virt's own device tree, dumped with
// `qemu-system-aarch64 -machine virt,gic-version=3,dumpdtb=...` at the same
// `-m 256` the runner uses, then compacted with `dtc -I dtb -O dtb` (QEMU's
// dumpdtb pads the blob to 1 MiB and sets `totalsize` to the padded length).
//
// They live in the crate rather than beside the Firecracker blobs in
// docs/reference/firecracker/fdt/ for two reasons: that path is
// Firecracker-specific, and these are regression fixtures rather than evidence
// cited by a document.

const QEMU_SMP1: &[u8] = include_bytes!("../fixtures/qemu-virt-smp1.dtb");
const QEMU_SMP2: &[u8] = include_bytes!("../fixtures/qemu-virt-smp2.dtb");
const QEMU_SMP4: &[u8] = include_bytes!("../fixtures/qemu-virt-smp4.dtb");

const QEMU_SWEEP: [(usize, &[u8]); 3] = [(1, QEMU_SMP1), (2, QEMU_SMP2), (4, QEMU_SMP4)];

#[test]
fn qemu_virt_machine_is_read_correctly() {
    for (cpus, blob) in QEMU_SWEEP {
        let m = describe_ok(blob);

        // -m 256, and QEMU puts DRAM at 1 GiB with no reserved prefix -- so
        // unlike Firecracker the memory node starts exactly at the DRAM base.
        assert_eq!(m.ram.base, 0x4000_0000, "smp={cpus}");
        assert_eq!(m.ram.size, 0x1000_0000, "smp={cpus}");

        assert_eq!(m.gic.version, GicVersion::V3, "smp={cpus}");
        assert_eq!(m.gic.distributor.base, 0x0800_0000, "smp={cpus}");
        // 64 KiB here too: the distributor span is architectural, not per-machine.
        assert_eq!(m.gic.distributor.size, 0x1_0000, "smp={cpus}");

        let uart = m.uart.expect("pl011");
        assert_eq!(uart.region.base, 0x0900_0000, "smp={cpus}");
        // SPI 1 -> INTID 33.
        assert_eq!(uart.intid, 33, "smp={cpus}");

        assert_eq!(m.cpu_count, cpus, "smp={cpus}");
    }
}

/// **QEMU virt's redistributor base does not move with the CPU count.**
///
/// This is the counterpart to `redistributor_base_moves_with_the_vcpu_count`,
/// and together they explain the whole bug. QEMU pre-allocates one large
/// redistributor window — `0x80a0000`, span `0xf60000`, identical at 1, 2 and 4
/// CPUs — so a hardcoded literal is *correct* on QEMU virt at any `SMP=N`. That
/// is exactly why the literals survived so long, and why moving to Firecracker,
/// where the base is `GICD - n * 0x2_0000`, broke `SMP>1` silently rather than
/// loudly.
#[test]
fn qemu_redistributor_base_is_fixed_unlike_firecrackers() {
    let mut seen = None;
    for (cpus, blob) in QEMU_SWEEP {
        let redist = describe_ok(blob)
            .gic
            .redistributors
            .unwrap_or_else(|| panic!("smp={cpus}: GICv3 redistributors"));

        assert_eq!(redist.base, 0x080a_0000, "smp={cpus}");
        match seen {
            None => seen = Some(redist),
            Some(prev) => assert_eq!(prev, redist, "smp={cpus}: must not vary with CPU count"),
        }
    }

    // And the contrast, in one assertion: Firecracker's does vary.
    let fc1 = describe_ok(VCPU1).gic.redistributors.unwrap();
    let fc4 = describe_ok(VCPU4).gic.redistributors.unwrap();
    assert_ne!(fc1.base, fc4.base, "Firecracker's redistributor base moves");
}

/// virtio INTIDs are 48+ on QEMU virt and 32+ on Firecracker — read, not assumed.
///
/// `VIRTIO_MMIO_SPI_BASE` in `src/main.rs` is a single constant that has to be
/// 48 for one machine and 32 for the other. Both numbers come out of the same
/// code path here, from the machine's own `interrupts` cells.
#[test]
fn qemu_virtio_intids_start_at_48_firecrackers_at_32() {
    let qemu = describe_ok(QEMU_SMP1);
    let slots = qemu.virtio_slots();
    for (k, slot) in slots.iter().enumerate() {
        assert_eq!(slot.region.base, 0x0a00_0000 + (k as u64) * 0x200, "slot={k}");
        // QEMU virt packs eight slots into one 4 KiB page at stride 0x200.
        assert_eq!(slot.region.size, 0x200, "slot={k}");
        assert_eq!(slot.intid, 48 + k as u32, "slot={k}");
    }

    let fc = describe_ok(VCPU1);
    assert_eq!(fc.virtio_slots()[0].intid, 32);
    assert_eq!(qemu.virtio_slots()[0].intid, 48);
}

/// QEMU virt describes 32 virtio slots; the cap keeps the low 8, visibly.
#[test]
fn qemu_virtio_truncation_is_deterministic_and_reported() {
    let m = describe_ok(QEMU_SMP1);
    assert_eq!(m.virtio_seen(), 32, "QEMU virt advertises 32 slots");
    assert_eq!(m.virtio_slots().len(), MAX_VIRTIO_SLOTS);
    // The LOW eight, not the first eight encountered.
    assert_eq!(m.virtio_slots()[0].region.base, 0x0a00_0000);
    assert_eq!(m.virtio_slots()[MAX_VIRTIO_SLOTS - 1].region.base, 0x0a00_0e00);

    // Firecracker's three fit, so nothing is dropped and seen == kept.
    let fc = describe_ok(VCPU1);
    assert_eq!(fc.virtio_seen(), 3);
    assert_eq!(fc.virtio_slots().len(), 3);
}

/// Nothing in this crate is machine-specific: the two trees disagree on every
/// address, and one code path reads both.
#[test]
fn no_address_is_hardcoded() {
    let fc = describe_ok(VCPU1);
    let qemu = describe_ok(QEMU_SMP1);

    assert_ne!(fc.ram.base, qemu.ram.base);
    assert_ne!(fc.gic.distributor.base, qemu.gic.distributor.base);
    assert_ne!(
        fc.gic.redistributors.unwrap().base,
        qemu.gic.redistributors.unwrap().base
    );
    assert_ne!(fc.uart.unwrap().region.base, qemu.uart.unwrap().region.base);
    assert_ne!(fc.uart.unwrap().intid, qemu.uart.unwrap().intid);
    assert_ne!(
        fc.virtio_slots()[0].region.base,
        qemu.virtio_slots()[0].region.base
    );
    assert_ne!(fc.virtio_slots()[0].intid, qemu.virtio_slots()[0].intid);
    // Even the virtio stride differs: 0x1000 vs 0x200.
    assert_ne!(fc.virtio_slots()[0].region.size, qemu.virtio_slots()[0].region.size);
}

/// The RTC must never be mistaken for the console.
///
/// Firecracker's `rtc@40001000` is `arm,pl031\0arm,primecell` and is listed
/// *before* the UART, so a device search that accepts `arm,primecell` finds the
/// clock. QEMU virt's real UART, meanwhile, legitimately carries
/// `arm,pl011\0arm,primecell` -- so the fix cannot be "reject primecell", it has
/// to be "ask for the UART compatibles explicitly".
#[test]
fn the_rtc_is_not_the_console() {
    // Firecracker: ns16550a at 0x40002000, NOT the pl031 at 0x40001000.
    let fc = describe_ok(VCPU1).uart.unwrap();
    assert_eq!(fc.region.base, 0x4000_2000);
    assert_ne!(fc.region.base, 0x4000_1000, "that is the RTC");

    // QEMU virt: pl011 at 0x9000000, which also claims primecell.
    let qemu = describe_ok(QEMU_SMP1).uart.unwrap();
    assert_eq!(qemu.region.base, 0x0900_0000);
}
