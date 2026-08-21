# Firecracker platform reference

**Stability: C** — active risk, expect surprises. Akuma boots, mounts its ext2
root, runs the boot suite and executes userspace processes. `vcpu_count > 1` is
known broken (§3.3); networking is unverified (§4).

Current-state facts only. History and the debugging narrative are in
`docs/archive/AKUMA_FIRECRACKER_KVM.md`; the design argument is in
`proposals/FIRECRACKER_PORT.md`; the procedure is
`docs/runbooks/run-on-firecracker.md`.

Constants below were read from **Firecracker v1.16.1** and verified identical on
`main` as of 2026-08-21. They have moved between Firecracker releases before —
re-read `src/vmm/src/arch/aarch64/layout.rs` and `gic/gicv3/mod.rs` if the pinned
version changes.

- [`memory-map.md`](memory-map.md) — the full guest physical layout
- [`disk-and-volumes.md`](disk-and-volumes.md) — how drives are presented, and
  the AWS EBS/instance-store mapping

## 1. Selecting the target

`platform-firecracker` is a cargo feature, not a profile:

```bash
cargo build --release --features platform-firecracker
rust-objcopy -O binary target/aarch64-unknown-none/release/akuma akuma-fc.bin
```

or `overlays/devbox-firecracker/build.sh`, which also asserts the load address.

The feature selects `platform::firecracker` in `src/platform.rs` and makes
`build.rs` emit `--defsym=KERNEL_PHYS_BASE_OVERRIDE=0x80300000` so `linker.ld`
links there.

## 2. Where each machine is described

One file: **`src/platform.rs`**. It holds a `machine` module per target with the
same constant names, selected by feature. Nothing else in the tree should need a
`#[cfg]` for a platform.

| Constant | QEMU virt | Firecracker |
|---|---|---|
| `RAM_BASE` | `0x4000_0000` | `0x8000_0000` |
| `KERNEL_PHYS_BASE` (`src/config.rs`) | `0x4010_0000` | `0x8030_0000` |
| `GICD_PA` | `0x0800_0000` | `0x3FFF_0000` |
| `GICR_PA` | `0x080A_0000` | `0x3FFD_0000` **(1 vCPU only — §3.3)** |
| `UART_PA` | `0x0900_0000` | `0x4000_2000` |
| `FW_CFG_PA` | `Some(0x0902_0000)` | `None` |
| `VIRTIO_PA` | `0x0A00_0000` | `0x4000_3000` |
| `VIRTIO_STRIDE` | `0x200` | `0x1000` |
| `VIRTIO_INTID_BASE` | `48` | `32` |
| `MMIO_WINDOW_IS_DEVICE` | `false` | `true` |

## 3. Invariants

### 3.1 Fixed VAs, discovered PAs

The kernel's device *virtual* addresses are compile-time constants in
`akuma_primitives::addr` and are identical on every machine. The *physical* side
is a runtime table (`akuma_exec::mmu::DevRegion`), installed via
`set_device_map`. This asymmetry is the core of the abstraction: only the PA side
is machine-specific, and only the PA side can require discovery.

Device VAs are **spans**, not pages, and `DEV_WINDOW_NO_OVERLAP` is a `const`
assertion that no two overlap. Do not add a device to
`akuma_primitives::addr` without adding its span to `DEV_WINDOW_SPANS` — the
assertion is what makes the layout self-checking.

### 3.2 Boot assembly maps only the console

`src/boot.rs` maps one device page: the UART. That is the only device whose
address can be a compile-time literal, because the assembly runs before any FDT
can be parsed. Everything else is installed from Rust by
`mmu::rebuild_boot_device_table`, called from `kernel_main` **before**
`gic::init()`. Anything that touches GIC or virtio MMIO earlier than that will
fault.

### 3.3 `vcpu_count` must be 1

Firecracker computes the GIC redistributor base as:

```
GICR_base = 0x3FFF_0000 - vcpu_count * 0x2_0000
```

so CPU0's frames move with the configured vCPU count:

| vCPUs | CPU0 RD_base | CPU0 SGI_base |
|---|---|---|
| 1 | `0x3FFD_0000` | `0x3FFE_0000` |
| 2 | `0x3FFB_0000` | `0x3FFC_0000` |
| 4 | `0x3FF7_0000` | `0x3FF8_0000` |

