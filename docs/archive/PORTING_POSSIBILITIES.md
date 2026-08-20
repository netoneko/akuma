# Porting possibilities: Firecracker, and what it costs to get there

**Date:** 2026-08-18
**Scope:** hosting options for running Akuma as a Firecracker microVM guest, and
the guest-side delta between Akuma's current QEMU `virt` target and Firecracker's
aarch64 platform.
**Question:** what is the cheapest way to get an aarch64 host with `/dev/kvm`,
and how much of the port actually needs one.

Nothing in this document has been applied. It is an options survey plus a work list.

---

## 1. The hard constraint

Akuma is AArch64-only. Firecracker requires read/write access to `/dev/kvm`.
Firecracker on x86_64 cannot boot an aarch64 guest. So the host must be an
**aarch64 machine with KVM** — there is no way around that with a cheaper x86 box.

### 1.1 AWS shipped nested virtualization in 2026 — Intel only

AWS enabled nested virtualization on non-metal EC2 in 2026, but the rollout is
restricted to **8th-generation Intel** families: `c8i`, `m8i`, `r8i`. KVM and
Hyper-V are the only supported L1 hypervisors. **Graviton is explicitly not
supported**, and there is no announced roadmap for it.

The architectural background: Graviton2 is ARMv8.2, which has no nested-virt
extension at all (NV arrived in ARMv8.3). Graviton3 is ARMv8.4 and does have it,
but AWS only exposes it on the bare-metal SKUs.

**Consequence:** "chain Firecracker inside a smaller ARM instance" is not
available on AWS. For an aarch64 guest, ARM + KVM means `.metal`, full stop.
The Intel nested-virt path is useless here because it cannot boot an aarch64
kernel.

### 1.2 There is no small ARM metal

The smallest Graviton bare-metal SKU is a whole 64-core socket. Instance size is
therefore not a cost lever on AWS; the only levers are **which metal generation**
and **spot vs on-demand**.

---

## 2. AWS cost table

On-demand, `us-east-1`, as of 2026-08-18.

| Instance | vCPU / RAM | On-demand | Notes |
|---|---|---|---|
| **c6g.metal** | 64 / 128 GiB | **$2.176/hr** ($1,588.48/mo) | cheapest ARM metal; Graviton2 |
| c7g.metal | 64 / 128 GiB | $2.32/hr | Graviton3 |
| m6g.metal | 64 / 256 GiB | $2.464/hr | **spot $0.679/hr** (~72% off) |
| c8g.metal-24xl | 96 / 192 GiB | $3.8285/hr | Graviton4 |
| m8g.metal-24xl | 96 / 384 GiB | $4.30848/hr | Graviton4 |

Reserved-instance pricing for `c6g.metal` runs $1.5859/hr (1-year flexible) down
to $0.94/hr (3-year locked) — 27–57% off. **Not worth it for a porting project**;
the commitment outlives the work.

### 2.1 Spot is the real lever

`m6g.metal` spot is $0.679/hr against $2.464 on-demand. `c6g.metal` spot lands in
the same ballpark. For a kernel port, spot interruption is a non-event — the
workload is "boot a microVM, read the console, kill it", so a reclaimed instance
costs a relaunch, not data.

Rough monthly cost by usage pattern, on spot at ~$0.65/hr:

| Pattern | Cost |
|---|---|
| ~20 hrs/week (normal dev cadence) | **~$56/mo** |
| 8 hrs/day, weekdays | ~$114/mo |
| 24/7 spot | ~$470/mo |
| 24/7 on-demand `c6g.metal` | $1,588/mo |

The difference between the first row and the last is entirely about whether the
instance is treated as a build/validation target or as a standing dev box. It
should be the former — see §4.

### 2.2 Graviton2 is fine as the host

