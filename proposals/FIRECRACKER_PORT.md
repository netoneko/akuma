# Firecracker port: verified constants and work plan

**Date:** 2026-08-21
**Supersedes the open questions in:** `docs/archive/PORTING_POSSIBILITIES.md`
**Status:** nothing applied. Constants below were read from Firecracker `main` and
`rust-vmm/linux-loader` `main` via the GitHub contents API, not inferred.

`PORTING_POSSIBILITIES.md` was an options survey with five open questions and a
memory map quoted from a partial excerpt. This document closes four of the five
questions, corrects the memory map, and adds three deltas that survey missed.

---

## 1. Local `/dev/kvm`: Docker is out, Virtualization.framework is in

### 1.1 Docker Desktop cannot host Firecracker — two independent blockers

Measured on this machine (Docker Desktop 4.54.0, engine 29.1.2, linuxkit VM):

```
$ docker run --rm --privileged alpine ls /dev/kvm
ls: /dev/kvm: No such file or directory

$ ... zcat /proc/config.gz | grep -i virtualiz
# CONFIG_VIRTUALIZATION is not set          <- KVM compiled out entirely

$ ... dmesg | grep -i EL
[    0.005578] CPU: All CPU(s) started at EL1   <- no EL2, so no hypervisor
```

Both blockers are outside our control:

1. **The guest kernel has KVM compiled out.** `CONFIG_VIRTUALIZATION is not set`
   in the shipped `6.12.54-linuxkit` kernel. There is no module to load and no
   `/dev/kvm` node to create. The kernel is a 40 MB blob baked into the app
   bundle (`/Applications/Docker.app/Contents/Resources/linuxkit/kernel`) with no
   supported replacement path.
2. **The VM boots at EL1.** Docker VMM (`libkrun.dylib`, same directory) does not
   request nested virtualization, so there is no EL2 for KVM to run in even with
   a KVM-capable kernel.

Fixing either one alone changes nothing. **Docker is a dead end.**

### 1.2 The host *does* support nested virtualization

Queried directly against Virtualization.framework on this machine (M4 Pro /
`T6041`, macOS 15.7.3):

```swift
VZGenericPlatformConfiguration.isNestedVirtualizationSupported  // => true
```

So `PORTING_POSSIBILITIES.md` §5 question 2 is **answered: the capability is
present.** Using it needs a VM whose creator sets
`isNestedVirtualizationEnabled = true` on a `VZGenericPlatformConfiguration`,
plus a guest kernel with `CONFIG_KVM=y` (any stock Ubuntu/Debian arm64 cloud
kernel has it as a module).

Nothing on this machine currently does that — no UTM, no lima/limactl, no
colima, no vfkit; only Homebrew `qemu-system-aarch64`, which is TCG and gives no
`/dev/kvm`.

### 1.3 How to actually get `/dev/kvm` here

Two routes. Both need a decision from you before anything is installed.

**Route A — Lima (fastest).** Lima's `vz` driver exposes the flag directly:

```bash
limactl start --vm-type=vz --nested-virt   # alias for .nestedVirtualization = true
# then, in the guest:
ls -lh /dev/kvm
```

