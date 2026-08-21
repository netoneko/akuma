# Firecracker aarch64 guest physical memory map

**Stability: A** — measured. Read from Firecracker **v1.16.1**
(`src/vmm/src/arch/aarch64/layout.rs`, `gic/gicv3/mod.rs`, `arch/aarch64/mod.rs`,
`device_manager/mmio.rs`), confirmed identical on `main` on 2026-08-21, and then
**confirmed against the FDT Firecracker actually emits** — at 1, 2, 4 and 8 vCPUs
on an `m6g.metal` host. The blobs and a per-value comparison are in
`fdt/`; the procedure is `docs/runbooks/dump-firecracker-fdt.md`.

Confirmed by measurement: the GICD base and its 64 KiB span, the GICR base moving
`-0x20000` per vCPU, the serial at `0x40002000`, virtio-mmio at `0x40003000+`
stride `0x1000` with INTIDs from SPI 32, DRAM at `0x80000000`, and the kernel load
address and FDT placement (§3).

**The serial is a 16550, not a PL011.** The FDT advertises
`compatible = "ns16550a"`. The map below says PL011 because that is the driver
Akuma points at it, and transmit works either way — `DR` and `THR` are both at
offset `0x00` — but the status registers differ (`FR` at `0x18` vs `LSR` at
`0x05`), so console *input* on this platform is reading the wrong register.
Details: `fdt/README.md`.

**One correction the source reading did not give.** The FDT `memory` node starts
at **`0x80200000`, not `0x80000000`**: Firecracker reserves the first 2 MiB of
DRAM (`SYSTEM_MEM_SIZE`) and the node describes only what follows, so with
1024 MiB configured it reads `<0x0 0x80200000 0x0 0x3fe00000>` — 1022 MiB. The
map below is right about where DRAM begins; anything reading the `memory` node,
`detect_memory()` included, sees `0x80200000`.

These constants have moved between Firecracker releases. **Re-read them if the
pinned version changes.**

```
                                          ← the GIC is BELOW the MMIO window
GIC ITS / MSI     GICR_base - 0x2_0000                size 0x2_0000
GIC redistributors 0x3FFF_0000 - n*0x2_0000            size n * 0x2_0000
GIC distributor   0x3FFF_0000                          size 0x1_0000  (64 KiB)
── 0x4000_0000   MMIO32_MEM_START ──────────────────────────────────────
boot device       0x4000_0000                          len 0x1000
RTC (PL031)       0x4000_1000                          len 0x1000   (unmapped by Akuma)
serial (PL011)    0x4000_2000                          len 0x1000   ← the console
virtio-mmio #0    0x4000_3000    ← MEM_32BIT_DEVICES_START, stride 0x1000
virtio-mmio #k    0x4000_3000 + k*0x1000
   (hole)
PCI mmconfig      0x7000_0000 .. 0x8000_0000           unused without --enable-pci
── 0x8000_0000   DRAM_MEM_START ────────────────────────────────────────
reserved          0x8000_0000 .. 0x8020_0000           SYSTEM_MEM_SIZE = 0x20_0000
kernel load       0x8020_0000    ← get_kernel_start()
                  0x8030_0000    ← where Akuma actually lands (+ text_offset)
   ... guest RAM ...
initrd            just below the FDT                   (Akuma uses none)
FDT               (0x8000_0000 + ram_size) - 0x20_0000 ← TOP of DRAM, 2 MiB
```

## 1. Derivations

Nothing here is a bare literal in Firecracker; each is computed.

| Value | Expression |
|---|---|
| `DRAM_MEM_START` | `0x8000_0000` |
| `SYSTEM_MEM_SIZE` | `0x20_0000` (2 MiB, reserved at the base of DRAM) |
| kernel load base | `get_kernel_start()` = `SYSTEM_MEM_START + SYSTEM_MEM_SIZE` |
| Akuma's link address | `get_kernel_start() + text_offset` = `0x8020_0000 + 0x10_0000` |
| `MMIO32_MEM_START` | `1 << 30` |
| `MMIO_LEN` | `0x1000` |
| RTC | `BOOT_DEVICE_MEM_START + MMIO_LEN` |
| serial | `RTC_MEM_START + MMIO_LEN` |
| virtio base | `SERIAL_MEM_START + MMIO_LEN` |
| GIC distributor | `MMIO32_MEM_START - 0x1_0000` |
| GIC redistributors | `dist_addr - vcpu_count * 0x2_0000` |
| `PCI_MMCONFIG_START` | `DRAM_MEM_START - (256 << 20)` |
| FDT | `dram.last_addr() - FDT_MAX_SIZE + 1`, `FDT_MAX_SIZE = 0x20_0000` |

## 2. The kernel load address

`rust-vmm/linux-loader`'s aarch64 PE loader computes
`kernel_load = kernel_offset + text_offset`, validates only the `0x644d5241`
magic at header offset 56, and never reads `res5`. It requires `kernel_offset` to
be 2 MiB aligned (`0x8020_0000` is).

Two header fields matter:

- `text_offset` — honoured as-is when `image_size != 0`. Akuma declares
  `0x10_0000`.
- `image_size` — if **zero**, the loader ignores `text_offset` and substitutes
  `0x80000`. Akuma's is linker-derived and non-zero.

Hence `0x8030_0000`, which `linker.ld` must match exactly. Verified:
`nm` reports `0000000080300000 T _boot`.

## 3. Empirically confirmed

From a real boot (`docs/archive/AKUMA_FIRECRACKER_KVM.md`), 512 MiB guest:

```
DTB ptr from boot (x0 arg): 0x9fe00000
Kernel binary: 3948 KB (0x80300000 - 0x806db070)
[Memory] Detected from DTB: base=0x80200000, size=510 MB
Timer frequency: 24000000 Hz
```

- `x0 = 0x9fe00000` = `0xA000_0000 - 0x20_0000` — the FDT is in the **last 2 MiB
  of DRAM**, as derived above.
- The FDT `memory` node begins at `0x8020_0000`, not `0x8000_0000`: the first
  2 MiB is reserved, so guest-usable RAM starts where the kernel loads. Akuma's
  `detect_memory()` handles this unchanged.

## 4. Consequences for Akuma

- **`0x4000_0000..0x8000_0000` is MMIO, not RAM.** `boot.rs`'s L1[1] identity
  block must be Device on this platform; mapping it Normal-cacheable aliases live
  device registers with mismatched attributes (CONSTRAINED UNPREDICTABLE). Chosen
  by `platform::machine::MMIO_WINDOW_IS_DEVICE`.
- **The boot map already covers RAM.** `boot.rs` statically maps `[0, 3 GiB)` as
  three 1 GiB blocks, so a Firecracker guest of ≤ 1 GiB is fully covered, and
  `mmu::extend_boot_ram_identity_map` — which works in absolute L1-index space
  from `0xC000_0000` — correctly extends beyond that.
- **The GIC is inside L1[0].** Both the distributor and the redistributors sit
  below `0x4000_0000`, which `boot.rs` maps as a 1 GiB device block, so
  secondaries can reach the redistributor through the identity map during bringup.
- **The FDT at top-of-RAM is safe.** All FDT reads (`detect_memory`,
  `smp_shared::probe_dtb`) complete before the heap exists, so the PMM later
  reclaiming those pages is harmless.
- **The redistributor base is vCPU-dependent**, which no build-time constant can
  express. See the platform reference §3.3.
