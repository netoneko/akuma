# Firecracker platform reference

**Stability: C** — active risk, expect surprises. Akuma boots, mounts its ext2
root, runs the boot suite and executes userspace processes. `vcpu_count > 1`
works as of 2026-08-21 — the device map is read from the FDT (§3.3); networking
is unverified (§4).

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
| `GICR_PA` | `0x080A_0000` | `0x3FFD_0000` **(bootstrap only; real value from the FDT — §3.3)** |
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

### 3.3 The redistributor base comes from the FDT, not from a constant

Firecracker computes the GIC redistributor base as:

```
GICR_base = 0x3FFF_0000 - vcpu_count * 0x2_0000
```

so CPU0's frames move with the configured vCPU count:

| vCPUs | CPU0 RD_base | CPU0 SGI_base | measured |
|---|---|---|---|
| 1 | `0x3FFD_0000` | `0x3FFE_0000` | ✓ |
| 2 | `0x3FFB_0000` | `0x3FFC_0000` | ✓ |
| 4 | `0x3FF7_0000` | `0x3FF8_0000` | ✓ |

`platform::firecracker::GICR_PA` is the 1-vCPU value and is **a bootstrap
constant only** — enough to print and survive until the FDT is read. Using it
with more vCPUs points CPU0 at *another core's* redistributor: `gic_v3::init`
clears the wrong `GICR_WAKER` (a per-PE register, so CPU0's stays asleep) and
`enable_irq(27)` enables the virtual timer on the wrong frame — **the boot core
silently loses its timer interrupt**, with no build or boot error.

`platform::install_fdt_device_map` (called from `kernel_main`, after
`mmu::ensure_boot_identity_covers` maps the blob and **before** `gic::init`)
replaces the bootstrap map with one derived from the device tree via
`crates/akuma-firecracker`. It **reads** the redistributor from `intc`'s second
`reg` entry rather than computing it from `vcpu_count` — `cpu_count * 0x2_0000`
happens to equal the redistributor span, so a derivation would pass every test
here and still be an address inferred from an unrelated property.

The boot log states which map the GIC was configured from, and it is the first
thing to check when a multi-vCPU boot misbehaves:

```
[Platform] firecracker device map installed            <- bootstrap
[Platform] FDT device map: GICR=0x3ffb0000 (moved from bootstrap literal)
[SMP-shared] probed 2 core(s)
```

A failure to parse is not fatal — the bootstrap map is retained and the log says
so (`no FDT` / `FDT rejected`). That is correct at `vcpu_count: 1` and on QEMU
virt at every `SMP=N`, and wrong for multi-vCPU Firecracker, so
`GICR=0x3ffd0000` alongside `probed 2 core(s)` means the parse fell back and the
boot core is about to lose its tick.

Measured 2026-08-21, Lima nested virt (`vz`, 4 CPUs), `--no-disk --no-net`: 1, 2
and 4 vCPUs all report the predicted `GICR`, bring up every secondary
(`✓ 3 secondary core(s) online` at 4), and keep the timer
(`[Timer] host WFI probe: tick = 1000 us`). The same change re-derives QEMU
virt's `0x080A_0000` from its own tree, where the boot suite stays at 298/0 under
`SMP=4` — that boot is what proves the parse against a machine whose answers are
independently known.

### 3.4 No `fw_cfg`, no RTC, no ramfb — and since 2026-08-21, none compiled in

Superseded in part: the drivers are no longer merely guarded, they are absent.
`kernel_framebuffer` and `kernel_audio` (build.rs) keep `src/ramfb.rs`,
`src/fw_cfg.rs` and the virtio-sound driver out of a `platform-firecracker`
image — verified by symbol count, 0 here against 14 in the QEMU build. The
runtime guard described below still exists and still matters for any build that
does compile them. `test_platform_device_gates` asserts the general rule: a
driver may only be compiled in if the machine has the device.



`platform::machine::FW_CFG_PA` is `None`, and `src/fw_cfg.rs` gates both public
entry points on it. Touching an unmapped device VA is an EL1 translation fault,
not a read of zeroes, so the gate is required rather than defensive. `ramfb`
declines cleanly as a result.

Firecracker does have a PL031 RTC at `0x4000_1000`, but Akuma does not map it;
the boot log's `Warning: RTC not available` is expected.

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

### 3.6 The virtio status handshake must be stepped

Firecracker validates every write to the virtio MMIO status register against an
exact-match transition table; QEMU just ORs the bits. `virtio-drivers` writes
`ACKNOWLEDGE|DRIVER` in one store, which Firecracker rejects, leaving the device
at `INIT` forever — no queues, no I/O, and a failure that presents as an ext2
hang because config reads still work.

All drivers therefore go through **`akuma_virtio::VirtioTransport`**
(`crates/akuma-virtio/src/transport.rs`), which steps the status register one
milestone at a time. Never construct a bare `MmioTransport` for a device driver;
read that module's header first.

### 3.7 The `entropy` device is not optional

Firecracker attaches no virtio-rng unless the config says `"entropy": {}`. Without
it Akuma prints `[RNG] Hardware RNG not available` and three boot-suite tests
fail — `rng entropy-live`, plus `getrandom` returning `EIO`, which also fails
`syscall_bkl_optout`. QEMU's runner always provides one, so this is easy to
mistake for a kernel regression.

### 3.8 A print is a lock acquisition

`console::emit` runs inside `with_irqs_disabled` and, under `kernel_console_lock`,
acquires a `Spinlock`. So **never print from a section that disables preemption**:
the print spins on a lock whose holder may be a thread that cannot be scheduled,
and on a single-vCPU guest nothing can drain it. The kernel wedges with no output.

The concrete instance: `akuma_net::smoltcp_net::poll` holds `NETWORK` with
`PreemptGuard` active. Anything it needs to report is recorded (see `DhcpReport`)
and emitted after `drop(guard)`; RX-path observability is atomic counters
(`rx_counters()`), not prints.

Consequences to respect:

- **`akuma-net` prints with `safe_print!`, never `log::`.** Its `log` dependency
  exists for **smoltcp**, built with a compile-time max-level so smoltcp's
  per-packet tracing is elided. Routing kernel messages through that facade either
  resurrects the tracing or loses the messages with it. `src/klog.rs` installs a
  heap-free sink so third-party crates (virtio-drivers) still report at info+.
- **The console lock is compiled in, and *acquiring* it is a runtime decision.**
  Both halves of that are correctness. Acquiring it on a single-vCPU guest can
  deadlock, for the reason above: nothing can drain a lock whose holder cannot be
  scheduled. Not acquiring it on a multi-vCPU guest byte-interleaves at the shared
  PL011 register (`docs/archive/UART_SMP_INTERLEAVE_FIX.md`). Neither condition is
  knowable at build time now that `vcpu_count` is free (§3.3), so `build.rs`
  compiles the lock into every non-size profile and `console::set_multicore`
  decides whether to take it — flipped by `smp_shared::bringup_secondaries`
  **before** the first `PSCI CPU_ON`.

  That last detail is load-bearing. Flipping it from the secondary's own entry
  path instead leaves the BSP printing unlocked while the secondary starts, which
  is reliably enough to corrupt one line:

  ```
  [SMP-sh[aSrMePd-]s hCPaUr_eOdN]  ccoorree  11  (omnpliidnre= 0(x1i)d l-e>  toikd
  ```

  `platform-firecracker` was previously excluded from the lock altogether, which
  was right while the target was single-vCPU-only and is wrong now.
  `CONSOLE_LOCK=1`/`=0` still force the compile-time half either way.

### 3.9 Address-range checks must not be machine-relative literals

`akuma_exec::mmu::is_kernel_text` is the single place that answers "is this
address kernel code", backed by a window installed from `main.rs`. Five sites in
`src/exceptions.rs` and one in `akuma-exec`'s scheduler previously used
`KERNEL_PHYS_BASE..0x6000_0000`, which **inverts into an empty range** when the
kernel loads above `0x6000_0000` — producing `[IRQ POISON]` on every timer tick
and `[SGI-S POISON]` on every context switch.

Never write a literal upper bound for a kernel-address test.

## 4. Known broken

- ~~**`vcpu_count > 1` cannot address its GIC**~~ — fixed 2026-08-21 by reading
  the device map from the FDT (§3.3). Verified at 1/2/4 vCPUs under Lima nested
  virt with `--no-disk --no-net`: predicted `GICR` each time, every secondary
  online, timer alive, console serialized.

- **Multi-vCPU under boot-suite load storms the BKL.** This is what replaced
  §3.3 as the blocker, and it is a contention bug, not an addressing one — the
  GIC is now configured correctly and the cores really do run.

  Measured with a disk attached, `--no-net`, Lima nested virt:

  | vCPUs | result | `[BKL] stuck` lines |
  |---|---|---|
  | 1 | 284 PASSED / 0 FAILED, suite completes | 0 |
  | 2 | 274 PASSED, suite **does not complete** | 635 |

  At 2 vCPUs the log ends in an unbroken run of
  `[BKL] stuck: owner=1 waiter=2 tag=511 (aff0+1)` and the run hits its timeout
  with the late boot-suite tests (including `gicr-device-map` itself) never
  reached. The first such line appears early — during the realloc test — and the
  suite keeps making progress for thousands of lines before the storm becomes
  terminal, so the symptom is degradation, not an immediate hang.

  **Not Firecracker-specific, but much worse here.** The same `SMP=4` QEMU boot
  emits 109 of those lines and still finishes 299/0. So this is the known
  load-driven `tag=511` class rather than anything new about this platform; KVM
  simply drives it harder. Treat a fix as an SMP problem and reproduce it on
  QEMU first, where the boot is cheap.

  The single-vCPU path is unaffected (0 stuck lines) and remains the one to use
  for anything that is not specifically SMP work.

- **vCPU counts above 4 are untried.** The measured FDT sweep goes to 8
  (`fdt/`), the boot does not. `MAX_CORES` in `src/smp_shared.rs` is the other
  ceiling to check first. Lima's own vCPU count caps what can be tested here.

- **SMP networking is untested**, and blocked behind the RX item below anyway.
- **Inbound (RX): root-caused and fixed 2026-08-21, not yet verified on a boot.**
  Firecracker's virtio-net will not read a frame off the host tap until the
  *total* capacity of the posted receive descriptors reaches `MAX_BUFFER_SIZE` =
  65562 bytes (`read_from_mmds_or_tap`); a single 2 KB buffer never opened that
  gate, so every inbound frame was dropped into `no_rx_avail_buffer` with no
  guest-visible error. `RX_BUFFER_LEN` is now 65568. `extreme-size` keeps 2 KB on
  purpose and therefore has no inbound networking here. Historical detail below.
- ~~**Inbound (RX) never reaches the guest.**~~ Outbound works and is well-formed on
  the wire; dnsmasq answers `DHCPOFFER`. But no `DHCPREQUEST` follows, host ARP goes
  unanswered, and tap0 shows every host→guest frame dropped — while
  `rx_counters()` confirms a receive buffer *is* posted. So descriptors are
  available and the device still does not fill them. `RING_EVENT_IDX` is ruled out.
  See `docs/archive/AKUMA_FIRECRACKER_KVM.md` §5.1. This is the only thing between
  here and an SSH session: ext2 mounts, the boot suite passes 290/0/0, userspace
  runs, and herd starts `/bin/sshd`.
- **`virtio-drivers` must stay at 0.13+.** 0.7.5 sizes the virtio-net header by
  `MRG_RXBUF` (10 bytes) rather than `VERSION_1` (12), which shifts every frame two
  bytes left under Firecracker. QEMU tolerated it; Firecracker does not.
- **Networking untested.** `VIRTIO_INTID_BASE = 32` is wired but unexercised;
  it depends on the same virtio handshake as block.
- **`src/tests.rs` bakes in QEMU's map.** ~20 sites treat
  `0x4000_0000..0x8000_0000` as kernel RAM; under Firecracker that is the MMIO
  window. Some fail loudly, others pass vacuously. The Firecracker boot therefore
  reports far fewer `PASSED` lines than QEMU's 289, and the two counts are not
  comparable.
