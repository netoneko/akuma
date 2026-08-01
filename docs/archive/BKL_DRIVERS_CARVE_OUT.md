# BKL Drivers Carve-Out (Phase 6)

**Status**: Landed 2026-08-01. Default-on in `smp-shared` since 2026-08-01.
**Feature**: `no-bkl-drivers` → `cfg(kernel_no_bkl_drivers)`
**Guard**: `DriverBklGuard` (`src/syscall/fs.rs`)
**Runtime toggle**: `smp_shared::drivers_bkl_drop_enabled()` / `set_drivers_bkl_drop_enabled()`

---

## 1. Audit

Phase 6 of `BKL_FINE_GRAINED_LOCKING_PLAN.md` calls for auditing device drivers
for BKL dependence, adding per-driver locks where needed, making IRQ handlers
BKL-free, and verifying with tests. The audit found that most of this work was
already done by the preceding phases, and that the plan's IRQ-handler goal
belongs to Phase 7 (scheduler), not Phase 6.

### Device inventory

| Driver | File | Lock | Already BKL-free via |
|--------|------|------|---------------------|
| virtio-blk | `src/block.rs` | `BLOCK_DEVICE: Spinlock` | `no-bkl-vfs` (Phase 4) |
| virtio-net | `crates/akuma-net/src/smoltcp_net.rs` | `NETWORK: Spinlock` | `no-bkl-network` (Phase 2) |
| virtio-rng | `src/rng.rs` | `RNG_DEVICE: Spinlock` | **this phase** |
| virtio-sound | `src/audio.rs` | `SOUND_DEVICE: Spinlock` | **this phase** |
| ramfb (framebuffer) | `src/ramfb.rs` | `FB_STATE: Spinlock` | **this phase** |
| PL011 UART | `src/console.rs` | (raw MMIO, no lock) | always BKL-free |
| Timer/GIC | `src/timer.rs`, `src/gic*.rs` | per-core sysregs + `RTC`/`UTC_OFFSET_US` Spinlocks | n/a (see §2) |
| virtio-gpu | — | — | **does not exist** (see §3) |

Every virtio device is **polling-based**: `read_blocks`/`write_blocks` (blk),
`read_bytes` (rng), `pcm_xfer` (sound) busy-wait on the used ring. No virtio
IRQ handler is registered anywhere in the kernel. The only registered device
IRQ handler is the timer (`timer_irq_handler`, IRQ 27, `src/main.rs:945`).

### Uncovered syscall paths (this phase's targets)

Before this phase, the following device-driver syscalls held the BKL for their
entire duration despite their driver state having its own Spinlock:

| Syscall | File:Line | Device | Inner lock |
|---------|-----------|--------|------------|
| `sys_getrandom` | `proc.rs:1183` | virtio-rng | `RNG_DEVICE` |
| `sys_read` → `DevUrandom` | `fs.rs:600` | virtio-rng | `RNG_DEVICE` |
| `sys_pread64` → `DevUrandom` | `fs.rs:740` | virtio-rng | `RNG_DEVICE` |
| `sys_write` → `DevDsp` | `fs.rs:984` | virtio-sound | `SOUND_DEVICE` |
| `sys_fb_init` | `fb.rs:6` | ramfb | `FB_STATE` |
| `sys_fb_draw` | `fb.rs:17` | ramfb | `FB_STATE` |
| `sys_fb_info` | `fb.rs:48` | ramfb | `FB_STATE` |

All are reached through the single `enter_kernel()` at `rust_sync_el0_handler`
(`exceptions.rs:2220`). None used `VfsBklGuard`, `NetBklGuard`, or any other
BKL-drop guard.

### Already covered (no action needed)

- **virtio-blk** (`BLOCK_DEVICE`): every disk read/write goes through ext2's
  `read_state`/`write_state`, which the VFS carve-out already wraps in
  `VfsBklGuard`. The `BLOCK_DEVICE` Spinlock is the inner lock credited in the
  VFS syscall→lock map (`locking.md`).
- **virtio-net** (`NETWORK`): the `no-bkl-network` carve-out already drops the
  BKL for all smoltcp syscalls.

---

## 2. IRQ handlers

The plan's Phase 6 calls for "BKL-free IRQ handlers." The audit found that this
goal is already partially met and that the remaining work is Phase 7 territory:

- **Scheduler SGI preempting EL0** is already BKL-free (M5c,
  `exceptions.rs:1562-1576`): the fast path runs the switch without
  `enter_kernel()`, made atomic by `POOL` alone.
- **All device IRQs** (only the timer, IRQ 27) run holding the BKL:
  `rust_irq_handler_with_sp` calls `enter_kernel()` at line 1579 before
  `dispatch_irq`. The timer handler touches scheduler-adjacent state (alarm
  queue via `kernel_timer::on_timer_interrupt`, preemption watchdog, scheduler
  SGI trigger) — making it BKL-free is a scheduler change, not a device-driver
  change, and belongs to Phase 7.

Since no virtio device uses interrupt-driven I/O (all are polling-based), there
are no virtio IRQ handlers to convert. The only IRQ handler is the timer, which
is scheduler-coupled.

---

