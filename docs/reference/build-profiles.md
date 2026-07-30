# Build profiles & distributions

At-a-glance comparison of Akuma's seven build targets. For the exhaustive
per-feature/per-knob breakdown, see
[`reference/subsystems/config-flags.md`](subsystems/config-flags.md) — this
doc answers "which one do I build/run", that one answers "what exactly does
each flag do".

A build target is always **profile + feature set**, selected together by a
`scripts/build_*.sh` script (or `cargo run`/`overlays/devbox/run.sh` for
distros meant to boot). The profile only sets codegen (opt level, LTO,
codegen-units); the feature set is what actually changes behaviour. Two
targets can share a profile (`release`, `size`) and differ only in features —
`size` and `extreme-size` are the clearest example, since both use
`opt-level = "z"` and are told apart at build time solely by the `extreme`
feature (`build.rs` cannot see `OPT_LEVEL` to distinguish them).

## The seven targets

| Target | Profile | Build command | Binary size | Networking | Purpose |
|---|---|---|---|---|---|
| **release** (default) | `release` | `cargo build --release` / `cargo run --release` | 3.8 MB | smoltcp (native) + built-in SSH | Day-to-day development image. Full feature set: editor, sound, TLS (RSA + Ed25519), rump *available* (opt-in per box), all `sc-*` syscall families. |
| **size** | `size` (inherits `release`) | `scripts/build_size.sh` | — (not currently built; historically ~1-1.5 MB range) | smoltcp + built-in SSH + `kernel-tls` | Slimmer image for constrained VMs. Drops `neko` and `tls-rsa` (RSA-only HTTPS breaks; SSH is Ed25519-only and unaffected). Keeps every `sc-*` family. |
| **extreme-size** | `extreme-size` (inherits `size`) | `scripts/build_extreme_size.sh` | 728 KB | smoltcp + built-in SSH, **no HTTPS** | 4 MB RAM floor target. Same codegen knobs as `size`; the *only* discriminator is the `extreme` feature, since both profiles use `opt-level = "z"`. Drops `kernel-tls` entirely (no in-kernel `curl https://`), `neko`, `tls-rsa`, tighter stack/heap constants via `cfg(kernel_profile_extreme)`. |
| **release-smp** | `release-smp` (inherits `release`) | `cargo build --profile release-smp --features smp` | 2.9 MB | smoltcp + built-in SSH | Multikernel / one-kernel-per-core (see `docs/reference/subsystems/smp.md`). Off by default — `cargo build --release` is byte-for-byte single-core; this target adds secondary-core bringup, PSCI `CPU_ON`, the inter-core message bus. |
| **release-smp-shared** | `release-smp-shared` (inherits `release`) | `cargo build --profile release-smp-shared --features smp-shared` | 4.0 MB | smoltcp + built-in SSH | Real (shared-kernel) SMP — one shared kernel across cores (see `docs/reference/subsystems/smp-shared.md`). The **inverse** of `release-smp`: all cores share one kernel/PMM/heap/run-queue under real locks. Mutually exclusive with `smp` (build.rs panics if both). |
| **devbox** | `devbox` (inherits `release`) | `scripts/build_devbox.sh` / `overlays/devbox/run.sh` | 1.4 MB | **rump only** (no smoltcp, no built-in SSH) | *(deferred — see `devbox-smoltcp`.)* Rump-stack workstation image: NetBSD rump as box 0's default stack, built-in SSH dropped. `--no-default-features`, so smoltcp (and `kernel-tls`/`tls-rsa`/built-in SSH) is compiled out. |
| **devbox-smoltcp** (default devbox) | `release-smp-shared` | `scripts/build_devbox_smoltcp.sh` / `overlays/devbox/run-smoltcp.sh` | 1.7 MB | smoltcp (native) + userspace `/bin/sshd`, **no built-in SSH** | The **default** "develop inside Akuma" image (2026-07-19). Native smoltcp stack for box 0 + real shared-kernel SMP (`SMP=N`); built-in SSH dropped (`userspace-sshd`) so the userspace `/bin/sshd` (herd) over smoltcp is the only sshd. Keeps the default feature set (smoltcp/`kernel-tls` stay in). rump_server work is deferred. |

Sizes above are from the checked-out `target/aarch64-unknown-none/*/akuma`
binaries as of 2026-07-18; rebuild locally to get current numbers — they
drift with every feature/dependency change.

## Feature deltas vs. default `release`

`release` builds with cargo's normal default feature resolution
(`neko, smoltcp, kernel-tls, tls-rsa, sound, rump, sc-aio, sc-sysv-ipc,
sc-framebuffer, sc-containers, sc-timerfd, sc-eventfd, sc-pidfd, sc-epoll`).
Every other target passes `--no-default-features` and explicitly re-adds only
what it wants:

| Target | Drops vs. default | Adds vs. default |
|---|---|---|
| `size` | `neko`, `tls-rsa`, `rump`, `sound` | `no-tests` |
| `extreme-size` | `neko`, `tls-rsa`, `kernel-tls`, `rump`, `sound` | `no-tests`, `extreme` |
| `devbox` | `smoltcp`, `kernel-tls`, `tls-rsa` | `devbox` (→ `rump-default` + `userspace-sshd`), `no-tests` |
| `release-smp` | — (inherits default set) | `smp` |
| `release-smp-shared` | — (inherits default set) | `smp-shared` |
| `devbox-smoltcp` | — (inherits default set) | `devbox-smoltcp` (→ `userspace-sshd` + `smp-shared`), `no-tests` |

`release-smp`, `release-smp-shared`, and `devbox-smoltcp` all *keep* the full
default feature set and only layer their feature on top, rather than starting
from `--no-default-features` (unlike `size`/`extreme`/`devbox`). `smp` and
`smp-shared` are mutually exclusive (build.rs enforces).

## Which one do I want?

- **Developing/debugging the kernel day to day** → `release` (`cargo run --release`).
- **Testing a minimal-RAM boot path without going to the extreme** → `size`.
- **Verifying the kernel still fits a 4 MB VM** → `extreme-size`. No in-kernel HTTPS; use a userspace tool if you need TLS.
- **Working inside Akuma as a Unix box (self-hosted toolchain, editor, daily use)** → `devbox-smoltcp` (the default devbox: native smoltcp + real SMP; `overlays/devbox/run-smoltcp.sh`). The rump `devbox` is deferred but still boots via `overlays/devbox/run.sh` (needs `RUMP_NIC=1`).
- **Exercising real (shared-kernel) SMP** → `release-smp-shared` (`--features smp-shared`); see `docs/reference/subsystems/smp-shared.md`.
- **Exercising the multikernel (one-kernel-per-core) bringup** → `release-smp`, gated behind the §10/§11 acceptance test in `docs/MULTIKERNEL.md`.

## Background

- [`reference/subsystems/config-flags.md`](subsystems/config-flags.md) — full profile/feature/env-var/debug-knob reference.
- [`runbooks/build-devbox.md`](../runbooks/build-devbox.md) — step-by-step devbox build + boot.
- `archive/OPTIONAL_SMOLTCP.md` — why smoltcp became optional (the devbox's origin).
- `overlays/devbox/README.md` — devbox design rationale; `devbox-smoltcp` (default) vs. the deferred rump `devbox`.
- `docs/reference/subsystems/smp-shared.md` + `docs/archive/SMP_SHARED.md` — real shared-kernel SMP.
- `Cargo.toml` `[profile.*]` blocks, each with inline commentary on what distinguishes it.
