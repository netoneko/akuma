# Build profiles & distributions

At-a-glance comparison of Akuma's five build targets. For the exhaustive
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

## The five targets

| Target | Profile | Build command | Binary size | Networking | Purpose |
|---|---|---|---|---|---|
| **release** (default) | `release` | `cargo build --release` / `cargo run --release` | 3.8 MB | smoltcp (native) + built-in SSH | Day-to-day development image. Full feature set: editor, sound, TLS (RSA + Ed25519), rump *available* (opt-in per box), all `sc-*` syscall families. |
| **size** | `size` (inherits `release`) | `scripts/build_size.sh` | — (not currently built; historically ~1-1.5 MB range) | smoltcp + built-in SSH + `kernel-tls` | Slimmer image for constrained VMs. Drops `neko` and `tls-rsa` (RSA-only HTTPS breaks; SSH is Ed25519-only and unaffected). Keeps every `sc-*` family. |
| **extreme-size** | `extreme-size` (inherits `size`) | `scripts/build_extreme_size.sh` | 728 KB | smoltcp + built-in SSH, **no HTTPS** | 4 MB RAM floor target. Same codegen knobs as `size`; the *only* discriminator is the `extreme` feature, since both profiles use `opt-level = "z"`. Drops `kernel-tls` entirely (no in-kernel `curl https://`), `neko`, `tls-rsa`, tighter stack/heap constants via `cfg(kernel_profile_extreme)`. |
| **release-smp** | `release-smp` (inherits `release`) | `scripts/build_smp.sh` (paired with `--features smp`) | 2.9 MB | smoltcp + built-in SSH | Multikernel / one-kernel-per-core (see `docs/reference/subsystems/smp.md`). Off by default — `cargo build --release` is byte-for-byte single-core; this target adds secondary-core bringup, PSCI `CPU_ON`, the inter-core message bus. |
| **devbox** | `devbox` (inherits `release`) | `scripts/build_devbox.sh` / `overlays/devbox/run.sh` | 1.4 MB | **rump only** (no smoltcp, no built-in SSH) | "Sit down and develop inside Akuma" workstation image. Makes the NetBSD rump stack the *default* stack for box 0 and drops the in-kernel SSH server, so userspace `/bin/sshd` (via herd) is the only sshd. Built with `--no-default-features`, so smoltcp and everything coupled to it (`kernel-tls`, `tls-rsa`, built-in SSH) is compiled out entirely — not just disabled at runtime. |

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

`release-smp` is the odd one out: it's the only non-default target that
*keeps* the full default feature set and only layers `smp` on top, rather
than starting from `--no-default-features`.

## Which one do I want?

- **Developing/debugging the kernel day to day** → `release` (`cargo run --release`).
- **Testing a minimal-RAM boot path without going to the extreme** → `size`.
- **Verifying the kernel still fits a 4 MB VM** → `extreme-size`. No in-kernel HTTPS; use a userspace tool if you need TLS.
- **Working inside Akuma as a Unix box (self-hosted toolchain, editor, daily use)** → `devbox`. This is the only target that runs the rump network stack by default and needs `RUMP_NIC=1` at boot (`overlays/devbox/run.sh` sets this).
- **Exercising multikernel/SMP bringup** → `release-smp`, gated behind the §10/§11 acceptance test in `docs/MULTIKERNEL.md`.

## Background

- [`reference/subsystems/config-flags.md`](subsystems/config-flags.md) — full profile/feature/env-var/debug-knob reference.
- [`runbooks/build-devbox.md`](../runbooks/build-devbox.md) — step-by-step devbox build + boot.
- `overlays/devbox/README.md` — devbox design rationale (rump-as-default, no built-in SSH).
- `archive/OPTIONAL_SMOLTCP.md` — why smoltcp became optional (the devbox's origin).
- `Cargo.toml:77-115` — the five `[profile.*]` blocks, each with inline commentary on what distinguishes it.
