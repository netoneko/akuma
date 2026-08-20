# Akuma on Firecracker: first boot, and the five reset-value assumptions it exposed

**Date:** 2026-08-21
**Result:** Akuma boots on Firecracker v1.16.1 under KVM on an Apple M4 Pro, reaches
its idle loop with zero tripwire poison, passes its memory test suite, and drives
virtio-blk. It does not yet mount ext2.
**Reproduce:** `docs/runbooks/run-on-firecracker.md`
**Design:** `proposals/FIRECRACKER_PORT.md`

---

## 1. What was achieved

Two things that were open questions the day before:

- **A local aarch64 KVM host on a Mac.** macOS 15.7.3 / M4 Pro, via a Lima VM
  with nested virtualization. Verified from inside the guest:
  ```
  crw-rw---- 1 root kvm 10, 232 /dev/kvm
  [    0.056826] CPU: All CPU(s) started at EL2
  [    0.174297] kvm [1]: nv: 570 coarse grained trap handlers
  CONFIG_VIRTUALIZATION=y
  CONFIG_KVM=y
  ```
  `started at EL2` is the line that matters — that is nested virt actually
  engaging, not merely being supported by the silicon.

- **Akuma running as a Firecracker guest.** Console, FDT-derived RAM, PMM, MMU,
  exec, GIC, timer, the memory test suite, and virtio-blk all work.

Boot log highlights from the run with a disk attached:

```
DTB ptr from boot (x0 arg): 0x9fe00000
Akuma Kernel starting...
[Platform] firecracker device map installed
Kernel binary: 3948 KB (0x80300000 - 0x806db070)
[Memory] Detected from DTB: base=0x80200000, size=510 MB
GIC initialized
Timer frequency: 24000000 Hz
Memory Tests: ALL PASSED
[Block] Found virtio-blk at slot 0
[Block] Capacity: 2048 MB (4194304 sectors)
[FS] Initializing filesystem...          <- stops here
```

Three predictions from `proposals/FIRECRACKER_PORT.md` confirmed empirically:

- The FDT lands at the **top of DRAM**: `x0 = 0x9fe00000`, which is exactly
  `0xA000_0000 - 0x20_0000` (`FDT_MAX_SIZE`) for a 512 MiB guest. §4.4 predicted
  this, and predicted correctly that it is harmless because all FDT reads finish
  before the allocator exists.
- The kernel loads at **`0x8030_0000`** = `get_kernel_start()` + `text_offset`.
  §3 Q1's arithmetic held; the Image header was accepted with no changes.
- **The FDT `memory` node starts at `0x8020_0000`, not `0x8000_0000`** — the
  first 2 MiB (`SYSTEM_MEM_SIZE`) is reserved, so guest-visible RAM begins where
  the kernel loads. Akuma's existing `detect_memory()` handled this with no
  changes at all.

## 2. Docker Desktop cannot be the KVM host

Recorded so it is not retried. Two independent blockers, both outside our control:

```
$ docker run --rm --privileged alpine zcat /proc/config.gz | grep -i virtualiz
# CONFIG_VIRTUALIZATION is not set        <- KVM compiled out of the kernel
$ ... dmesg | grep -i EL
[0.005578] CPU: All CPU(s) started at EL1  <- no EL2 to run a hypervisor in
```

The kernel is a 40 MB blob in `Docker.app/Contents/Resources/linuxkit/kernel`
with no supported replacement, and Docker VMM (`libkrun.dylib`) never asks
Virtualization.framework for nested virt. Fixing either alone changes nothing.

Contrast with the Lima guest above, which is the same hardware doing it right.

## 3. The theme: five places Akuma trusted a reset value

Every single failure on the way to a boot was the same mistake in a different
place — **depending on the value a register or address range happens to have
because QEMU happens to provide it.** KVM is deliberately hostile to this, which
is what made Firecracker such an effective test of the assumption.

Worth stating because it predicts where the *next* bugs are: not in logic, but in
constants that were only ever validated against one machine.

### 3.1 `GICD_IROUTER` writes landed on the redistributor

Found by inspection while scoping the port, before any boot. The distributor was
mapped as a single 4 KiB page while `GICD_IROUTER` lives at offset 0x6000, and
`DEV_GIC_DIST_VA + 0x6000` was exactly `DEV_GICR_SGI_VA` — so step 3 of
`enable_irq`'s four-step SPI sequence had never once reached the distributor.

