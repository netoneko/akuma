# Measured Firecracker aarch64 FDTs

**Stability: A** — measured, not derived. These are the device trees Firecracker
**v1.16.1** actually handed a guest, captured 2026-08-21 on an `m6g.metal`
(Graviton2, Neoverse N1 `MIDR_EL1=0x413fd0c1`) in `ap-northeast-1`, KVM in **VHE**
mode, host kernel `6.17.0-1019-aws`.

Captured by booting Alpine 3.24.1 (`linux-virt` 6.18.44) as a Firecracker microVM
with `init=` pointed at a script that base64s `/sys/firmware/fdt` to the serial
console. Firecracker builds the FDT in-process and has no dump facility, so
booting a guest that exposes it is the only way to read it. Procedure and
tooling: `../../../../docs/archive/AKUMA_FIRECRACKER_TERRAFORM.md`, project
`akuma-terraform`.

Guest config for every dump: 1024 MiB RAM, one virtio-net, one virtio-blk, one
virtio-rng (`"entropy": {}`).

## Files

| File | What |
|---|---|
| `summary.txt` | the addresses per vCPU count, with the values predicted from Firecracker's source printed underneath |
| `fdt-vcpu{1,2,4,8}.dtb` | the blobs, header magic `d00dfeed` verified |
| `fdt-vcpu{1,2,4,8}.dts` | `dtc` renderings |
| `hostinfo.txt` | host CPU, features, `started at EL2` proof, Firecracker version |
| `guestinfo-vcpu1.txt` | the guest's `/proc/interrupts`, `/proc/iomem`, `/proc/cmdline` |

## What they confirm

**The GIC redistributor base moves with the vCPU count**, exactly as
`proposals/FIRECRACKER_PORT.md` §2.1 predicted from
`gic/gicv3/mod.rs`'s `get_redists_addr()`:

| vCPUs | `intc` `reg` (GICD, then GICR) | predicted GICR base |
|---|---|---|
| 1 | `0x3fff0000 0x10000`, `0x3ffd0000 0x20000` | `0x3ffd0000` ✓ |
| 2 | `0x3fff0000 0x10000`, `0x3ffb0000 0x40000` | `0x3ffb0000` ✓ |
| 4 | `0x3fff0000 0x10000`, `0x3ff70000 0x80000` | `0x3ff70000` ✓ |
| 8 | `0x3fff0000 0x10000`, `0x3fef0000 0x100000` | `0x3fef0000` ✓ |

This is the measurement that settles the tactical-vs-structural question in
`FIRECRACKER_PORT.md` §5: a build-time constant cannot express an address that
moves with a runtime `SMP=N`.

**GICD is 64 KiB** (`0x10000`), not the single 4 KiB page Akuma's device VA map
reserved before the `GICD_IROUTER` aliasing fix
(`docs/archive/GICD_IROUTER_ALIASING.md`).

**virtio INTIDs start at SPI 0, i.e. INTID 32** — `virtio_mmio@40003000` has
`interrupts = <0x00 0x00 0x01>` (`GIC_SPI`, 0, edge), `@40004000` → SPI 1,
`@40005000` → SPI 2. QEMU virt starts virtio at INTID 48. Confirms
`VIRTIO_MMIO_SPI_BASE = 32` on this platform.

**virtio-mmio stride is `0x1000`, one device per slot, only for configured
devices** — three nodes for three devices, at `0x40003000`, `0x40004000`,
`0x40005000`.

QEMU virt is the opposite on both counts: its device tree advertises **32**
`0x200`-spaced slots from `0xa000000`, almost all empty, of which
`crates/akuma-virtio/src/probe.rs` walks the low eight. So neither the stride, the
count, nor the "slots are always present" assumption carries between the two
machines. Both trees are parsed by the same code in `crates/akuma-firecracker`,
whose fixtures include the QEMU dumps for exactly this comparison.

**The serial at `0x40002000` (SPI 3 → INTID 35) advertises
`compatible = "ns16550a"` — a 16550, not a PL011.** The memory map calls it a
PL011 because that is what Akuma drives it as, and TX works: a PL011's `DR` and a
16550's `THR` are both at offset `0x00`, so byte writes transmit either way. Reads
do **not** line up — PL011 `FR` is at `0x18`, 16550 `LSR` at `0x05` — so anything
depending on a status flag (RX, or a TX-full check) is reading an unrelated
register. Worth resolving before trusting console input on this platform.

Related trap in the same tree: `rtc@40001000` is
`compatible = "arm,pl031\0arm,primecell"`, and it is listed *before* the UART. A
device search that falls back to `arm,primecell` therefore finds the **clock** and
calls it the console. `akuma-firecracker` matches `ns16550a` and `arm,pl011`
explicitly for that reason.

**The `memory` node starts at `0x80200000`, not `0x80000000`.** Worth stating
because the memory *map* documents `DRAM_MEM_START = 0x8000_0000`: Firecracker
reserves the first 2 MiB (`SYSTEM_MEM_SIZE`) and the FDT `memory` node describes
only what follows. With 1024 MiB configured the node reads
`<0x0 0x80200000 0x0 0x3fe00000>` — 1022 MiB. `detect_memory()` reading base and
size from this node is what produces Akuma's
`[Memory] Detected from DTB: base=0x80200000`.

**PSCI v1.3 is advertised** (`psci: PSCIv1.3 detected in firmware`), and the tree
describes **every** configured vCPU — `cpu@0..n`, 1/2/4/8 nodes across the sweep.
Being *described* is not being *running*: secondaries are powered off awaiting a
PSCI wakeup, as `FIRECRACKER_PORT.md` §3 Q5 said. (An earlier revision of this file
claimed only `cpu@0` appeared; that was read off the 1-vCPU dump and is wrong.)

A trap that follows from it: `cpu_count * 0x2_0000` happens to equal the
redistributor span, so code could derive the GIC address from the CPU list and
pass every test here. That is the same class of mistake as the compile-time
literal — an address inferred from an unrelated property. `akuma-firecracker`
reads the span from `intc`'s second `reg` entry and merely *asserts* the two
agree.

## Nodes Akuma does not get

Nothing in the tree corresponds to `fw_cfg` (QEMU-only, hence `ramfb.rs` being
gated off) or to PCI. Present but unused by Akuma: `rtc@40001000` (PL031),
`vmgenid`, `ptp@2149572608` (`0x80200000`), `intc/msic`, `apb-pclk`.
