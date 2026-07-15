# QEMU HVF — Booting Akuma under Apple Hypervisor.framework (RESOLVED 2026-06-09)

Akuma now boots and runs userspace under `-accel hvf` on Apple Silicon, and
`scripts/cargo_runner.sh` **uses HVF by default** there (auto-detected; falls back
to TCG on other hosts, and `HVF=0` forces TCG). Inference is ~70–120× faster than
TCG (see "Result" below).

The historical blocker was an assertion in QEMU's HVF backend:

```
Assertion failed: (isv), function hvf_handle_exception, file hvf.c, line 1883.
```

It turned out **not** to be a single QEMU bug, and **not** the NEON `STP`/`LDP`
save/restore in `src/exceptions.rs` (an earlier version of this doc guessed that —
it was wrong; NEON saves go to the stack, which is RAM that HVF faults in
transparently without trapping to QEMU). It was four separate Akuma-side
assumptions that only hold under QEMU TCG software emulation. All four are fixed,
and all three kernel profiles (`release`, `size`, `extreme-size`) boot to SSH and
run userspace under both HVF and TCG.

## Root cause 1 — GICv2 MMIO programming model (the `isv` assertion)

`assert(isv)` in QEMU's HVF `EC_DATAABORT` handler fires when a data abort traps
to the hypervisor with ISV=0 (no decoded syndrome). Apple HVF reports ISV=0 for
accesses to addresses it cannot resolve as plain RAM — including MMIO and faults
to unmapped guest-physical addresses.

Under `-machine virt`, **TCG defaults to GICv2 but HVF presents GICv3**. The
GICv2 driver (`src/gic.rs`) programmed:
- the distributor's `GICD_IPRIORITYR`/`GICD_ITARGETSR` (GICv2 layout), and
- the **GICv2 CPU interface MMIO at `0x0801_0000`**, which does not exist under
  GICv3 (the CPU interface is a system-register interface, `ICC_*_EL1`).

The first distributor write past the v2/v3-divergent region faulted with ISV=0 →
assert. (Empirically the fault hit during the `GICD_IPRIORITYR` loop.)

**Fix:** a proper **GICv3 driver**, `src/gic_v3.rs`, now the default. It uses the
system-register CPU interface (`ICC_SRE/PMR/IGRPEN1/IAR1/EOIR1/SGI1R_EL1`) and the
per-PE **redistributor** (GICR) for SGI/PPI config. The legacy GICv2 MMIO driver
is kept behind the `gic-v2` Cargo feature for reference/fallback (it works under
TCG with `gic-version=2`, never under HVF). The runner passes
`-machine virt,gic-version=3` for both accelerators. Akuma uses only SGI 0 and
PPIs 27/30, so no SPI routing (`GICD_IROUTER`) is needed. Two redistributor frames
(`0x080A_0000` RD_base, `0x080B_0000` SGI_base) were added to the device mapping
in `boot.rs` and `crates/akuma-exec/src/mmu/mod.rs` (`DEV_PAGES`).

## Root cause 2 — physical timer (CNTP) is trapped under HVF

After the GIC, the kernel crashed programming `CNTP_CVAL_EL0` (EC=0x0, undefined).
The **physical** timer/counter belongs to the hypervisor under HVF; an EL1 guest
must use the **virtual** timer (`CNTV_*`, PPI 27). Akuma had split the two: CNTP
(PPI 30) for preemption, CNTV (PPI 27) for the async alarm queue (`kernel_timer`).

**Fix:** unify onto the single virtual timer. `src/timer.rs` now programs
`CNTV_CVAL_EL0`/`CNTV_CTL_EL0` and reads `CNTVCT_EL0` (the physical and virtual
time bases differ under HVF because `CNTVOFF` is nonzero). The 10 ms preemption
tick (PPI 27) owns the hardware and, on each tick, services the async alarm queue;
`kernel_timer::update_hardware_timer` is now a no-op (it would otherwise push the
next tick out to a far-future alarm and freeze preemption). Async timers get
~10 ms resolution, which is fine for the SSH read timeouts and periodic monitors
that use them. This is also more portable: EL1 guests use the virtual timer on
real hardware too.

## Root cause 3 — IC IVAU on the not-yet-mapped user VA

Userspace ELF exec then failed (`busybox` → exit -14): the demand-pager loads a
code page, runs cache maintenance, then maps the page. The maintenance did
`DC CVAU` on the kernel alias (`kva`, correct) but `IC IVAU` on the **user VA**,
which is not mapped until *after* the maintenance — so on real hardware/HVF the
`IC IVAU` translation-faulted (EC=0x25, DFSC=translation fault L3). TCG treats
cache ops as no-ops, so it never showed.

**Fix:** I-cache invalidation to PoU is by physical address, so all four
demand-paging sites in `src/exceptions.rs` now `IC IVAU` via `kva` (the always-
mapped kernel alias of the same frame), matching the `DC CVAU` above them. The
companion self-tests in `src/tests.rs` had the same mistake (cleaning a user VA in
an inactive address space) and were corrected to use `kva`.

## Root cause 4 — post-indexed (writeback) MMIO store on the `extreme` profile

After 1–3, `release` and `size` ran under HVF but the `extreme-size` profile still
hit `(isv)` — during the GICR SGI-frame setup in `gic_v3::init` (the redistributor
read fine; a *write* faulted). Cause: ISV is also 0 for **writeback (pre/post-indexed)
and pair/SIMD** load/stores. The `GICR_IPRIORITYR` loop, written with
`write_volatile`, was lowered by the `extreme-size` profile's optimizer to a
post-indexed store:

```
str  w10, [x8], #0x4      ; ISV = 0  → HVF asserts
```

`release` happened to emit `str w10, [x8, #off]` (ISV=1), which is why only
`extreme` crashed. `write_volatile` guarantees the access happens but **not** the
addressing mode.

**Fix:** `gic_v3.rs` does all GICv3 MMIO through small inline-asm helpers
(`mmio_r32`/`mmio_w32`, and a `strb` for `set_priority`) that force a plain
single-register `ldr`/`str` with base-register-only addressing — ISV=1 on every
optimization level. This is the general rule for MMIO on hardware/HVF: never let
the compiler pick the addressing mode for a device access.

## Result

`llama-cli -m /models/stories15M-q4_0.gguf -p "Once upon a time" -n 16 -t 1 -c 256 -st`

| Metric      | TCG (`-t 1`) | HVF (`-cpu host`) | Speedup |
|-------------|--------------|-------------------|---------|
| Prompt      | ~100 t/s     | ~7000–7500 t/s    | ~70×    |
| Generation  | ~13–14 t/s   | ~1300–1700 t/s    | ~100×   |
| Full run    | tens of s    | ~1.7 s wall       |         |

(M4 Pro, QEMU 10.2.0, 256 MB, release build. The lingering-llama OOM on repeated
runs without `-st` is unchanged — see docs/LOW_MEMORY_ENVIRONMENT.md.)

## How to run

```bash
cargo run --release                # HVF auto-selected on Apple Silicon
HVF=0 cargo run --release          # force TCG (e.g. deterministic gdb crash repro)
ssh -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no -p 2222 root@localhost
```

The runner selects HVF when `uname` is `Darwin`/`arm64` and QEMU lists the `hvf`
accelerator; otherwise it uses TCG. HVF runs on real-hardware timing and is
non-deterministic, so prefer `HVF=0` for gdbstub-based deterministic crash repro.

Akuma is single-core, so HVF's gain is native instruction execution, not
parallelism — keep `-t 1` for llama.
