# Debug & diagnostic scripts

Grade: — (an index, not an architecture doc)

`scripts/` also holds the build-profile wrappers (covered in
[`../build-system.md`](../build-system.md)) and two self-contained campaign
harnesses with their own `README.md` — `scripts/bkl_rustc_bench/` and
`scripts/bkl_smp_regimen/` — not duplicated here. This directory indexes the
rest: standalone debugging and regression helpers that have no README of
their own and, before this page, weren't mentioned anywhere in
`docs/reference/`.

Everything below is host-side Python/bash; run from the repo root.

| Category | Doc |
|---|---|
| Log & crash analysis | [`log-analysis.md`](log-analysis.md) |
| Multi-VM / hang hunting | [`multi-vm.md`](multi-vm.md) |
| Fork / SMP regression harnesses | [`fork-smp-harnesses.md`](fork-smp-harnesses.md) |
| Container / environment helpers | [`env-helpers.md`](env-helpers.md) |

## Background

- [`../../archive/SCRIPTS.md`](../../archive/SCRIPTS.md) — the detailed pass
  behind this index: per-script history, why the dead ones (removed
  2026-08-07) were dead, and what each survivor is actually for.
- [`../../archive/BUG_FIX_LIST.md`](../../archive/BUG_FIX_LIST.md) — itemized
  fix history; several scripts below exist to regress a specific entry there.
