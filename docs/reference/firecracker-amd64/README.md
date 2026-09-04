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

## What this VMM can be asked for

Measured by `amd64/probe-hardware.sh`, which boots Linux under a matrix of
configurations and reads the guest's own inventory. A config the VMM rejects is
as informative as one it accepts, so failures are captured rather than aborting.

| case | result | what the guest sees |
|---|---|---|
| `baseline` | boots | no virtio at all; the command line has no device token |
| `block` | boots | `virtio_mmio.device=4K@0xc0001000:5` -> `vda` |
| `two-block` | boots | **two** tokens: `4K@0xc0001000:5` and `4K@0xc0002000:6` |
| `rng` / `balloon` / `pmem` / `hotplug` | boot | one token each, at `0xc0001000:5` |
| `pci`, `block-pci`, `rng-pci` | boot | a PCIe segment; see below |
| `vsock` | boots | (first run failed on a stale socket file, not a limit) |
| `net` | boots | one token, exactly like a disk |
| `net-dhcp` | boots | **gets a DHCP lease**: `10.0.2.15`, gateway `10.0.2.2` |

## Networking, and the host side of it

`amd64/net-setup.sh` gives the host a tap, DHCP and NAT **through Docker, with no
sudo** — `--network host` puts the container in the host's network namespace, so
an interface it creates belongs to the host and outlives the container, and
`--cap-add=NET_ADMIN` grants exactly what `ip tuntap` and `iptables` need.

Two things that cost a run each and are easy to miss:

* **`--device /dev/net/tun` is required.** `--network host` shares the network
  namespace, not `/dev`. Without it `ip tuntap add` fails with "open: No such
  file or directory", which reads like the tap is missing and is actually the
  control device being absent inside the container.
* **`ip tuntap ... user <uid>`, numerically.** The container has no account for
  the host's user, so a name fails with `invalid user "..."`.

The addresses are deliberately identical to
`overlays/devbox-firecracker/guest-setup.sh` — gateway `10.0.2.2`, guest
`10.0.2.15` — which are QEMU's SLIRP addresses, so a guest cannot tell the three
hosts apart.

Proved with a Linux guest and in-kernel DHCP (`ip=dhcp`), so no root filesystem
is involved:

```text
Sending DHCP requests ., OK
IP-Config: Got DHCP answer from 10.0.2.2, my address is 10.0.2.15
     device=eth0, hwaddr=02:fc:00:00:00:01, ipaddr=10.0.2.15, gw=10.0.2.2
```

**The NIC arrives the same way a disk does** — one `virtio_mmio.device=` token,
dense in the same slot array, parsed by the same code. Nothing about device
discovery has to change for networking; what is missing is kernel-side wiring of
`akuma-net`, which already builds for `x86_64-unknown-none` (as do `akuma-net-nic`,
`akuma-net-yarn` and `akuma-net-unix`).

**Devices are dense and ordered.** Every virtio device gets a token, allocated
from `0xc0001000` with a `0x1000` stride and an IRQ from 5 upward, in attachment
order. That is what `MmioDevices::geometry()` in `crates/akuma-ryzen-amd64`
computes, and `two-block` is the case that proves the stride rather than assuming
it from a single device.

## Firecracker **does** have PCI

Worth stating plainly because the opposite was written down in this tree and
propagated as a premise. `firecracker --enable-pci` ("Enables PCIe support")
exists in v1.16.1 and builds a real segment:

```text
pci: adding PCI segment: id=0x0,
     PCI MMIO config address: 0xeec00000,          <- ECAM
     mem32 area: [0xc0001000-0xeebfffff],
     mem64 area: [0x4000000000-0x7fffffffff],
     IO area:    [0xcf8-0xcff]
```

Note `0xeec00000`: the same span the E820 map reports as *reserved* in every
capture. The MCFG table is published whether or not PCI is enabled.

With `--enable-pci` the command line loses **both** `pci=off` and every
`virtio_mmio.device=` token — the devices move to PCI and are found by
enumeration instead of announcement.

**Why the confusion is easy.** Firecracker's own published CI `vmlinux` is built
**without `CONFIG_PCI`**: in the `block-pci` capture the guest prints zero PCI
enumeration lines and never finds the disk. The VMM offered the bus; that guest
could not see it. Anyone testing with the standard artifacts would conclude PCI
does not work.

Akuma drives MMIO **by choice**: the driver already exists and works on both
architectures, and PCI would mean config-space enumeration and BAR programming
for no capability this target needs yet. If that changes, QEMU's ordinary
`pc`/`q35` machine becomes the right local stand-in and `-M microvm` can go.

## Files

| | |
|---|---|
| `probe-*.log` | one Linux boot per configuration in the matrix above |

| | |
|---|---|
| `linux-boot-{1,2,4,8}-vcpu.log` | Linux 6.1.128 boot log under Firecracker at that vCPU count |
| `README.md` | this file |

**Background:** [`../../archive/AKUMA_FIRECRACKER_AMD64.md`](../../archive/AKUMA_FIRECRACKER_AMD64.md)
is the port, one section per stage; `crates/akuma-ryzen-amd64` is the parser and
carries the reasoning in its module headers.
