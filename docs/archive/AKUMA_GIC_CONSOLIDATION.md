# `akuma-gic`: one interrupt controller, one crate

**2026-09-01.** The GICv3 driver was being run from **three** files. It is now
`crates/akuma-gic`. `src/gic.rs` and `src/gic_v3.rs` are deleted;
`src/smp_shared.rs` lost its redistributor half.

## The result

| | before | after |
|---|---|---|
| `src/` production `unsafe` blocks | 104 | **92** |
| `crates/` production `unsafe` blocks | 324 | **329** |
| GIC blocks in `src/` | 12 | **0** |

Twelve `unsafe` blocks left `src/` and only **five** arrived in the crate. The
other seven are gone from the tree: four deleted with a dead backend, three that
turned out to be duplicates of code the crate already had.

## Why there was no `akuma-gic` already

Every earlier extraction chased *pure logic worth host-testing* — the readiness
map, the mapping plan, the futex deadline algebra. A GIC driver has none of
that: it is MMIO and system registers end to end, and there is nothing to assert
about it off-hardware. It never met the criterion.

It meets the current one, which is different: get `unsafe` **out of `src/`**, so
`src/` can take `#![forbid(unsafe_code)]`. Under that criterion a crate with no
testable logic and eight `unsafe` blocks is still worth making.

## Four blocks were deleted, not moved: `gic-v2`

All four `unsafe` blocks in `src/gic.rs` were inside
`#[cfg(feature = "gic-v2")] impl Gic` — a legacy GICv2 MMIO backend. The feature:

- was enabled by **no** build script, profile, or acceptance playbook — only its
  own `Cargo.toml` line and doc mentions referenced it;
- **could not work** on the primary dev platform. Under `-accel hvf` QEMU
  presents GICv3 and there is no `0x0801_0000` GICv2 CPU-interface frame at all,
  so the driver's first distributor write faulted with `ISV=0` and HVF asserted
  (`QEMU_HVF_ISV_BUG.md` root cause 1);
- guarded a machine Akuma never starts: QEMU is always launched
  `-machine virt,gic-version=3`.

So `src/gic.rs` was a `#[cfg]` dispatcher between one live backend and one that
could not run. Deleting the dead half left nothing but forwarding, so the file
went too. `trigger_sgi_self` was the only entry point there that was more than a
forward; it moved into the crate.

**To recover it:** `git show <this commit>~1:src/gic.rs`.

## Three blocks were duplicates

`src/smp_shared.rs::secondary_gic_init` carried its own copies of:

- `mmio_w32` / `mmio_r32` — **byte-identical** to `gic_v3.rs`'s, down to the
  ISV-safety comment, which said "same reasoning as `gic_v3::mmio_w32`". The
  duplication was known and recorded rather than accidental.
- `GICR_WAKER_PROCESSOR_SLEEP` / `GICR_WAKER_CHILDREN_ASLEEP`, and the
  `GICR_SGI_*` offsets — all already present in `gic_v3.rs`.
- Four raw `msr` instructions re-doing `gic_v3::init`'s CPU-interface sequence
  (`ICC_SRE_EL1`, `ICC_PMR_EL1`, `ICC_BPR1_EL1`, `ICC_IGRPEN1_EL1`).

Folding it in as `akuma_gic::secondary_init` reuses the crate's existing
`mmio_*` pair and `read_sysreg!`/`write_sysreg!` macros, so all three blocks
disappeared. `src/smp_shared.rs` went from **4 `unsafe` blocks to 1** — the PSCI
SMC/HVC conduit call, and nothing else.

### What `secondary_init` needs from `src/`

The redistributor geometry is machine-specific (and on Firecracker depends on
the vCPU count), so it is **passed in** as a `RedistributorLayout`, read from the
installed device map by `src/platform.rs`. That keeps the FDT-discovered
redistributor winning over any compile-time literal — getting it wrong points a
core at another core's frames and silently costs it its timer interrupt.

Note the two paths reach the same hardware through *different mappings*, on
purpose: `init` uses the `addr::DEV_GIC*` device-window VAs, while
`secondary_init` uses PAs, because it runs during bring-up on the boot page
table where only the low 1 GiB identity block exists.

## What the crate does not do

The `ICC_*_EL1` writes deliberately did **not** move into `akuma-cpu`. That
crate's defining property is that everything in it is *safe to execute* from
safe code; enabling a CPU interface or writing `ICC_EOIR1_EL1` changes interrupt
delivery for the whole core. Same reasoning keeps `smc`/`hvc` out of it.

The crate cannot take `#![forbid(unsafe_code)]` and never will — it is 5 blocks
behind one stated contract, in the shape `akuma-net-nic` uses for DMA:

> Every address passed to `mmio_w32`/`mmio_r32` is a device-mapped GIC register —
> either inside the L0[1] device window or inside the low 1 GiB identity block.
> Nothing else may be passed.

## Do not "simplify" the MMIO

`read_volatile`/`write_volatile` are avoided deliberately. The optimizer may
lower a volatile loop to a post-indexed store (`str w, [x], #4`); writeback and
pair/SIMD forms set `ESR.ISV=0`, and QEMU's HVF backend asserts
(`hvf.c: assert(isv)`) on a data abort it cannot decode. That crashed QEMU under
HVF on `extreme-size` while working on `release`, purely because the two profiles
picked different addressing modes. The explicit `asm!` pins the instruction form.

## Verification

- `extreme-size` image **byte-identical** at 724,328 B.
- Host tests 1101 passed / 0 failed; clippy clean on `--release` and on both of
  the crate's feature arms.
- **SMP=4 boot**: 3/3 secondaries online (`secondary_init`), workers and
  userspace on all 4 cores (SGI delivery), one thread migrated across 4 distinct
  cores, **0 FAILED** across every self-test.
- **`extreme-size` boot**: serves ssh, which exercises the
  `#[cfg(not(kernel_smp_shared))]` single-core arm — `trigger_sgi` rather than
  `trigger_sgi_self`. That arm is invisible to `cargo check --release`.

### Two traps this hit

1. **`cargo check --release` does not compile the single-core GIC arm.** A bare
   `gic::trigger_sgi` call behind `#[cfg(not(kernel_smp_shared))]` in
   `src/main.rs` survived the rename and only `extreme-size` caught it. Same
   lesson as the `-D unused-imports` trap in
   [`AKUMA_FPCACHE_EXTRACTION.md`](AKUMA_FPCACHE_EXTRACTION.md): **build the
   `extreme-size` arm on every extraction.**
2. **`extreme-size` will not start at `MEMORY=4096K` on this tree.** QEMU rejects
   it before the kernel runs — `Not enough space for DTB after kernel/initrd`.
   That is unrelated to this change (the image is byte-identical across it), but
   it means `acceptance/05`'s 4.0 MB floor needs re-checking on its own.

## Background

- [`AKUMA_SMP_SHARED_SPLIT.md`](AKUMA_SMP_SHARED_SPLIT.md) — the file this took
  the redistributor half out of, and the inventory of what remains there.
- [`AKUMA_FPCACHE_EXTRACTION.md`](AKUMA_FPCACHE_EXTRACTION.md) — the same goal,
  two steps earlier.
- [`QEMU_HVF_ISV_BUG.md`](QEMU_HVF_ISV_BUG.md) — why GICv3 is the only backend
  that can work here, and why the MMIO instruction form is pinned.
- [`../reference/subsystems/drivers/gic.md`](../reference/subsystems/drivers/gic.md)
  — current-state driver reference.
