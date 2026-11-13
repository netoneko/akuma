# amd64: `box run` failed to spawn after the ELF loader was gated (2026-11-01)

**Stability: B**: fixed and verified on QEMU TCG; not run on the metal.

## Symptom

On the amd64 kernel every `box run` (and `box use` of a fresh container) answered `box run: failed to spawn
/bin/busybox` (`scripts/box_amd64_suite.py`: 12 checks failed). Found while gating the process-model work
(`TAOXICHUAN_CLEANUP_PLAN.md` step A4), on a tree that had not touched the spawn path.

## Cause

amd64 dispatches `box`'s private `SPAWN_EXT` (x86_64 `0x1000 + 315`) to glue's `sys_spawn_ext`, which loads the
image through `akuma-exec`'s ELF loader. Row 8 of `AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md` (`3e2bb725`, "one ELF
loader") put glue's `execve`/`SPAWN`/`SPAWN_EXT` behind a new `exec` feature and turned it on only for the AArch64
kernel, on the premise that both x86 kernels load with their own loaders and reach only `install_image`. True for
`execve`; not for amd64's box spawns, which still go through glue. Without the feature the arm is compiled out and
glue answers `ENOSYS`.

## Fix

`amd64/Cargo.toml`: glue with `features = ["smoltcp", "exec"]`. Taoxichuan has no `box` and keeps the feature off,
so its audit surface is unchanged.

## Verified

`scripts/box_amd64_suite.py` against `INIT=/bin/sshd amd64/run.sh` (TCG): no failures (12 before). amd64 boot
self-tests 819/819.

Open: the dependency-analysis row should have listed amd64's remaining glue loader users before gating; any other
amd64 path into glue's loader would have failed the same way. None other is known.

## Background

- [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md), the row-8 entry.
- [`AKUMA_AMD64_BOXES_AUDIT.md`](AKUMA_AMD64_BOXES_AUDIT.md): what the box suite checks.
