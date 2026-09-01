# GIC (interrupt controller)

Source: `crates/akuma-gic` (the whole driver, since 2026-09-01 —
[`archive/AKUMA_GIC_CONSOLIDATION.md`](../../../archive/AKUMA_GIC_CONSOLIDATION.md)).
It was `src/gic.rs` + `src/gic_v3.rs` + the redistributor half of
`src/smp_shared.rs` until then.

> **Stability: B (watch).** The GICv3 backend and the HVF boot-blocker it
> fixed have been dormant since 2026-06-09; the `smp` feature's cross-core
> doorbell (`trigger_sgi_core`) is new (2026-06-29) and still under active
> development. The recurring lesson: **never let the compiler choose the
> addressing mode for a device MMIO access** — a writeback (`str w, [x], #4`)
> or pair/SIMD store sets the data-abort's ISV bit to 0, and QEMU's HVF
> backend `assert(isv)`s on any data abort it can't decode.

## There is one backend

GICv3, always. The CPU interface is EL1 system registers (`ICC_*_EL1`) and
SGI/PPI config lives in the per-PE redistributor. QEMU is always started with
`-machine virt,gic-version=3`, under both accelerators.

A legacy GICv2 MMIO backend existed behind `feature = "gic-v2"` until
2026-09-01. It was deleted: no build script, profile or acceptance playbook ever
enabled it, and it could not run under HVF at all — QEMU presents GICv3 there
with no `0x0801_0000` CPU-interface frame, so the driver's first distributor
write faulted with `ISV=0` and HVF asserted (see `archive/QEMU_HVF_ISV_BUG.md`
root cause 1). Recover it from `git show <commit>~1:src/gic.rs` if a TCG-only
reference is ever wanted; nothing in the tree dispatches on a backend any more.

## Who calls what

| Path | Entry points |
|---|---|
| BSP bring-up | `init` (distributor + this PE's redistributor + CPU interface) |
| Secondary bring-up | `secondary_init(idx, RedistributorLayout)` — takes the redistributor geometry as an argument, read from `src/platform.rs`'s installed device map, so an FDT-discovered redistributor beats any compile-time literal |
| IRQ dispatch | `acknowledge_irq`, `end_of_interrupt` |
| Registration | `enable_irq`, `set_priority` |
| Scheduling | `SGI_SCHEDULER`, `trigger_sgi` (single-core self-target), `trigger_sgi_self` / `trigger_sgi_core` / `broadcast_sgi` (`kernel_smp_shared` only) |

`init` reaches the hardware through the `addr::DEV_GIC*` device-window VAs;
`secondary_init` uses PAs, because it runs on the boot page table where only the
low 1 GiB identity block exists. Both are correct; they are different mappings
of the same registers.

## GICv3 init sequence

`akuma_gic::init()`, in order:

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
(`rust_irq_handler_with_sp`, `crates/akuma-exceptions/src/lib.rs`) does:

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

## Cross-core doorbell (shared-kernel SMP)

`trigger_sgi_core(target_aff0, sgi_id)` (`gic_v3.rs:243-253`,
`cfg(kernel_smp_shared)` only) targets one specific core by affinity-0
(`MPIDR & 0xff`) via `ICC_SGI1R_EL1`'s 16-bit TargetList, instead of
`trigger_sgi`'s hardcoded "this CPU" target list. `trigger_sgi_self`
(self-targeted, reading its own `MPIDR`) builds on it so the shared timer
handler rings *this* core's scheduler SGI rather than always hitting PE0.
Valid only for `aff0 < 16` (QEMU `virt`'s single affinity-1 cluster). See
[`../smp-shared.md`](../smp-shared.md).

## Background

- `archive/QEMU_HVF_ISV_BUG.md` — the four HVF boot-blockers, of which GICv2
  MMIO (root cause 1) and writeback MMIO addressing (root cause 4) are GIC-
  specific.
- `archive/DEVICE_MMIO_VA_CONFLICT.md` — why GIC frames live under `L0[1]`.
