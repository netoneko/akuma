# Taoxichuan's `uname` said Akuma (2026-11-01)

**Stability: A**: fixed and verified on the Mac TCG rig.

## Symptom

Inside a Taoxichuan guest, `uname -a` answered `Akuma akuma 0.0.8 <sha>-release x86_64`, the AArch64 kernel's
identity, and GLM, asked what the kernel calls itself, said "Akuma". Noticed by the user while talking to goose in the
fixed point's K2 guest on the ryzen.

## Cause

`uname` moved from a Taoxichuan arm to `akuma-syscalls-glue` in the first glue batch (`74962ae5`, cleanup step 2c).
The arm had answered `Linux` / `taoxichuan` / `6.6.0-akuma` / `#1 SMP akuma-taoxichuan`; glue's `sys_uname` copies one
compile-time `UTSNAME` (`Akuma`, `akuma`, glue's release, the build's sha and profile), so every kernel on glue
answers Akuma. Nothing checks `uname` (the battery does not), so the change went unnoticed. A Linux-ABI kernel should
say `Linux`: build systems, `./configure` and runtime checks branch on `uname -s`.

## Fix

Glue has `proc::UtsIdentity` and `proc::set_uts_identity`: a kernel registers its `sysname`, default `nodename`,
`release` and `version` at init, and `sys_uname` writes them over the defaults field by field (whole fields, so a shorter
name leaves no tail). `machine` stays the compiled architecture's, and `sethostname`/`setdomainname` still override.
A kernel that registers nothing keeps glue's answer, so the AArch64 kernel and amd64 are unchanged. Taoxichuan
registers `Linux` / `taoxichuan` / `6.6.0-akuma` / `version::BUILD_ID`.

## Verified

Mac TCG rig: `uname -a` is `Linux taoxichuan 6.6.0-akuma 98b7455f-release x86_64`, `uname -s` is `Linux`, `hostname`
is `taoxichuan` and `hostname tx2` then `uname -n` answers `tx2`. Battery 9/9, 0 faults. Glue host clippy, AArch64
release and amd64 builds clean.

Open: the K2 guest left running on the ryzen predates this and still answers Akuma until a newer kernel boots there.

## Background

- [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md): step 2c, the glue batches.