It worked anyway because QEMU's `GICD_IROUTER` resets to 0, which targets core 0,
which is what the code wanted. Full analysis, including the INTID→register
aliasing table and why INTID ≥ 128 would corrupt redistributor state for real:
**`docs/archive/GICD_IROUTER_ALIASING.md`**.

Fixed by giving each device a *span* rather than a page in
`akuma_primitives::addr`, with a `const` no-overlap assertion
(`DEV_WINDOW_NO_OVERLAP`) and two host tests. The predecessor test compared base
addresses only, which is why a 64 KiB device declared as one page passed it for
years.

### 3.2 `TPIDRRO_EL0` — KVM's poison reached `current_tid()`

First actual failure, right after device probing:

```
[FATAL] TPIDRRO_EL0 CORRUPT: tid=0x1de7ec7edbadc0de >= MAX_THREADS (256)
System halted - cannot determine current thread
```

`0x1de7ec7edbadc0de` ("I detected bad code") is the poison arm64 KVM's
`reset_unknown()` stamps into system registers whose reset value the
architecture leaves **UNKNOWN**. `TPIDRRO_EL0` is one of those.

`akuma_primitives::preempt::current_tid` reads it and halts the core if it is out
of range — correctly, since every per-slot static is indexed by it. Until
`threading` installs a real tid, that read has to see 0, and on QEMU it did,
because QEMU zeroes the register. Akuma had never said so out loud.

Fixed with `msr tpidrro_el0, xzr` at both entry points — `src/boot.rs` for the
BSP and `secondary_entry_shared` in `src/smp_shared.rs` for PSCI-woken
secondaries, which each get their own freshly-reset register.

### 3.3 fw_cfg — reading a device that isn't there

```
[Exception] Sync from EL1: EC=0x25, ISS=0x47
  ELR=0x8041502c, FAR=0x8000012008
```

`EC=0x25` is a data abort at the same EL; `ISS=0x47` decodes to a level-3
translation fault. `FAR = 0x80_0001_2008` is `DEV_FW_CFG_VA + 0x08` — the fw_cfg
selector register, reached from `ramfb::init`.

Firecracker has no `fw_cfg` device, so `platform::machine::FW_CFG_PA` is `None`
and nothing is mapped at that VA. On QEMU an absent file yields a clean "not
found"; here *touching the register at all* faults.

Fixed with a compile-time `AVAILABLE` gate on both public entry points in
`src/fw_cfg.rs`, so callers get the same "not found" answer they would get from a
machine whose fw_cfg simply lacks the file. `ramfb` now declines gracefully:
`[ramfb] Not available: ramfb fw_cfg entry not found`.

### 3.4 An inverted kernel-text range flooded `[IRQ POISON]`

Every timer tick printed:

```
[IRQ POISON] eret elr=0x8046cbc4 spsr=0x20000345 switched=0 tid=0 core=0
```

The tripwire:

```rust
let kernel_text = (crate::config::KERNEL_PHYS_BASE as u64..0x6000_0000).contains(&elr);
```

`0x6000_0000` is ~511 MB above QEMU's `0x4010_0000`. With
`KERNEL_PHYS_BASE = 0x8030_0000` the range is `0x8030_0000..0x6000_0000` —
**start greater than end, so permanently empty.** `kernel_text` was always
false, and for an EL1-target frame the predicate is `!kernel_text`, so every
legitimate frame was reported as corrupt.

Five sites in `src/exceptions.rs` shared the literal.

### 3.5 The same range again, plus a fourth copy of the kernel base

With 3.4 fixed, the sibling tripwire took over — `[SGI-S POISON]` from
`akuma-exec`'s scheduler, on every context switch (`old_tid=0 new_tid=1` and
back, so the scheduler was working; only the check was wrong):

```rust
let kernel_text = (0x4010_0000..0x6000_0000).contains(&elr);
```

Alongside it, `crates/akuma-exec/src/threading/mod.rs:8`:

```rust
// Must match KERNEL_PHYS_BASE in src/config.rs and KERNEL_PHYS_BASE in linker.ld.
const KERNEL_PHYS_BASE: usize = 0x4010_0000;
```