## 3. virtio-gpu

**virtio-gpu does not exist in this codebase.** Zero matches for `gpu`/`GPU`/
`virtio_gpu`/`VirtioGpu` across all `.rs` files. Graphics output is via QEMU
`ramfb` (`src/ramfb.rs`) — a fw_cfg-backed RAM framebuffer, not a virtio device.
There is nothing to gate off.

---

## 4. Implementation

Following the `no-bkl-mm` pattern exactly (Phase 5, the most recent carve-out):

### Guard

`DriverBklGuard` (`src/syscall/fs.rs`) mirrors `MmBklGuard`:
- Runtime toggle `drivers_bkl_drop_enabled()` (default on), latched at
  construction (same discipline as `VfsBklGuard`/`MmBklGuard` — `drop()` never
  re-reads the toggle).
- Uses the same `bkl::dropped_window_open()`/`close()` ledger as all other
  guards.
- Zero-cost no-op unless both `kernel_smp_shared` and `kernel_no_bkl_drivers`
  are set.

### Feature wiring

- `build.rs`: emits `cfg(kernel_no_bkl_drivers)` from
  `CARGO_FEATURE_NO_BKL_DRIVERS`.
- `Cargo.toml`: `no-bkl-drivers = []` (bin-crate-only, like `no-bkl-mm` —
  none of the driver Spinlocks need to know the feature exists). Added to the
  `smp-shared` feature set.
- `src/smp_shared.rs`: `DRIVERS_BKL_DROP_ENABLED: AtomicBool` (default on),
  `drivers_bkl_drop_enabled()` / `set_drivers_bkl_drop_enabled()`.

### Guard placement

Per the playbook ("scope the window as narrowly as possible"), the guard is
constructed after early-error/argument-validation returns:

- `sys_getrandom` (`proc.rs`): after `validate_user_ptr` check; wraps the
  entire chunked-read loop.
- `sys_read`/`sys_pread64` `DevUrandom` arm (`fs.rs`): after the multikernel
  secondary-forward arm (which is `#[cfg(kernel_smp)]` — compiled out in
  `smp-shared`, and must stay BKL-held regardless); wraps the `fill_bytes` +
  `copy_to_user_safe` path.
- `sys_write` `DevDsp` arm (`fs.rs`): wraps the `audio::play` call.
- `sys_fb_init`/`sys_fb_draw`/`sys_fb_info` (`fb.rs`): after dimension/pointer
  validation; wraps the `ramfb` device call.

Cross-core forwarding arms (`#[cfg(kernel_smp)]`) stay outside the guard — they
marshal through the BKL-protected bounce and must keep the lock.

---

## 5. A/B measurement

A same-binary `bkl-profile` A/B was run at SMP=4 on the standing regimen
(`net4` → `read4` → `cp2` → `rm`), toggling the default in source between the
two builds (feature set byte-identical: `devbox-smoltcp,no-tests,bkl-profile`).

| | ON (carve-out active) | OFF (BKL-held) |
|---|---|---|
| total contended spins | 40,560,909 | 44,555,503 |
| `getrandom` (tag 278) | not attributed | not attributed |
| 6/6 digests | exact | exact |
| PANIC / WILD / stale | 0 / 0 / 0 | 0 / 0 / 0 |

`getrandom` does not appear in either side's attribution — it is below the noise
floor on both sides. The TLS handshakes in `curl`'s mbedTLS call `getrandom`
(~256 bytes per handshake), but QEMU's virtio-rng services that in microseconds
with a tight polling loop under the `RNG_DEVICE` Spinlock, generating no
measurable BKL contention. The ~9% total-spin difference is within boot-to-boot
variance (SSH handshake timing, connection scheduling), not a signal from this
carve-out.

This confirms the expectation: like `no-bkl-mm` (Phase 5), this phase is
plan-driven, not evidence-driven. No device-driver syscall has ever been named by
a `bkl-profile` attribution run as a significant BKL holder. The value is
completeness (eliminating the last class of BKL-held device I/O) and
future-proofing (if virtio-rng or virtio-sound ever becomes contended, the guard
is already in place).

---

## 6. Verification

- **Boot self-test** (`test_drivers_bkl_drop` in `process_tests.rs`): drives
  the real syscall entry points (`handle_syscall`) for `getrandom`,
  `fb_init`/`fb_draw`/`fb_info`, checking `bkl::in_dropped_window()` is false
  after each return (ledger balanced). Covers early-error paths (must not open
  the window), a real guarded path (`fb_init` with valid dims), and the runtime
  kill switch.
- **Build matrix**: default (single-core, no-op), `smp-shared` (guard active),
  `devbox-smoltcp` (deployment target with `no-tests`) — all compile clean.
- **Host unit tests**: 445+ tests pass across all extracted crates.

---

## Background

- [`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) —
  Phase 6 § (this plan).
- [`locking.md`](../reference/subsystems/locking.md) — the carve-out playbook
  and syscall→lock map (updated with the `no-bkl-drivers` section).
- [`BKL_MM_CARVE_OUT.md`](BKL_MM_CARVE_OUT.md) — Phase 5, the template this
  phase follows most closely.
