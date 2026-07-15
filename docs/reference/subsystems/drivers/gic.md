# GIC (interrupt controller)

Source: `src/gic.rs` (backend selector), `src/gic_v3.rs` (default backend).

> **Stability: B (watch).** The GICv3 backend and the HVF boot-blocker it
> fixed have been dormant since 2026-06-09; the `smp` feature's cross-core
> doorbell (`trigger_sgi_core`) is new (2026-06-29) and still under active
> development. The recurring lesson: **never let the compiler choose the
> addressing mode for a device MMIO access** — a writeback (`str w, [x], #4`)
> or pair/SIMD store sets the data-abort's ISV bit to 0, and QEMU's HVF
> backend `assert(isv)`s on any data abort it can't decode.

## Backend selection

`src/gic.rs` is a thin dispatcher over two mutually exclusive backends,
selected at compile time:

| Backend | Selected by | CPU interface | Works under |
|---|---|---|---|
| GICv3 (default) | `not(feature = "gic-v2")` | EL1 system registers (`ICC_*_EL1`) | HVF and TCG (`gic.rs:1-13`) |
| GICv2 | `feature = "gic-v2"` | MMIO frame at `DEV_GIC_CPU_VA` (phys `0x0801_0000`) | TCG only |

`gic.rs`'s public API (`init`, `enable_irq`, `acknowledge_irq`,
`end_of_interrupt`, `trigger_sgi`, `set_priority`, `gic.rs:194-250`) just
`#[cfg]`-branches to either the local GICv2 `Gic` struct or `crate::gic_v3::*`.
Callers never care which backend is active. QEMU is always started with
`-machine virt,gic-version=3` (both accelerators); the `gic-v2` feature exists
for reference/fallback under TCG only, per `config-flags.md`.

**Why GICv3 is default:** under `-accel hvf` on Apple Silicon, QEMU presents
GICv3 and there is no `0x0801_0000` GICv2 CPU-interface MMIO frame at all — the
legacy driver's first distributor write past the v2/v3-divergent region faults
with ISV=0 and HVF asserts. See `archive/QEMU_HVF_ISV_BUG.md` root cause 1.

## GICv3 init sequence

`gic_v3::init()` (`src/gic_v3.rs:144-182`), in order:

1. Set `ICC_SRE_EL1.SRE=1` — switch the CPU interface from (nonexistent)
   MMIO to system registers.
2. Wake this PE's redistributor: clear `GICR_WAKER.ProcessorSleep`, spin until
   `ChildrenAsleep` clears.
3. Configure SGIs/PPIs (INTID 0-31) in the redistributor's **SGI_base** frame:
   all Group 1 non-secure (`IGROUPR0`), mid priority (`IPRIORITYR`), all
   disabled (`ICENABLER0` — `enable_irq` turns on what's used).
4. Enable the distributor: `GICD_CTLR.ARE_NS | ENABLE_GRP1`, spin on `RWP`
   (register-write-pending).
5. Configure the CPU interface: unmask all priorities (`ICC_PMR_EL1=0xFF`), no
   sub-priority grouping (`ICC_BPR1_EL1=0`), enable Group 1
   (`ICC_IGRPEN1_EL1=1`).

Akuma uses only **SGI 0** (scheduler doorbell) and **PPI 27** (EL1 virtual
timer) — see [`timers.md`](timers.md). No SPI is ever routed, so
`GICD_IROUTER` is never programmed (`gic_v3.rs:9-14`).

## Register frames and MMIO addressing

GICv3 has three MMIO frames on QEMU `virt` (confirmed from the generated DTB):
GICD at phys `0x0800_0000`, GICR RD_base and SGI_base at phys `0x080A_0000` /
`0x080B_0000`. All three are remapped to non-conflicting virtual addresses
under `L0[1]` — see [`../memory.md`](../memory.md) "Memory layout" for the
authoritative VA table (`DEV_GIC_DIST_VA`, `DEV_GICR_RD_VA`, `DEV_GICR_SGI_VA`
in `akuma_exec::mmu`); don't duplicate it here. Background on *why* devices
live in `L0[1]` instead of identity-mapped `L0[0]`:
`archive/DEVICE_MMIO_VA_CONFLICT.md`.

MMIO reads/writes go through `mmio_r32`/`mmio_w32` (`gic_v3.rs:75-92`), which
emit a single-register `ldr`/`str` via inline `asm!` rather than
`read_volatile`/`write_volatile`. This is deliberate: the optimizer is free to
lower a `write_volatile` loop to a post-indexed store (`str w, [x], #4`),
which sets ISV=0 and crashes QEMU under HVF — this bit the `extreme` build
profile specifically (root cause 4, `archive/QEMU_HVF_ISV_BUG.md`) while
`release` happened to emit an ISV-safe form. `set_priority`'s `strb` follows
the same rule. CPU-interface system registers are addressed by raw
`S<op0>_<op1>_C<n>_C<m>_<op2>` encoding (`gic_v3.rs:123-129`) rather than
mnemonics, so the `msr`/`mrs` assembles on any AArch64 toolchain regardless of
GICv3 mnemonic support.

## IRQ dispatch to the scheduler

`src/irq.rs` is the handler-registration layer above the GIC:
`register_handler(irq, f)` records `f` in a `Vec<Option<IrqHandler>>` and
calls `gic::enable_irq(irq)` (`irq.rs:84-96`). The unified IRQ entry
(`rust_irq_handler_with_sp`, `src/exceptions.rs:1455-1470`) does:

```
irq = gic::acknowledge_irq()
if irq == SGI_SCHEDULER: akuma_exec::threading::sgi_scheduler_handler_with_sp(irq, sp)  // EOIs itself, may switch SP
else: irq::dispatch_irq(irq); gic::end_of_interrupt(irq)
```

SGI 0 (`SGI_SCHEDULER`, `gic.rs:192`) is special-cased because it may return a
different stack pointer for a context switch; every other registered IRQ
(currently only PPI 27, the timer) goes through the generic dispatch table and
is EOI'd by the wrapper. For what the scheduler does once it gets the SGI, see
[`../scheduler.md`](../scheduler.md).

On a **secondary** core (`smp` feature), `register_handler` cannot be used for
per-PE interrupts — it pokes core 0's hardcoded redistributor frame, which
isn't even mapped on a secondary. `register_handler_no_gic` (`irq.rs:106-112`)
registers the dispatch-table entry only; the secondary enables its own
SGI/PPI directly in its own redistributor via `secondary_gic_init`
(`src/smp.rs`).

## Multikernel doorbell

`trigger_sgi_core(target_aff0, sgi_id)` (`gic_v3.rs:243-253`,
`cfg(kernel_smp)` only) targets one specific core by affinity-0
(`MPIDR & 0xff`) via `ICC_SGI1R_EL1`'s 16-bit TargetList, instead of
`trigger_sgi`'s hardcoded "this CPU" target list. It's the cross-core doorbell
used to wake a parked or busy peer core; valid only for `aff0 < 16` (QEMU
`virt`'s single affinity-1 cluster). See [`../scheduler.md`](../scheduler.md)
"SMP / multikernel" for the one-kernel-per-core design this doorbell serves.

## Background

- `archive/QEMU_HVF_ISV_BUG.md` — the four HVF boot-blockers, of which GICv2
  MMIO (root cause 1) and writeback MMIO addressing (root cause 4) are GIC-
  specific.
- `archive/DEVICE_MMIO_VA_CONFLICT.md` — why GIC frames live under `L0[1]`.
- `archive/MULTIKERNEL.md` — the SMP design `trigger_sgi_core` serves.
