//! Reading the machine's own description of itself.
//!
//! The kernel side of `akuma-ryzen-amd64`: a [`PhysMem`] over the physmap, and
//! the boot-time call that turns the PVH handoff into a `MachineDescription`.
//!
//! Everything that used to live in three modules here — `hvm.rs` (the handoff
//! block), `cmdline.rs` (virtio-MMIO discovery) and `acpi.rs` (the RSDP scan and
//! the MADT) — is in that crate now, where it is **host-tested against bytes
//! measured on both machines** rather than only exercised by booting one. What
//! is left here is the part that cannot move: the translation from a physical
//! address to something this kernel can dereference.

use akuma_ryzen_amd64::{MachineDescription, PhysMem};

use crate::phys::{PHYSMAP_LIMIT, phys_ptr};
use crate::serial;

/// Physical memory as the kernel sees it: the physmap, and nothing outside it.
///
/// The bound is not a formality. Every address handed to this comes from the
/// VMM — a handoff pointer, a table pointer inside another table, an EBDA
/// segment — and a read past the physmap is a page fault. Refusing is
/// recoverable and makes the parser return `None`; faulting during early boot
/// is a triple fault and a guest that vanishes with no output.
pub struct Physmap;

impl PhysMem for Physmap {
    fn read(&self, pa: u64, buf: &mut [u8]) -> bool {
        let Some(end) = pa.checked_add(buf.len() as u64) else {
            return false;
        };
        if end > PHYSMAP_LIMIT {
            return false;
        }
        for (i, slot) in buf.iter_mut().enumerate() {
            // SAFETY: the whole range was just proved to be inside the physmap,
            // which `boot.s` maps before `kmain` runs.
            *slot = unsafe { phys_ptr::<u8>(pa + i as u64).read_volatile() };
        }
        true
    }
}

/// Parse the machine description, or halt.
///
/// Halting is the honest response to a failure here: without a memory map there
/// is no heap, no frame allocator and nothing to report with beyond the console
/// that is already printing this line.
pub fn describe(start_info_pa: u64) -> MachineDescription {
    // Scratch for the command-line copy. The parsed devices are kept; the string
    // is not, so this buffer is dead the moment `describe` returns.
    let mut cmdline_buf = [0u8; 512];
    let Ok(d) = akuma_ryzen_amd64::describe(&Physmap, start_info_pa, &mut cmdline_buf) else {
        serial::puts("  [FATAL] no hvm_start_info magic at the PVH handoff address\n");
        crate::halt();
    };
    d
}

/// Print what the machine said. A dump, not a configuration step.
///
/// This is the amd64 equivalent of dumping an FDT, and it is worth printing
/// every boot for the reason `docs/reference/firecracker-amd64/` exists: on this
/// machine **every ACPI table address moves with the vCPU count**, so a log that
/// records where they were is the only way to notice when they move somewhere
/// unexpected.
pub fn report(d: &MachineDescription) {
    let si = &d.start_info;
    serial::puts("  version=");
    serial::put_dec(u64::from(si.version));
    serial::puts(" modules=");
    serial::put_dec(u64::from(si.nr_modules));
    serial::puts(" rsdp=0x");
    serial::put_hex(si.rsdp_paddr);
    serial::puts(" cmdline=0x");
    serial::put_hex(si.cmdline_paddr);
    serial::puts("\n  memmap: ");
    serial::put_dec(u64::from(si.memmap_entries));
    serial::puts(" entries\n");
    for r in d.regions() {
        serial::puts("    0x");
        serial::put_hex(r.addr);
        serial::puts(" + 0x");
        serial::put_hex(r.size);
        serial::puts("  ");
        serial::puts(r.kind_str());
        serial::puts("\n");
    }
    serial::puts("  usable RAM: ");
    serial::put_dec(d.usable_ram() / 1024 / 1024);
    serial::puts(" MiB\n");

    // The command line is re-read for printing rather than kept, because the
    // buffer it was copied into belongs to `describe`'s frame.
    for dev in d.virtio.as_slice() {
        serial::puts("  virtio-mmio: 0x");
        serial::put_hex(dev.base);
        serial::puts(" + 0x");
        serial::put_hex(dev.len);
        serial::puts(" irq ");
        serial::put_dec(u64::from(dev.irq));
        serial::puts("\n");
    }

    let Some(rsdp) = d.rsdp else {
        serial::puts("  acpi: none found (no RSDP in the EBDA or the BIOS window)\n");
        return;
    };
    serial::puts("  acpi: RSDP at 0x");
    serial::put_hex(rsdp.addr);
    serial::puts(" rev=");
    serial::put_dec(u64::from(rsdp.revision));
    serial::puts(" oem=");
    for b in rsdp.oem {
        // The OEM id is six bytes with no NUL and no guarantee of being
        // printable; substitute rather than emitting a control byte into a log
        // that gets grepped.
        serial::putb(if b.is_ascii_graphic() || b == b' ' { b } else { b'?' });
    }
    serial::puts(" xsdt=0x");
    serial::put_hex(rsdp.xsdt);
    serial::puts("\n  acpi: tables:");
    akuma_ryzen_amd64::acpi::for_each_table(&Physmap, &rsdp, |t| {
        serial::puts(" ");
        for b in t.signature {
            serial::putb(if b.is_ascii_graphic() { b } else { b'?' });
        }
        serial::puts("@0x");
        serial::put_hex(t.addr);
        serial::puts("+");
        serial::put_dec(u64::from(t.length));
    });
    serial::puts("\n");

    if let Some(madt) = d.madt.as_ref() {
        serial::puts("  acpi: cpus=");
        serial::put_dec(madt.cpus().len() as u64);
        serial::puts(" lapic=0x");
        serial::put_hex(u64::from(madt.local_apic_addr));
        for io in madt.ioapics() {
            serial::puts(" ioapic=0x");
            serial::put_hex(u64::from(io.addr));
            serial::puts(" gsi_base=");
            serial::put_dec(u64::from(io.gsi_base));
        }
        serial::puts("\n");
    }
}