`c6g.metal` being ARMv8.2 (no nested-virt extension) does **not** matter here.
Akuma-under-Firecracker is a single level of virtualization: the metal instance
is L0 and runs KVM directly. ARMv8.3 NV would only matter if something needed to
run a hypervisor *inside* the guest. So the cheapest metal SKU is also a correct
one.

---

## 3. Non-AWS options

Listed for completeness; AWS is assumed to be the realistic production target.

### 3.1 Local M-series Mac — likely the best dev loop, and free

macOS 15.0+ exposes **nested virtualization through Hypervisor.framework on M3
and later**. A Linux aarch64 VM running under the Apple Virtualization Framework
backend (UTM's AVF backend, *not* its QEMU backend) can therefore have a working
`/dev/kvm` inside it, and run Firecracker natively.

The development machine this was investigated on is an **Apple M4 Pro running
macOS 15.7.3** — both conditions satisfied. This is worth an afternoon of
verification before provisioning anything in AWS. Note that KVM itself never runs
on macOS; the `/dev/kvm` lives inside the Linux guest, which is the level we need.

### 3.2 Cheap ARM SBC

A Raspberry Pi 5 (~$80) is a genuine aarch64 KVM host and runs Firecracker. Slow
CPU, which is irrelevant for a kernel that boots in under a second. Any Ampere or
Snapdragon mini-PC works equally well.

### 3.3 What does *not* work

- **Hetzner CAX** (Ampere Altra, from €5.99/mo) — plain KVM guests, no nested
  virt exposed. Cheapest ARM compute around, unusable here.
- **Oracle Cloud Ampere A1 free tier** (4 OCPU / 24 GB, free) — same problem, VM
  not metal.
- Any other ARM VPS — the question to ask a provider is only ever "is `/dev/kvm`
  present in the guest", and for ARM VPS the answer is essentially always no.

---

## 4. Most of the port does not need KVM at all

This is where the AWS bill actually collapses. The Firecracker port is largely a
**platform/memory-map change**, and QEMU can be pointed at Firecracker's guest
ABI without any hypervisor involved. Real metal is only needed for the last mile:
actually booting under `firecracker` + `/dev/kvm` on real Graviton. That is a
handful of spot-hours, not a standing instance.

### 4.1 What Akuma already gets right

Findings from reading the tree, not assumptions:

- **No PCI anywhere in the kernel.** `grep -i pci src/ --include="*.rs" -l`
  returns zero files. Firecracker's aarch64 default transport is virtio-MMIO
  (PCI is opt-in behind `--enable-pci` in recent versions), so Akuma is already
  on the right transport. This is the single biggest thing that would otherwise
  have been a rewrite.

- **The ARM64 Linux `Image` header already exists.** `src/boot.rs:76-86` emits a
  full 64-byte header — `b _boot_code` at code0, `text_offset = 0x100000`,
  `image_size = IMAGE_RESERVE` (linker-derived), and magic `0x644d5241`
  ("ARM\x64") at offset 56. Firecracker's aarch64 loader consumes exactly this
  format; rust-vmm's `linux-loader` describes its aarch64 path as "PE (Image)
  kernel images", which is the same arm64 Image header. **Worth confirming
  empirically** — `linux-loader`'s validation was not read directly — but the
  header Akuma emits appears to be precisely what it wants, so the image-format
  work is likely already done.

- **GICv3 is already the default.** `src/gic.rs:7` documents GICv3 as the default
  driver, and `scripts/cargo_runner.sh:231` runs `-machine virt,gic-version=3`.
  Firecracker uses an in-kernel GICv3. No interrupt-controller rewrite.

- **FDT is already the boot-info path.** `_boot_code` stashes `x0` (the DTB
  pointer) at entry before touching anything. Firecracker aarch64 is FDT-only —
  no ACPI — and passes the FDT pointer in `x0` per the Linux arm64 boot protocol.
  Same contract.

- **PL011 console.** Firecracker's aarch64 serial is PL011-compatible, which is
  the driver Akuma already has.

- **Device coverage lines up.** `crates/akuma-virtio/src/probe.rs:34-39` declares
  `NET = 1`, `BLOCK = 2`, `RNG = 4`, `SOUND = 25`. Firecracker offers virtio-net,
  virtio-block, and virtio-rng — the first three. `SOUND` has no Firecracker
  equivalent and degrades on its own: `probe()` simply returns `None`.

- **`fw_cfg` is not load-bearing.** `src/fw_cfg.rs` has exactly two callers, both
  in `src/ramfb.rs` (framebuffer setup). Firecracker has no `fw_cfg` device, and
  a headless boot never needs it. The mapping can just go away on that platform.

### 4.2 The actual deltas

**1. Memory map — the main one.**

Firecracker's aarch64 layout (`src/vmm/src/arch/aarch64/layout.rs` on `main`):

```
DRAM_MEM_START      = 0x8000_0000     (2 GiB)
MMIO32_MEM_START    = 0x4000_0000     (1 GiB)
MMIO32_MEM_SIZE     = 0x4000_0000     (1 GiB, so the window is 1 GiB .. 2 GiB)
BOOT_DEVICE_MEM_START = 0x4000_0000
RTC_MEM_START       = BOOT_DEVICE_MEM_START + MMIO_LEN
SERIAL_MEM_START    = RTC_MEM_START + MMIO_LEN
MEM_32BIT_DEVICES_START = SERIAL_MEM_START + MMIO_LEN   <- virtio-mmio lives here
```

Akuma currently sits at `KERNEL_PHYS_BASE = 0x4010_0000` (`src/config.rs:27`),
i.e. QEMU virt's RAM base + 1 MB. **Under Firecracker that address is inside the
MMIO window, not RAM.** The kernel base has to move above `0x8000_0000`.

Note this layout has evolved across Firecracker releases (older versions put the
serial at `0x4000_0000` with no boot device). Re-read `layout.rs` at whatever
version gets pinned rather than trusting these constants.

**2. Device physical addresses are hardcoded in early boot asm.**

`src/boot.rs:224-256` builds the L3 device page table with literal QEMU virt PAs:

| L3 slot | PA | Device |
|---|---|---|
| L3[0] | `0x0800_0000` | GIC distributor |
| L3[1] | — | GICv2 CPU interface |
| L3[2] | `0x0900_0000` | PL011 UART |
| L3[3] | `0x0902_0000` | fw_cfg |
| L3[4] | `0x0A00_0000` | virtio-MMIO base |
| L3[5] | `0x080A_0000` | GICv3 redistributor RD_base |
| L3[6] | `0x080B_0000` | GICv3 redistributor SGI_base |

Every one of these differs under Firecracker. The kernel-side VAs
(`crates/akuma-primitives/src/addr.rs:71-84`, the `0x80_0000_xxxx` block) are
stable and do not need to move — only the PA side of the mapping does.

**3. virtio-MMIO slot stride and count.**

`crates/akuma-virtio/src/probe.rs:17-25` hardcodes 8 slots at
`DEV_VIRTIO_VA + n * 0x200` — QEMU virt's layout. Firecracker uses a `MMIO_LEN`
stride (`0x1000`) and places one FDT node per configured device, with devices
inserted into the FDT in ascending address order. So the stride, the base, and
the slot count all change. This is a constants change, not a driver change: the
probe loop itself (`device_id_at`, `probe_with`) is layout-agnostic.

**4. No `fw_cfg`, no ramfb.**

Drop the `L3[3]` mapping and gate `src/ramfb.rs` off on this platform.

### 4.3 Recommended shape of the change

The recurring theme above is that `src/boot.rs` hardcodes one machine's physical
device map in assembly that runs before any FDT parsing. Two ways to handle it:

- **Tactical:** a `platform-firecracker` build feature that swaps the PA literals
  in the boot asm, the `KERNEL_PHYS_BASE` in `src/config.rs`, the linker base in
  `linker.ld`, and the virtio slot table in `probe.rs`. Fastest path to a booting
  microVM; leaves two hardcoded machine descriptions in the tree.

- **Structural:** keep only a minimal early-console PA hardcoded (enough to print
  before the MMU is fully configured), and derive the real device map from the
  FDT once Rust is running, re-mapping devices at that point. More work, but it
  makes the third platform free and removes a whole class of "which machine is
  this" constants.

Given that the tactical path already has to touch all four of those files, the
structural one is worth costing out before committing.

### 4.4 Suggested sequencing

1. **Local, no KVM.** Retarget QEMU `-M virt` to Firecracker's memory map,
   GICv3, and slot stride. Verify the existing boot suite still passes. This
   shakes out the large majority of the work at zero hosting cost.
2. **Local, with KVM.** Verify nested virt on the M4 Pro (§3.1); if it works,
   run real `firecracker` against the image locally and iterate until it boots.
3. **AWS spot metal.** `c6g.metal` or `m6g.metal` on spot, for a few hours, to
   confirm behaviour on real Graviton silicon and under a production-shaped
   Firecracker config. Terminate when done.

Only if step 2 fails does the AWS bill become a continuous cost, and even then
the spot pattern in §2.1 keeps it near $56/mo at normal dev cadence.

---

## 5. Open questions

- Does `linux-loader` accept Akuma's `Image` header as-is, or does it require the
  `res5` PE-header offset field (currently `0`) to be populated? §4.1 argues it
  does not, but this was not verified against the loader source.
- Does the M4 Pro / macOS 15.7.3 nested-virt path actually yield a usable
  `/dev/kvm` for Firecracker, or only for the AVF-based hypervisors? Untested.
- What is Firecracker's GIC distributor/redistributor placement? Not present in
  the `layout.rs` excerpt read; Firecracker derives it relative to the MMIO
  window top and writes it into the FDT. Needs reading before §4.2 item 2 can be
  turned into concrete constants.
- Does the boot suite (`src/process_tests.rs`) make any assumptions about the
  QEMU virt memory map that would need per-platform expectations?

---

## Sources

- [InfoQ — AWS Introduces Nested Virtualization on EC2 Instances](https://www.infoq.com/news/2026/03/aws-ec2-nested-virtualization/)
- [AWS re:Post — Nested virtualization on Graviton](https://repost.aws/questions/QUEoabj2ZERq2P5QFL6d6-RQ/nested-virtualization-on-graviton)
- Vantage instance pricing: [c6g.metal](https://instances.vantage.sh/aws/ec2/c6g.metal),
  [c7g.metal](https://instances.vantage.sh/aws/ec2/c7g.metal),
  [m6g.metal](https://instances.vantage.sh/aws/ec2/m6g.metal),
  [c8g.metal-24xl](https://instances.vantage.sh/aws/ec2/c8g.metal-24xl)
- [Firecracker aarch64 `layout.rs`](https://github.com/firecracker-microvm/firecracker/blob/main/src/vmm/src/arch/aarch64/layout.rs)
- [Firecracker FAQ](https://github.com/firecracker-microvm/firecracker/blob/main/FAQ.md)
- [Firecracker issue #1264 — virtio-mmio FDT node ordering on AArch64](https://github.com/firecracker-microvm/firecracker/issues/1264)
- [rust-vmm/linux-loader README](https://github.com/rust-vmm/linux-loader/blob/main/README.md)
- [Booting AArch64 Linux — kernel.org](https://docs.kernel.org/arch/arm64/booting.html)
- [UTM issue #6700 — Enable Nested Virtualization on macOS 15](https://github.com/utmapp/UTM/issues/6700)
- [Parallels forum — macOS 15 Sequoia nested virtualization for M3+ Macs](https://forum.parallels.com/threads/macos-15-sequoia-nested-virtualization-for-m3-macs.364397/)