A **fourth** mirrored copy of the kernel load address — and dead, never
referenced, hidden by the module's `#![allow(dead_code)]`. Deleted.

Both 3.4 and 3.5 are now one runtime window, `akuma_exec::mmu::is_kernel_text`,
installed once from `main.rs` via `set_kernel_text_window`. It is two relaxed
atomic loads on the IRQ path, which sits right next to two `read_volatile`s of
the trap frame, so the cost is in the noise — and it cannot invert.

## 4. What the port actually needed

Smaller than the survey estimated, because the structural decision paid off.
`src/platform.rs` is the only file describing either machine.

| Change | Where |
|---|---|
| Machine descriptions (both) | `src/platform.rs` (new) |
| Device VAs become spans; GICD gets 64 KiB | `crates/akuma-primitives/src/addr.rs` |
| `DEV_PAGES` const → runtime `DevRegion` map | `crates/akuma-exec/src/mmu/mod.rs` |
| Boot asm maps **only** the UART | `src/boot.rs` |
| L1[1] Normal vs Device, per machine | `src/boot.rs` (assembler `.if`) |
| virtio stride/count become runtime | `akuma-primitives`, `akuma-virtio` |
| virtio INTID base 48 → 32 | `src/main.rs` |
| Kernel base via linker `--defsym` | `build.rs`, `linker.ld`, `src/config.rs` |
| `platform-firecracker` feature | `Cargo.toml` |

The boot assembly maps one page — the console — because that is the only device
whose address *can* be a compile-time literal. Everything else is installed from
Rust by `mmu::rebuild_boot_device_table` before the first GIC or virtio access.
That structure is what made the Firecracker arm mostly a table of constants.

**No regression on QEMU**: `cargo run --release` still boots to 289 PASSED /
0 FAILED with `[Platform] qemu-virt device map installed`.

## 5. Open

- **ext2 mount does not complete.** Reaches `[FS] Initializing filesystem...` and
  the file-page cache init, then stops. Untriaged. Note the test used a
  *read-only* drive (the Lima virtiofs mount of the host is `ro`), which ext2
  mount may not tolerate — that is the first thing to rule out.
- **Firecracker rejects Akuma's virtio handshake order.** Harmless so far — the
  device works and the capacity is read correctly — but Firecracker logs
  `invalid virtio driver status transition: 0x0 -> 0x3` and `ack virtio features
  in invalid state 0x0`. Akuma writes the status register in an order
  Firecracker's stricter model does not accept. Worth fixing before trusting
  virtio-net.
- **`SMP=N > 1` untested and expected broken.** The FDT-derived device map is
  *not* implemented; the compile-time bootstrap map assumes one vCPU, and
  Firecracker's redistributor base is `0x3FFF_0000 - vcpu_count * 0x2_0000`.
  This is the single largest remaining piece.
- **No networking tested.** Needs a tap device and a `network-interfaces` entry.
- **`src/tests.rs` map assertions.** ~20 sites treat `0x4000_0000..0x8000_0000`
  as kernel RAM. Under Firecracker that is the MMIO window. Some fail loudly;
  the dangerous ones **pass vacuously**. This is why the Firecracker run reports
  only 1 `PASSED` line against QEMU's 289 — most of the suite did not run
  (no filesystem), so the two numbers are not yet comparable.

## 6. The lesson worth keeping

Four of the five bugs were a constant that had only ever been checked against one
machine, and the fifth was a mirrored copy of such a constant. None were logic
errors. The device-map abstraction that came out of this — **fixed VAs, discovered
PAs** — is the shape that prevents the class, and the `const`
`DEV_WINDOW_NO_OVERLAP` assertion is what makes the layout self-checking rather
than self-documenting.

The remaining known instance of the same class is the vCPU-dependent
redistributor base (§5). It is currently a literal that is correct for exactly one
configuration, which is precisely the shape of the four bugs above.

---

## Background

- `proposals/FIRECRACKER_PORT.md` — verified constants and the port design.
- `docs/archive/GICD_IROUTER_ALIASING.md` — §3.1 in full.
- `docs/runbooks/run-on-firecracker.md` — how to reproduce all of this.
- `docs/archive/PORTING_POSSIBILITIES.md` — the original options survey.