Requirements are exactly what this machine has: Apple silicon M3 or newer,
macOS 15+. The guest is a stock Ubuntu cloud image, whose kernel ships `kvm` as a
module — the thing Docker's linuxkit kernel lacks (§1.1). Caveat: reports exist of
*QEMU*-in-guest misbehaving on Apple silicon even when `/dev/kvm` is present
(lima-vm/lima#4498). That is a QEMU issue, not a `/dev/kvm` issue, and Firecracker
is the target here — but treat "`/dev/kvm` appears" and "Firecracker boots" as two
separate things to verify.

**Route B — a small in-tree Swift VZ host (no third-party dependency).**
`swiftc` is present (`/usr/bin/swiftc`, CommandLineTools) and
Virtualization.framework is a system framework, so a ~150-line host that sets
`isNestedVirtualizationEnabled = true` on a `VZGenericPlatformConfiguration` and
boots a distro kernel + initrd is entirely doable with what is already on the
box. Nothing is downloaded but a distro cloud image — no build scripts, no
`build.rs`, no opaque binaries. Slower to stand up than Route A, and it becomes
ours to maintain.

Route A if you want KVM this week; Route B if the dependency rule matters more
than the day. Either way this is optional — Phases 1 and 2 in §6 are the bulk of
the work and need no hypervisor at all, and the approved metal instance covers
Phase 3 directly.

---

## 2. Verified Firecracker aarch64 memory map

From `src/vmm/src/arch/aarch64/layout.rs`, `gic/gicv3/mod.rs`,
`device_manager/mmio.rs` (`MMIO_LEN = 0x1000`), and `arch/aarch64/mod.rs`.

```
                                        <- GIC sits BELOW the MMIO window
GIC ITS / MSI     GICR_base - 0x2_0000              size 0x2_0000
GICR base         0x3FFF_0000 - nvcpu*0x2_0000      size nvcpu*0x2_0000
GICD              0x3FFF_0000                       size 0x1_0000  (64 KiB)
=== 0x4000_0000  MMIO32_MEM_START ===
boot device       0x4000_0000                       len 0x1000
RTC (pl031)       0x4000_1000                       len 0x1000
serial (pl011)    0x4000_2000                       len 0x1000
virtio-mmio #0    0x4000_3000   <- MEM_32BIT_DEVICES_START, stride 0x1000
virtio-mmio #k    0x4000_3000 + k*0x1000
  (hole)
PCI mmconfig      0x7000_0000 .. 0x8000_0000        (unused without --enable-pci)
=== 0x8000_0000  DRAM_MEM_START ===
ACPI/system mem   0x8000_0000 .. 0x8020_0000        SYSTEM_MEM_SIZE = 0x20_0000
kernel load base  0x8020_0000                       get_kernel_start()
  ... guest RAM ...
FDT               (0x8000_0000 + ram_size) - 0x20_0000   <- TOP of DRAM, 2 MiB
```

### 2.1 The GIC redistributor base moves with vCPU count

This is the single most important thing the earlier survey did not have, and it
changes the recommended approach (§5).

```rust
// gic/gicv3/mod.rs
const fn get_dist_addr()  -> u64 { layout::MMIO32_MEM_START - 0x1_0000 }
const fn get_redists_addr(vcpu_count: u64) -> u64 {
    get_dist_addr() - vcpu_count * (2 * 0x1_0000)
}
```

GICD is fixed at `0x3FFF_0000`. The redistributors are stacked *downward* from
it, `0x2_0000` per vCPU, so CPU0's frames depend on how many vCPUs the microVM
was configured with:

| vCPUs | GICR base = CPU0 RD_base | CPU0 SGI_base |
|---|---|---|
| 1 | `0x3FFD_0000` | `0x3FFE_0000` |
| 2 | `0x3FFB_0000` | `0x3FFC_0000` |
| 4 | `0x3FF7_0000` | `0x3FF8_0000` |
| 8 | `0x3FEF_0000` | `0x3FF0_0000` |

Akuma hardcodes CPU0's RD_base/SGI_base as literals in pre-MMU boot assembly
(`src/boot.rs:249-255`) and again in `crates/akuma-exec/src/mmu/mod.rs:132-133`.
Under QEMU virt those addresses are vCPU-count-independent; under Firecracker
they are not. **A build-time constant cannot express this** — `SMP=N` is a
runtime choice in this tree.

Note also that GICD needs **64 KiB**, not the single 4 KiB page Akuma maps. See
§4.

---

## 3. Answers to `PORTING_POSSIBILITIES.md` §5

**Q1 — Does `linux-loader` accept Akuma's Image header as-is, does it need `res5`?**

**Yes, accepted as-is; `res5` is never read.** The aarch64 arm of
`linux_loader::loader::pe::PE::load` validates exactly one thing:

```rust
#[cfg(target_arch = "aarch64")]
if u32::from_le(image_header.magic) != 0x644d_5241 {
    return Err(Error::InvalidImageMagicNumber.into());
}
```

The second magic check the file also contains (`magic2 != 0x0543_5352`) is
`#[cfg(target_arch = "riscv64")]` and does not apply.

**But `text_offset` is honored, and that changes the link address.** The loader
computes:

```rust
let text_offset = if image_header.image_size == 0 { 0x80000 }
                  else { image_header.text_offset };
// kernel_offset must be 2 MiB aligned
let mem_offset = kernel_offset.unwrap_or(0) + text_offset;
```

Firecracker passes `kernel_offset = get_kernel_start() = 0x8020_0000`. Akuma sets
`text_offset = 0x100000` and a non-zero `image_size`, so the image lands at
**`0x8030_0000`**, not `0x8020_0000`. Whatever `linker.ld` uses as
`KERNEL_PHYS_BASE` on this platform must equal `0x8020_0000 + text_offset`.
Keeping `text_offset = 0x100000` and linking at `0x8030_0000` is consistent with
the QEMU path (which also consumes `text_offset` the same way), so there is no
reason to change the header.

**Q2 — Does the M4 Pro / macOS 15.7.3 nested-virt path work?**

Capability confirmed (§1.2). End-to-end `/dev/kvm`-inside-a-guest not yet
demonstrated, because that needs a VM manager we do not have installed.

**Q3 — Firecracker's GIC distributor/redistributor placement?**

Answered in §2/§2.1. It is *not* derived from the MMIO window top as the survey
guessed — GICD is `MMIO32_MEM_START - 0x1_0000`, i.e. immediately *below* the
window, and the redistributors below that.

**Q4 — Does the boot suite assume the QEMU virt memory map?**

**Yes, extensively.** `src/tests.rs` has ~20 sites baking in `0x4000_0000` —
`0x4000_0000..0x8000_0000` treated as "the kernel identity-mapped RAM range"
(`tests.rs:3556`, `3570`, `7628`, `7642`, `7843`), `KERNEL_VA_START = 0x4000_0000`
(`6483`), and a batch of mmap-placement tests asserting nothing is returned in
that window. Under Firecracker `0x4000_0000..0x8000_0000` is the *MMIO window*
and RAM starts at `0x8000_0000`, so every one of those assertions inverts.
These need to be expressed against `akuma_exec::mmu::ram_base()` /
`kernel_va_end()` — which already exist and are already runtime-dynamic — rather
than against literals.

**Q5 (new, was not asked) — how do secondary vCPUs start?**

Same as QEMU virt: PSCI. `arch/aarch64/vcpu.rs:355` — *"Other vCPUs are powered
off initially awaiting PSCI wakeup"* — only `index == 0` gets PC and `x0` set,
and `KVM_ARM_VCPU_PSCI_0_2` is in the vCPU feature set. Akuma's existing PSCI
`CPU_ON` bringup needs no change. Good news for `smp-shared`.

---

## 4. Deltas the earlier survey missed

### 4.1 The identity map is already two-thirds right

`src/boot.rs:183-197` builds three 1 GiB L1 block entries:

| Entry | Range | Attrs today | Under Firecracker |
|---|---|---|---|
| L1[0] | `0x0000_0000-0x3FFF_FFFF` | Device | **correct** — the whole GIC (`0x3FEF_0000`+) falls in here already |
| L1[1] | `0x4000_0000-0x7FFF_FFFF` | Normal | **wrong** — this is the MMIO window; must become Device |
| L1[2] | `0x8000_0000-0xBFFF_FFFF` | Normal | **correct** — this is where RAM actually is |

So the GIC is device-mapped for free, and the first 1 GiB of Firecracker RAM is
Normal-mapped for free. Only L1[1]'s attributes are wrong, and guests larger than
1 GiB need L1[3]+ (`kernel_va_end()` already handles growth — see
`docs/archive`-era MEMORY>2GB work).

### 4.2 virtio IRQ base differs: 48 → 32

`src/main.rs:1869` hardcodes `VIRTIO_MMIO_SPI_BASE: u32 = 48`, used as
`intid = VIRTIO_MMIO_SPI_BASE + slot` (`main.rs:1432`). That is right for QEMU
virt (virtio-mmio is SPI 16..23 → INTID 48..55). Firecracker allocates from
`GSI_LEGACY_START = 0`, which is SPI #32, so **the first virtio device is
INTID 32** and they ascend with MMIO address. The base becomes 32.

### 4.3 virtio slot geometry: 8 slots × 0x200 → N slots × 0x1000

`crates/akuma-virtio/src/probe.rs:14-27` hardcodes `NUM_SLOTS = 8` at stride
`0x200`. Firecracker uses stride `MMIO_LEN = 0x1000` and only instantiates
configured devices. As the survey said, the probe *loop* is layout-agnostic —
`device_id_at`/`probe_with` don't care — so this is a table change. But note
`akuma-rump` binds the **second** virtio-net by index, so slot ordering must stay
meaningful; Firecracker assigns addresses in device-creation order, so it does.

### 4.4 The FDT moves to the top of RAM — and this is *safer*, not riskier

QEMU virt puts the DTB right after the kernel image (`0x4020_0000`), inside
Akuma's `IMAGE_RESERVE` span, so the PMM never hands it out. Firecracker puts the
FDT in the **last 2 MiB of DRAM**, which the PMM *would* hand out.

This turns out to be fine: `src/main.rs:621-627` runs `detect_memory()` and
`smp_shared::probe_dtb()` before the heap is initialized, deliberately — the
existing comment at `main.rs:624` notes the allocator "can be placed exactly at
the DTB's address". Nothing reads the FDT after that point, so a top-of-RAM FDT
being reclaimed is harmless. `detect_memory` already reads `base`/`size` from the
FDT `memory` node, and Firecracker sets `x0` per the boot protocol, so the
`scan_for_dtb()` fallback is never needed.

### 4.5 Pre-existing bug: `GICD_IROUTER` writes land on the redistributor

Found while checking whether a 4 KiB GICD page suffices. It does not, and the
overflow is currently silent.

`src/gic_v3.rs:258-260` writes `GICD_IROUTER` at distributor offset
`0x6000 + intid*8`:

```rust
let route_off = gicd::IROUTER + idx * 8;   // IROUTER = 0x6000
mmio_w32(gicd(route_off), 0);              // gicd(off) = DEV_GIC_DIST_VA + off
```

But `DEV_GIC_DIST_VA` is mapped as a **single 4 KiB page** (`DEV_PAGES` entry
`(0, 0x0800_0000)` in `crates/akuma-exec/src/mmu/mod.rs:127`), and the device VA
block assigns a distinct device every 4 KiB
(`crates/akuma-primitives/src/addr.rs:71-84`):

```
DEV_GIC_DIST_VA  = 0x80_0000_0000
DEV_GICR_SGI_VA  = 0x80_0000_6000     <- exactly DEV_GIC_DIST_VA + 0x6000
```

So every `GICD_IROUTER` write aliases onto the **GICv3 redistributor SGI frame**:
INTID *n* writes to SGI-frame offset `n*8`. Step 3 of the four-step sequence
documented at `gic_v3.rs:220-224` ("route to core 0, written explicitly rather
than relying on a reset value the architecture leaves UNKNOWN") never reaches
`GICD_IROUTER` at all.

It is benign *today* by coincidence. For the INTIDs Akuma actually enables as
SPIs — virtio at 48..55 — `n*8` lands on `0x180` (`GICR_ICENABLER0`, and the
value written is 0, a no-op for a write-1-to-clear register) and on reserved
words at `0x184..0x1B8`. The routing works only because QEMU's `GICD_IROUTER`
reset value happens to target core 0.

Two reasons this matters for the port:

- It must be fixed anyway: GICD is a 64 KiB region on both machines, and the
  device VA map needs to reserve 16 pages for it instead of 1.
- Under Firecracker the redistributors sit *below* GICD rather than above, so the
  aliasing target changes, and any future reliance on real IROUTER routing under
  `smp-shared` would break differently.

This is independent of the port and worth its own fix + a boot-suite assertion
(per `CLAUDE.md`: kernel changes need `src/process_tests.rs` self-tests). Written
up in full — evidence, the INTID-to-register aliasing table, why it is benign
today, and the fix + verify shape — in
**`docs/archive/GICD_IROUTER_ALIASING.md`**.

---

## 5. Recommended approach: structural, not tactical

`PORTING_POSSIBILITIES.md` §4.3 offered "tactical" (a `platform-firecracker`
feature swapping PA literals) vs "structural" (hardcode only an early console PA,
derive the device map from the FDT once Rust runs) and left the choice open.

**The vCPU-dependent GICR base (§2.1) settles it.** A build feature cannot encode
an address that depends on `SMP=N`, which is chosen at runtime in this tree. The
tactical path would either hardcode a single supported vCPU count or reintroduce
the same FDT lookup the structural path does — while still leaving two machine
descriptions in the tree.

Additional weight on the same side:

- The device map is already duplicated in two places that must stay in sync
  (`src/boot.rs:224-256` boot asm and `akuma-exec/src/mmu/mod.rs:126-134`
  `DEV_PAGES`, whose comment already says "Must mirror DEV_PAGES"). A third
  machine doubles that to six literal tables.
- The kernel-side VAs are already stable and do not move (§4.5 notwithstanding) —
  only the PA side of each mapping changes. That is exactly the shape that
  re-mapping-after-FDT-parse handles well.
- `detect_memory()` already proves the pattern works: RAM base and size are
  FDT-derived at runtime, with a QEMU literal only as fallback, and
  `akuma_exec::mmu::ram_base()` is already an atomic set from it
  (`mmu/mod.rs:20-30`, `FALLBACK_RAM_BASE` at `:89`).
- §4.4 shows the ordering constraint is already satisfied: FDT parsing happens
  before the allocator exists, which is precisely when a device re-map must run.

Sketch:

1. Boot asm maps only what is needed to print: the UART page, plus the two 1 GiB
   identity blocks. Keep a compile-time UART PA (it is the one thing that must
   work before FDT parsing) with the platform selected by feature — this is the
   single remaining literal.
2. After `detect_memory()`, parse the FDT for `intc` (`reg` gives GICD and GICR
   bases and sizes), `pl011`, and the `virtio_mmio` nodes (`reg` → base+stride,
   `interrupts` → INTID). Both platforms emit all of these.
3. Rebuild the shared device L3 from the parsed table instead of `DEV_PAGES`, and
   feed `probe.rs` a runtime slot table instead of a `const`.
4. Fix the GICD span to 64 KiB while re-laying out the device VA block (§4.5).

Step 4's VA reshuffle is a breaking change to `addr.rs`'s published constants, so
it is worth doing in the same pass rather than twice.

### 5.1 What still needs no work

Confirming the survey's §4.1, all still true: no PCI anywhere in the kernel;
GICv3 already the default; FDT-in-`x0` already the boot-info path; PL011 console;
virtio net/block/rng already covered (`SOUND` degrades to `None` on its own);
`fw_cfg` has only two callers, both in `src/ramfb.rs`, so dropping its mapping and
gating `ramfb.rs` off is self-contained. Plus, newly: PSCI secondary bringup
(§3 Q5) and two of three identity blocks (§4.1).

### 5.2 What "just shift the addresses" actually does

Worth spelling out, because it is the obvious move and it *almost* works. "Just
shift" means: move `KERNEL_PHYS_BASE` to `0x8030_0000` in `config.rs` + `linker.ld`,
rewrite the L3 device PAs in `boot.rs` and `DEV_PAGES`, set
`VIRTIO_MMIO_SPI_BASE = 32`, retable `probe.rs` for stride `0x1000`, drop fw_cfg
and gate off `ramfb.rs`.

**It boots.** On a single-vCPU microVM that list is very likely sufficient — the
header is accepted as-is (§3 Q1), PSCI bringup is unchanged (§3 Q5), the FDT
arrives in `x0`, `detect_memory()` reads the right RAM base off the FDT, and
`extend_boot_ram_identity_map` is accidentally correct (§5.3). Then three things
are wrong, in descending order of how badly.

**1. `SMP=N` silently drives the wrong redistributor.** This is the one that makes
the shift not merely ugly but incorrect. Literals have to be pinned to some vCPU
count; pin them to 1 and CPU0's frames are `0x3FFD_0000`/`0x3FFE_0000`. Boot with
`vcpu_count: 2` and those same addresses are **CPU1's** frames (§2.1). Every
consumer follows the literal:

- `gic_v3::init` step 2 clears `GICR_WAKER.ProcessorSleep` on CPU1's
  redistributor (`src/gic_v3.rs:165-170`). `GICR_WAKER` is per-PE, so CPU0's stays
  asleep and CPU0 receives no SGIs or PPIs at all.
- Step 3 configures CPU1's SGI/PPI group, priority and enable state
  (`src/gic_v3.rs:172-180`).
- `enable_irq(27)` for the virtual timer (`src/main.rs:956`) takes the
  `irq < 32` arm and writes CPU1's `GICR_ISENABLER0`. **The boot core never gets a
  timer interrupt — no preemption, no scheduler tick.**
- `enable_irq(gic::SGI_SCHEDULER)` (`src/main.rs:944`) likewise.

There is no build error and no boot error. And it is differently wrong per
configuration: at `SMP=4` CPU0 drives CPU3's frames. Given that `smp-shared` is in
the default feature set and `SMP=N` is how this tree is normally run, a
build-time constant simply cannot express this address. **Fixing item 1 requires a
runtime value, which is the structural change.**

**2. L1[1] becomes a mismatched-attribute alias over live MMIO.** `src/boot.rs:187-191`
maps `0x4000_0000-0x7FFF_FFFF` as a 1 GiB **`NORMAL_BLOCK`** — correct on QEMU
virt, where that is RAM. Under Firecracker that range holds the PL011 at
`0x4000_2000` and every virtio slot from `0x4000_3000` up. Those same physical
addresses are simultaneously mapped Device-nGnRnE through L0[1]. One physical
location, two mappings, Normal-cacheable versus Device: CONSTRAINED UNPREDICTABLE
per the ARM ARM.

The kernel never *deliberately* touches the Normal alias — all device access goes
through `DEV_*_VA` — but a cacheable mapping over device memory permits
speculative reads, line fills and dirty write-backs against a UART FIFO or a
virtio doorbell. The fix is one word (`DEVICE_BLOCK` for `NORMAL_BLOCK`), and it
must not be forgotten, because the symptom is a random SError or a device
register that changed by itself, arbitrarily far from the cause. Note also that
`extend_boot_ram_identity_map`'s flags comment — *"same attributes boot.rs uses"*,
`crates/akuma-exec/src/mmu/mod.rs:71` — stops being true the moment L1[1] changes.

**3. The boot suite keeps passing while covering nothing.** The subtle one, and
the reason §6 puts the test rework in Phase 1. `src/tests.rs` treats
`0x4000_0000..0x8000_0000` as "the kernel identity-mapped RAM range" in ~20
places. Under Firecracker that range is MMIO and RAM starts at `0x8000_0000`:

- `tests.rs:7588` (`0x4000_0000, // first 2MB block (always mapped)`) **fails
  loudly.** Fine — a failing test is a working test.
- `tests.rs:7642`/`7653` ("mmap never returned addresses in
  0x40000000-0x7FFFFFFF") **passes vacuously.** `mmap` would never hand back an
  MMIO address anyway. Meanwhile the collision that assertion exists to catch — a
  user mapping landing on the kernel identity window — has moved to
  `0x8000_0000+`, where nothing checks it.
- `tests.rs:3570` (`a < 0xC000_0000 && end > 0x4000_0000`) still covers the real
  kernel window by luck for guests <= 1 GiB, since `0x8000_0000..0xC000_0000` is
  inside that range, and silently stops covering anything above `0xC000_0000`.

The tests that break are not the problem. The tests that keep passing are: the
regression net goes quiet exactly where the new memory map creates new
collisions. Rewriting them against `akuma_exec::mmu::ram_base()` /
`kernel_va_end()` — both already runtime-dynamic — is platform-neutral work that
should land *before* a second platform exists.

**Plus the maintenance shape.** The literals live in at least eight places that
must stay mutually consistent: `src/boot.rs:183-197` (L1 blocks),
`src/boot.rs:224-256` (L3 device PAs), `crates/akuma-exec/src/mmu/mod.rs:126-134`
(`DEV_PAGES`, whose own comment already says *"Must mirror DEV_PAGES"*),
`crates/akuma-primitives/src/addr.rs:71-84` (VAs), `crates/akuma-virtio/src/probe.rs:14-27`
(slot table), `src/main.rs:1869` (SPI base), `src/config.rs:27` + `linker.ld:21`
(kernel base), and `crates/akuma-exec/src/mmu/mod.rs:89-90` (`FALLBACK_RAM_BASE`/
`FALLBACK_RAM_END`, which under Firecracker would point the kernel-RAM window at
the MMIO window whenever `RAM_SIZE` has not been stored yet). `#[cfg]`-forking
each doubles that to sixteen, and the mirror invariant now has to hold across two
machine descriptions instead of one.

**Verdict.** The shift buys a booting single-vCPU microVM quickly and costs SMP,
an architecturally illegal alias, and the regression net. Items 1 and 3 are not
fixable with better literals — 1 needs a runtime value, 3 needs the assertions
rewritten — and together those are most of the structural work anyway. So the
shift is a good **spike** (that is exactly Phase 2 in §6, run on QEMU, where none
of the three bite) and a bad **destination**.

### 5.3 What survives the shift unchanged

Stated so the estimate is not inflated. Verified, not assumed:

- **`extend_boot_ram_identity_map` is accidentally correct**
  (`crates/akuma-exec/src/mmu/mod.rs:55`). It works in absolute L1-index space
  from `BOOT_STATIC_MAP_END = 0xC000_0000` rather than relative to `ram_base`,
  and `boot.rs` still statically maps `[0, 3 GiB)`. A 1 GiB Firecracker guest ends
  at exactly `0xC000_0000` and needs no extension; a 2 GiB guest extends L1[3]
  correctly. Worth stating explicitly because the obvious worry — that boot.rs
  only maps 1 GiB above `0x8000_0000` — does not hold: the static map is
  `[0, 3 GiB)` in absolute terms, so Firecracker's RAM base at 2 GiB leaves 1 GiB
  statically mapped and the extension covers the rest.
- `detect_memory()` already reads base and size from the FDT `memory` node, and
  `ram_base()`/`ram_end()`/`kernel_va_end()` are already dynamic.
- The FDT landing at the top of DRAM is harmless, and marginally safer than
  QEMU's placement (§4.4).
- PSCI secondary bringup, the PL011 driver, the virtio probe *loop*, the GICv3
  CPU interface (all system registers), and two of three identity L1 blocks
  (§4.1).

---

## 6. Sequencing

Revised from `PORTING_POSSIBILITIES.md` §4.4, with the QEMU-only phase carrying
more weight now that the structural approach is chosen.

**Phase 1 — QEMU, no KVM. The bulk of the work.**
Do the structural refactor (§5 steps 1-4) with QEMU virt as the only platform.
Nothing about deriving the device map from the FDT needs Firecracker; QEMU emits
the same nodes. Success criterion: the boot suite passes at `SMP=1/2/4` with
every device address FDT-derived and the literals gone. Fix §4.5 here.
Then re-express the `src/tests.rs` map assertions against `ram_base()` /
`kernel_va_end()` (§3 Q4) so they are platform-neutral before a second platform
exists.

**Phase 2 — ~~QEMU, Firecracker's map~~. Not possible; corrected 2026-08-21.**
The original plan here was to run QEMU with RAM at `0x8000_0000` and validate the
memory-map change without a hypervisor. That does not work: `qemu-system-aarch64
-M virt` has its RAM base and every device address baked into the machine model
(`hw/arm/virt.c`'s `base_memmap`), and there is no flag to relocate them. QEMU's
`microvm` machine, which would be the closest analogue, is x86-only.

So the Firecracker memory map can only be exercised under actual Firecracker,
which makes `/dev/kvm` a **gate** on this phase rather than a convenience. What
*can* be done without KVM — and was — is verify the link address statically:
`nm` the `platform-firecracker` build and check `_boot` against
`get_kernel_start() + text_offset`.

**Phase 3 — real Firecracker.** Needs `/dev/kvm`. Either a local nested-virt VM
(§1.2, needs a VM manager decision from you) or straight to AWS.

**Phase 4 — AWS metal.** §7.

Phase 1 is the majority of the work and costs nothing. Phase 2 collapsed into a
static check (above), so the first real Firecracker signal now comes from Phase 3
— which needs either the local nested-virt VM or the metal instance.

---

## 7. AWS metal: what to have ready

Since the instance is approved, the prep worth doing now is the part that makes
the metal hours short.

### 7.1 Instance choice

`m6g.metal` on **spot** is the pick: `PORTING_POSSIBILITIES.md` §2 measured spot
at $0.679/hr against $2.464 on-demand, and 256 GiB RAM over `c6g.metal`'s 128 GiB
costs nothing on spot. Spot interruption is a non-event for this workload — boot
a microVM, read the console, kill it. Graviton2 being ARMv8.2 (no nested-virt
extension) is irrelevant: the metal instance is L0 and runs KVM directly.

`us-east-1`. Confirm the current spot price at launch rather than trusting the
2026-08-18 figures.

### 7.2 Host checks, in order

```bash
ls -l /dev/kvm                      # present on metal by default
lsmod | grep kvm                    # kvm module loaded
cat /proc/cpuinfo | grep -c processor   # 64 -> vCPU budget for SMP tests
dmesg | grep -i kvm                 # "KVM: VHE mode initialized" or nVHE
```

If `/dev/kvm` is missing on a metal instance, the AMI is the problem, not the
hardware.

### 7.3 Things to decide before launching

- **Firecracker binary provenance.** Firecracker ships as a GitHub release
  artifact. Per your standing rule on third-party dependencies, name the exact
  version and source you want used; I will not pick one. Note it is a static
  binary — no build step, so no `build.rs` exposure.
- **AMI.** AL2023 arm64 or Ubuntu 24.04 arm64; both carry `kvm-arm`.
- **Getting the image up.** Akuma's kernel is a flat binary plus the ext2 disk
  image (`scripts/create_disk.sh`). The disk image is the big artifact — decide
  whether it is built on the metal host (64 cores makes that fast, and the
  self-host toolchain already exists) or shipped from here.

### 7.4 Firecracker config shape

For reference when Phase 3/4 arrives — one vCPU, one virtio-net, one
virtio-block, serial console:

```json
{
  "boot-source": {
    "kernel_image_path": "akuma.bin",
    "boot_args": "console=ttyS0"
  },
  "drives": [{
    "drive_id": "rootfs", "path_on_host": "devbox.img",
    "is_root_device": true, "is_read_only": false
  }],
  "machine-config": { "vcpu_count": 1, "mem_size_mib": 2048 },
  "network-interfaces": [{
    "iface_id": "eth0", "host_dev_name": "tap0"
  }]
}
```

With `vcpu_count: 1`, CPU0's GICR frames are at `0x3FFD_0000`/`0x3FFE_0000`
(§2.1) — worth pinning the first bring-up to one vCPU so the FDT-derived path can
be checked against a known-good number before `SMP=N` varies it.

`boot_args` is passed but Akuma ignores the cmdline; harmless.

---

## Sources

All constants read 2026-08-21 via the GitHub contents API at `main`.

- `firecracker-microvm/firecracker`: `src/vmm/src/arch/aarch64/layout.rs`,
  `src/vmm/src/arch/aarch64/mod.rs` (`get_kernel_start`, `get_fdt_addr`,
  `load_kernel`), `src/vmm/src/arch/aarch64/vcpu.rs` (`setup_boot_regs`),
  `src/vmm/src/arch/aarch64/gic/gicv3/mod.rs`,
  `src/vmm/src/arch/aarch64/gic/gicv2/mod.rs`,
  `src/vmm/src/device_manager/mmio.rs` (`MMIO_LEN`)
- `rust-vmm/linux-loader`: `src/loader/pe/mod.rs`
- Local measurements: Docker Desktop 4.54.0 linuxkit guest; Swift query against
  Virtualization.framework on macOS 15.7.3 / Apple M4 Pro
- Background: `docs/archive/PORTING_POSSIBILITIES.md`,
  `docs/archive/GICD_IROUTER_ALIASING.md`
- Lima nested virtualization: [lima-vm/lima#2530](https://github.com/lima-vm/lima/pull/2530),
  [VZ driver docs](https://lima-vm.io/docs/config/vmtype/vz/),
  [lima-vm/lima#4498](https://github.com/lima-vm/lima/issues/4498)
