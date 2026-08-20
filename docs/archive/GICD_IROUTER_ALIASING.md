# `GICD_IROUTER` writes land on the redistributor, not the distributor

**Date:** 2026-08-21
**Found:** while checking whether a 4 KiB GICD mapping suffices for the
Firecracker port (`proposals/FIRECRACKER_PORT.md` §4.5).
**Status:** OPEN. Not fixed. Benign today by coincidence; the coincidence is not
load-bearing on purpose, and it changes shape on a second platform.
**Severity:** latent. No known misbehaviour on QEMU virt today. Do not close this
on the strength of "SMP works fine" — see §4.

---

## 1. The claim

Step 3 of `gic_v3::enable_irq`'s four-step SPI configuration sequence never
reaches `GICD_IROUTER`. Every one of those writes goes to the **GICv3
redistributor SGI frame** instead, because the distributor is mapped as a single
4 KiB page and `GICD_IROUTER` lives at distributor offset `0x6000`.

## 2. Evidence

Three facts, each independently checkable.

**a. `GICD_IROUTER` is at offset `0x6000` and is written via the distributor base.**

`src/gic_v3.rs:35`:
```rust
pub const IROUTER: usize = 0x6000; // 8 bytes per INTID, from INTID 32
```

`src/gic_v3.rs:258-260`:
```rust
let route_off = gicd::IROUTER + idx * 8;
mmio_w32(gicd(route_off), 0);      // Aff0/Aff1
mmio_w32(gicd(route_off + 4), 0);  // Aff2/Aff3, IRM=0 (targeted)
```

`src/gic_v3.rs:66-68`:
```rust
fn gicd(off: usize) -> usize {
    mmu::DEV_GIC_DIST_VA + off
}
```

**b. The distributor is mapped as exactly one 4 KiB page.**

`crates/akuma-exec/src/mmu/mod.rs:126-134`:
```rust
const DEV_PAGES: &[(usize, usize)] = &[
    (0, 0x0800_0000), // L3[0]: GIC distributor (GICD, shared by v2 & v3)
    ...
```

One L3 slot, one 4 KiB page. `src/boot.rs:229-231` writes the same single entry
in pre-MMU assembly.

**c. `DEV_GIC_DIST_VA + 0x6000` *is* `DEV_GICR_SGI_VA`.**

`crates/akuma-primitives/src/addr.rs:71-84` assigns one device per 4 KiB:
```
DEV_GIC_DIST_VA = 0x80_0000_0000
DEV_GIC_CPU_VA  = 0x80_0000_1000
DEV_UART_VA     = 0x80_0000_2000
DEV_FW_CFG_VA   = 0x80_0000_3000
DEV_VIRTIO_VA   = 0x80_0000_4000
DEV_GICR_RD_VA  = 0x80_0000_5000
DEV_GICR_SGI_VA = 0x80_0000_6000     <- 0x80_0000_0000 + 0x6000
```

So `gicd(0x6000 + idx*8) == DEV_GICR_SGI_VA + idx*8`, exactly. INTID *n*'s
"IROUTER write" is a write to SGI-frame offset `n*8`.

## 3. Why nothing has broken

Two separate coincidences hold at once.

**The aliased writes are no-ops for the INTIDs Akuma uses.** Akuma enables
exactly one PPI (the virtual timer, INTID 27, `src/main.rs:956`), the scheduler
SGI (`src/main.rs:944`), and the virtio-mmio SPIs. PPIs and SGIs take the
`irq < 32` early-return arm in `enable_irq` (`gic_v3.rs:233-238`) and never reach
step 3 at all. That leaves the virtio SPIs, which on QEMU virt are SPI 16..23 →
INTID 48..55 (`VIRTIO_MMIO_SPI_BASE = 48`, `src/main.rs:1869`):

| INTID | `n*8` | SGI-frame register hit | Written value | Effect |
|---|---|---|---|---|
| 48 | `0x180` | `GICR_ICENABLER0` | `0` | none — write-1-to-clear |
| 48 (+4) | `0x184` | reserved | `0` | none |
| 49..55 | `0x188`..`0x1BC` | reserved | `0` | none |

The only architecturally live register in range is `GICR_ICENABLER0` at `0x180`,
and a zero write to a write-1-to-clear register clears nothing.

**The routing works anyway, from a reset value.** With step 3 a no-op, SPIs are
routed by whatever `GICD_IROUTER` resets to. On QEMU's GICv3 that is 0 —
Aff0/1/2/3 = 0, `IRM` = 0 — which targets core 0, which is what Akuma wanted.

The comment at `src/gic_v3.rs:220-222` is therefore precisely inverted from
reality:

> 3. `GICD_IROUTER` — affinity 0.0.0.0 (core 0), written explicitly rather
>    than relying on a reset value the architecture leaves UNKNOWN.

It *is* relying on that reset value, and on nothing else.

## 4. Why it still matters

