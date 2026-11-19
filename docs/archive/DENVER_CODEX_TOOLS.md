# Codex command tool on Denver

An opt-in amd64 Denver profile, `--features codex`, runs Codex 0.169.0's code-mode `exec_command` in an unboxed guest with `sandbox_mode = "danger-full-access"`. The probe uses a local fake Responses API, not real credentials. It asks Codex to write a file under `/work`, read it back, and return its contents; the model transcript must include the marker and exit code zero. This is a command-tool test, not a claim about every Codex tool, a provider, or sandbox enforcement.

## What changed

Denver had `wx-relaxed` already. The existing `userspace/forktest/c_stress/jitprobe.c` passed with it, so executable-page policy was not the immediate blocker. The code-mode host instead died with an invalid-opcode trap while starting its runtime. With temporary syscall tracing, a roughly 12 GiB `PROT_NONE | MAP_NORESERVE` mmap was placed near the beginning of the original 16 GiB slab. The host then requested a page at the next 1 TiB boundary, but Denver ignored the non-fixed hint and returned an address in the old slab. Subsequent smaller large-reservation attempts returned `ENOMEM`, followed by the trap. The inference was that the host needs a much larger aligned virtual placement window; the unmodified binary's successful end-to-end run with the profile supports that diagnosis, but does not establish every internal V8 requirement.

`large-virtual-reservations` is off by default in the Tokio kernels. The opt-in amd64 profile enlarges the slab to 2 TiB and prefers 1 TiB alignment for allocations of at least 4 GiB, keeping skipped address space available through the shared hole table. Because Tokio's lazy representation currently creates one page-table entry per reserved page, each mmap and page-walking memory operation is limited to 16 GiB in this profile: rejecting huge speculative reservations and oversized walks bounds physical page-table consumption and per-call work. This is a targeted compatibility profile, not a sparse virtual-memory implementation or full Linux mmap semantics. AArch64 is rejected at compile time until its machine path has been tested. The Denver `codex` feature composes this profile, `wx-relaxed`, `shell` and the existing JVM gate features; it does not enable boxes or sandboxing.

`MAP_FIXED_NOREPLACE` now checks present and `PROT_NONE` lazy pages and returns `EEXIST` on overlaps. Free fixed-address requests **inside** the slab are honored, and the skipped gap remains allocatable; the extended `mapnoreplace.c` probe exercises collisions and reuse after `munmap`. A free NOREPLACE request outside the slab returns `ENOMEM` rather than incorrectly succeeding at a different address. Historical `MAP_FIXED` handling outside the slab remains unchanged and still needs a proper external-region implementation.

## Verify

```sh
cargo build -p denver --target x86_64-unknown-none --release --features codex
DENVER_KERNEL="$PWD/target/x86_64-unknown-none/release/denver" \
CODEX_HOST_BINARY=/path/to/codex-code-mode-host \
DENVER_BUSYBOX=/path/to/static-musl-busybox \
CODEX_MODELS_CACHE="$HOME/.codex/models_cache.json" \
scripts/codex/denver-tool-probe.sh
```

`CODEX_BINARY` optionally selects the Codex binary and `DENVER_FAKE_PORT` selects a free local port. The model cache must describe the configured fake-model name `gpt-6.1-terra` and its code-mode tool. The script builds a disposable ext2 image and uses a QEMU snapshot. It never copies authentication into the image. A successful run reports `RESULT PASS` only when the actual tool response includes both the file marker and exit code 0.

Run `mapnoreplace.c` with `0x600010000000` for the current slab (the default address intentionally tests an unsupported external address). Also run `jitprobe.c`, `mapfixed.c`, and `denver/probes/codex-va.c` (the latter checks that oversized page-walking operations are refused). The `akuma-user-layout` tests cover aligned placement, the skipped hole, fixed claims, and reuse. Recheck the JVM and other Tokio-kernel gates before making the profile default.

## Remaining work

The container feature is still experimental: boxed file-backed mmap is refused, and the real `box` isolation gate is incomplete (see `DENVER_BOX_WIRING.md`). The Codex test runs without a sandbox. Tokio still needs the main kernel's seccomp syscall entry/enforcement and private-namespace plumbing before the `scripts/bwrap_suite.py` and `userspace/seccomp_probe` gates can support sandboxed tools. External fixed mappings, large truly sparse reservations, competing allocations, and AArch64 need their own implementation and regression tests.
