# `link_kernel.sh` wrote no `.bin` for any kernel (2026-11-05)

## Symptom

`cargo build -p shanghai --target aarch64-unknown-none --release` succeeded and left **no** `shanghai.bin`, so QEMU's
`-kernel` had nothing to take (a plain ELF gets no device tree). `akuma.bin` was also stale: its mtime was the last
`cargo run`, not the last `cargo build`. The `akuma-tokio-kernel` README promised `osaka.bin`.

## Cause

Two separate things, found in this order.

1. **The pattern matched a path cargo no longer passes.** `scripts/link_kernel.sh` (the `-C linker=` wrapper, written
   2026-08-26 so `cargo build` leaves a fresh `.bin`) converted only `*/deps/akuma-*`. The 2026-09 nightly links to
   `<profile>/build/<pkg>/<hash>/out/<name>` (printed from the wrapper: `.../release/build/shanghai/1b2d5924e5f1eb66/out/shanghai`),
   so the `case` matched nothing, for the main kernel too, and the `|| true` that keeps a conversion failure from failing a
   link also kept this quiet. The script's own header names this failure mode (a `.bin` from the other arm looking current).
2. **The residual hole the header already documents.** When cargo reuses a cached ELF (switching back to a feature set built
   earlier) it uplifts without relinking, so the wrapper never runs and the `.bin` on disk is the other build's. The B22
   gate hit it: its aarch64 *control* kernel booted the *middleware* `.bin` and printed the middleware's `PASS` where it
   expected `CONTROL PASS`.

## Fix

- `scripts/link_kernel.sh` handles both layouts and converts any binary linked for the target, not only `akuma-*`
  (only final links of binaries reach it: rlibs are archived, build scripts and proc macros link for the host). `mkbin.sh`'s
  progress line no longer reaches cargo as a warning.
- `shanghai/mw_gate.sh` runs `scripts/mkbin.sh` on the ELF itself after every build, as `scripts/cargo_runner.sh` does, so the
  residual hole cannot pick the kernel. Anything else that consumes a `.bin` after switching feature sets must do the same.
- `shanghai/build.rs` gained the aarch64 branch (`BOOT_STACK_SIZE`); it was amd64-only, so Shanghai did not build for aarch64.

## Verified

`touch shanghai/src/main.rs && cargo build -p shanghai --target aarch64-unknown-none --release` writes `shanghai.bin`
(1.7 MB) with no warnings; `sh shanghai/mw_gate.sh aarch64` boots control and middleware as different images and prints
`CONTROL PASS` / `PASS` at `SMP=1` and `SMP=4`.

## Open

The residual hole is still a hole for `cargo build` users: only the explicit `mkbin.sh` step closes it. `akuma.bin` was not
rebuilt here.

## Background

`scripts/link_kernel.sh` (header), `scripts/mkbin.sh`, `proposals/B22_SYSCALL_MIDDLEWARE.md`.