**The architecture does not promise the reset value.** The comment is right that
`GICD_IROUTER` is UNKNOWN at reset; it is only wrong about Akuma having dealt
with it. Any GIC implementation that resets IROUTER to something other than 0
delivers Akuma's virtio interrupts to a core that is not the boot core, and the
handler is only installed on the boot core. That is a dead NIC with no error
message.

**The blast radius grows with INTID.** The aliasing maps INTID *n* to SGI-frame
offset `n*8`, and the SGI frame is not sparse further out:

| INTID | `n*8` | SGI-frame register | Consequence of a zero write |
|---|---|---|---|
| 16 | `0x080` | `GICR_IGROUPR0` | would move all SGIs/PPIs to Group 0 — but INTID < 32 early-returns, so unreachable |
| 32 | `0x100` | `GICR_ISENABLER0` | none (write-1-to-set) |
| 64 | `0x200` | `GICR_ISPENDR0` | none (write-1-to-set) |
| 80 | `0x280` | `GICR_ICPENDR0` | none (write-1-to-clear) |
| 96 | `0x300` | `GICR_ISACTIVER0` | none (write-1-to-set) |
| 112 | `0x380` | `GICR_ICACTIVER0` | none (write-1-to-clear) |
| **128** | **`0x400`** | **`GICR_IPRIORITYR0`** | **sets SGI 0-3 priority to 0** |
| 384 | `0xC00` | `GICR_ICFGR0` | reconfigures SGI 0-15 trigger |
| 416 | `0xD00` | `GICR_IGRPMODR0` | changes SGI/PPI group modifier |
| 448 | `0xE00` | `GICR_NSACR` | changes non-secure access control |

Enabling any SPI at INTID >= 128 corrupts redistributor state for real. Nothing
in the tree does that today — the highest is 55 — so this is a trap set for
whoever adds the next device, not a present bug. A device on a machine with more
SPIs, or a `VIRTIO_MMIO_SPI_BASE` change, walks into it.

**It blocks the GICD span fix.** GICD is a 64 KiB region on every GICv3
implementation. Mapping it as one page is wrong independently of IROUTER; the
IROUTER aliasing is just the first symptom to surface. Fixing the span requires
reserving 16 L3 slots for GICD, which collides with the current one-device-per-
4 KiB VA assignment in `addr.rs` and therefore moves every `DEV_*_VA` constant.

**It changes shape under Firecracker.** The port moves the redistributors *below*
the distributor in physical space (`proposals/FIRECRACKER_PORT.md` §2.1) and the
virtio SPI base from 48 to 32. The VAs do not change, so the aliasing target does
not change either — but the coincidence in §3 now rests on KVM's vGICv3 IROUTER
reset value rather than QEMU's, and on INTIDs 32..39 landing on
`GICR_ISENABLER0` and reserved words instead of `GICR_ICENABLER0`. Both happen to
still be benign. That is two platforms relying on two separate accidents.

## 5. Fix

Not applied. The shape:

1. Give GICD its true 64 KiB span — 16 contiguous L3 slots, in both
   `DEV_PAGES` (`crates/akuma-exec/src/mmu/mod.rs:126`) and the boot assembly
   (`src/boot.rs:229-231`).
2. Re-lay out the `DEV_*_VA` block in `crates/akuma-primitives/src/addr.rs:71-84`
   so no device sits inside GICD's span. This is a breaking change to published
   constants, so it is worth doing in the same pass as any other device-map work
   rather than twice — see `proposals/FIRECRACKER_PORT.md` §5.
3. Consider whether the redistributor frames want their real 64 KiB spans too,
   for the same reason.

Do not "fix" this by clamping `enable_irq` to INTID < 128. That preserves the
aliasing and hides it better.

## 6. Verify

Per `CLAUDE.md`, a kernel change needs a boot-suite self-test in
`src/process_tests.rs`. The assertion that would have caught this:

- After `enable_irq(irq)` for an SPI, read back `GICD_IROUTER` for that INTID
  and assert it equals the value written. Today that read goes to the SGI frame
  too, so the test passes vacuously until step 1 of §5 lands — which is the
  point: write the test *and* the span fix together, and confirm the test fails
  with the span fix reverted.
- Separately, assert `DEV_GIC_DIST_VA + gicd::IROUTER` does not fall inside any
  other `DEV_*_VA` page. That one is a pure `const` assertion, catches the whole
  class, and can land immediately — before any span work.

The second check is cheap enough to add now and would have failed the day
`IROUTER` was introduced.

---

## Background

- `proposals/FIRECRACKER_PORT.md` §4.5 — where this was found, and why the port
  wants the same VA-block rework.
- `docs/archive/AKUMA_NET_ISSUES.md` §3.1 — the investigation that added the
  four-step SPI sequence (group/priority/route before enable), i.e. the change
  that introduced the IROUTER write.
- `docs/reference/subsystems/irq.md` — current-state IRQ documentation.