`platform::firecracker::GICR_PA` is the 1-vCPU value. With more vCPUs, CPU0
drives *another core's* redistributor: `gic_v3::init` clears the wrong
`GICR_WAKER` (a per-PE register, so CPU0's stays asleep) and `enable_irq(27)`
enables the virtual timer on the wrong frame — **the boot core silently loses its
timer interrupt.** No build or boot error.

The fix is to derive the device map from the FDT, which Firecracker populates
correctly. **Not implemented.** This is the largest outstanding piece.

### 3.4 No `fw_cfg`, no RTC, no ramfb

`platform::machine::FW_CFG_PA` is `None`, and `src/fw_cfg.rs` gates both public
entry points on it. Touching an unmapped device VA is an EL1 translation fault,
not a read of zeroes, so the gate is required rather than defensive. `ramfb`
declines cleanly as a result.

Firecracker does have a PL031 RTC at `0x4000_1000`, but Akuma does not map it;
the boot log's `Warning: RTC not available` is expected.

### 3.5a The virtio status handshake must be stepped

Firecracker validates every write to the virtio MMIO status register against an
exact-match transition table; QEMU just ORs the bits. `virtio-drivers` writes
`ACKNOWLEDGE|DRIVER` in one store, which Firecracker rejects, leaving the device
at `INIT` forever — no queues, no I/O, and a failure that presents as an ext2
hang because config reads still work.

All drivers therefore go through **`akuma_virtio::VirtioTransport`**
(`crates/akuma-virtio/src/transport.rs`), which steps the status register one
milestone at a time. Never construct a bare `MmioTransport` for a device driver;
read that module's header first.

### 3.5b The `entropy` device is not optional

Firecracker attaches no virtio-rng unless the config says `"entropy": {}`. Without
it Akuma prints `[RNG] Hardware RNG not available` and three boot-suite tests
fail — `rng entropy-live`, plus `getrandom` returning `EIO`, which also fails
`syscall_bkl_optout`. QEMU's runner always provides one, so this is easy to
mistake for a kernel regression.

### 3.5 Registers with UNKNOWN reset values must be initialised

KVM stamps `0x1de7ec7edbadc0de` into system registers whose architectural reset
value is UNKNOWN, specifically to catch guests that depend on one. QEMU zeroes
them instead. Any register Akuma reads before writing must be explicitly
initialised in `src/boot.rs` (BSP) **and** `secondary_entry_shared` in
`src/smp_shared.rs` (each PSCI-woken core resets its own copy).

Currently required:

- **`TPIDRRO_EL0`** — `preempt::current_tid` reads it and treats an out-of-range
  value as fatal. KVM's poison tripped it immediately.
- **`SCTLR_EL1.SA`/`.SA0`** — `boot.rs` ORs its bits into the reset value and
  clears nothing, so KVM's `SA0=1` was inherited and enabled EL0 SP-alignment
  checking, which this kernel's userspace ABI does not satisfy. Every `/bin/*`
  binary took `EC=0x26` at its entry point. Measured: QEMU `0x3490d185`
  (SA=0, SA0=0) versus KVM `0x34c5d1dd` (SA=1, SA0=1). Both are now forced off,
  in `boot.rs` **and** `secondary_entry_shared`.

Do not reconstruct `SCTLR_EL1` wholesale to avoid this — the reset value carries
the architecturally RES1 fields. Force only the bits Akuma has an opinion about.

Likewise, do not rely on an MMIO register's reset value. `GICD_IROUTER` was
relied on for years — see `docs/archive/GICD_IROUTER_ALIASING.md`.

### 3.6 Address-range checks must not be machine-relative literals

`akuma_exec::mmu::is_kernel_text` is the single place that answers "is this
address kernel code", backed by a window installed from `main.rs`. Five sites in
`src/exceptions.rs` and one in `akuma-exec`'s scheduler previously used
`KERNEL_PHYS_BASE..0x6000_0000`, which **inverts into an empty range** when the
kernel loads above `0x6000_0000` — producing `[IRQ POISON]` on every timer tick
and `[SGI-S POISON]` on every context switch.

Never write a literal upper bound for a kernel-address test.

## 4. Known broken

- **`vcpu_count > 1`** — §3.3. The largest outstanding piece.
- **Networking unverified.** `VIRTIO_INTID_BASE = 32` is wired and the DHCP/tap
  host side is scripted, but no lease has been observed yet.
- **Networking untested.** `VIRTIO_INTID_BASE = 32` is wired but unexercised;
  it depends on the same virtio handshake as block.
- **`src/tests.rs` bakes in QEMU's map.** ~20 sites treat
  `0x4000_0000..0x8000_0000` as kernel RAM; under Firecracker that is the MMIO
  window. Some fail loudly, others pass vacuously. The Firecracker boot therefore
  reports far fewer `PASSED` lines than QEMU's 289, and the two counts are not
  comparable.
