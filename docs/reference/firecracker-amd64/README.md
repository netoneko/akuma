# Firecracker on x86_64: what the machine tells the guest

**Grade: A** (stable, trust it) for the captures; they are measured output, not
prose. Captured 2026-09-04 on an **AMD Ryzen 7 8845HS** (Pop!_OS 22.04,
Firecracker v1.16.1, native KVM) by `amd64/dump-machine.sh`.

The x86_64 counterpart of [`../firecracker/fdt/`](../firecracker/fdt/), and the
first thing to know is that **there is no device tree here.** On aarch64
Firecracker hands the guest an FDT and `akuma-fdt` reads it. On x86_64 it hands
over three unrelated things:

| | where | what it carries |
|---|---|---|
| PVH `hvm_start_info` | address in `%ebx` at entry | E820 memory map, command-line pointer |
| the kernel command line | a string that block points at | every virtio-MMIO transport: base, size, IRQ |
| ACPI tables | found by **scanning** the BIOS window | interrupt controllers, via the MADT |

`crates/akuma-ryzen-amd64` parses all three, and its host tests are built from
the numbers in these captures.

## How these were captured

`amd64/dump-machine.sh` boots **Linux** under Firecracker and reads its boot
log — Linux prints every ACPI table it finds, with address, length and OEM id,
long before it needs a root filesystem. So no rootfs is involved: the guest
panics on "no working init" a moment after the dump is already on the serial
line. The kernel is Firecracker's own published CI `vmlinux` (an uncompressed
ELF; a distro `vmlinuz` is a compressed bzImage and Firecracker cannot load one).

```bash
FC_HOST=user@host amd64/dump-machine.sh                 # 1, 2, 4 and 8 vCPUs
FC_HOST=user@host VCPU_LIST="1 16" amd64/dump-machine.sh
```

The same three facts can be read from Akuma's own boot log — `amd64/src/machine.rs`
prints them every boot — which is the cross-check. The Linux capture is the
independent second opinion.

## The finding: every table address moves with the vCPU count

This is why these captures exist, and it is the amd64 twin of the aarch64 GIC
redistributor bug ([`../../archive/GICD_IROUTER_ALIASING.md`](../../archive/GICD_IROUTER_ALIASING.md),
where a base pinned to one vCPU made the boot core drive another core's frames).

```text
  vCPUs      1          2          4          8
  RSDP     0xE0000    0xE0000    0xE0000    0xE0000     <- the only fixed one
  XSDT     0xA00A7    0xA00C3    0xA00FB    0xA016B
  FACP     0x9FF17    0x9FF2B    0x9FF53    0x9FFA3
  DSDT     0x9FD30    0x9FD44    0x9FD6C    0x9FDBC
  APIC     0xA002B    0xA003F    0xA0067    0xA00B7
  MCFG     0xA006B    0xA0087    0xA00BF    0xA012F
  MADT len   0x40       0x48       0x58       0x78
```

The MADT grows by one 8-byte Local APIC entry per vCPU, and everything packed
around it slides. A kernel that pinned any of these to a literal would read the
right table at one vCPU count and a neighbouring table's bytes at another — with
**no error**, because the signature check would be the only thing standing
between it and garbage.

Only the RSDP is fixed, at `0x000E_0000` — the first address of the BIOS search
window. Which is fortunate, because it has to be found by scanning:
`hvm_start_info.rsdp_paddr` is **0** on Firecracker *and* on QEMU `microvm`
(measured both). The field exists in the PVH ABI and neither VMM fills it in.

## What does not move

| | value | note |
|---|---|---|
| RSDP | `0x000E_0000` | OEM id `FIRECK` |
| IOAPIC | `0xfec00000`, GSI 0-23 | one, at every vCPU count |
| Local APIC | `0xfee00000` | as the MADT reports it |
| virtio-MMIO | `0xc0001000`, 4 KiB stride | one token per attached device |
| MMIO hole | `0xeec00000-0xfebfffff` | reserved in the E820 map |

The IOAPIC address is the conventional x86 one, and Linux reports it — but the
kernel *reads* it from the MADT rather than assuming it, for the same reason
everything else here is read.

## Comparison: QEMU `microvm`

`amd64/run.sh` is the local stand-in, and it is a different machine that happens
to speak the same protocols. Both are parsed by the same code, which is the
point — a parser validated against one machine is a description of that machine.

| | Firecracker | QEMU `microvm` |
|---|---|---|
| RSDP | `0x000E_0000`, OEM `FIRECK` | `0x000F_5590`, OEM `BOCHS` |
| tables | FACP DSDT APIC MCFG | FACP APIC |
| IOAPICs | 1, at `0xfec00000` | **2**, at `0xfec00000` (GSI 0) and `0xfec10000` (GSI 24) |
| virtio-MMIO | `0xc0001000`, 4 KiB stride | `0xfeb00000`, 0x200 stride |
| device announcement | Firecracker writes the token | `run.sh` writes it (`-append`) |
| transport assignment | dense from its own base | **top-down**; devices must be pinned with `bus=virtio-mmio-bus.N` |

The two-IOAPIC difference is the kind of thing that makes a second machine worth
having: code written against Firecracker alone would reasonably assume one.

## Files

| | |
|---|---|
| `linux-boot-{1,2,4,8}-vcpu.log` | Linux 6.1.128 boot log under Firecracker at that vCPU count |

**Background:** [`../../archive/AKUMA_FIRECRACKER_AMD64.md`](../../archive/AKUMA_FIRECRACKER_AMD64.md)
is the port, one section per stage; `crates/akuma-ryzen-amd64` is the parser and
carries the reasoning in its module headers.
