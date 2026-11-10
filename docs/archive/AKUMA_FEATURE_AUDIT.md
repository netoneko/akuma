# Cargo feature audit (2026-11-10)

Part of the multiarch attempt ([`AKUMA_MULTIARCH_ATTEMPT_0.md`](AKUMA_MULTIARCH_ATTEMPT_0.md)): the
research doc's end state says "features are devices, same names in both machine halves"
([`AKUMA_ARCH_CONVERGENCE_RESEARCH.md`](AKUMA_ARCH_CONVERGENCE_RESEARCH.md) §1). This is the
measured starting point. Read-only audit; nothing changed yet.

## Counts

| manifest | features |
|---|---:|
| root `Cargo.toml` (AArch64 kernel) | 39 (+ `default`) |
| `amd64/Cargo.toml` | 22 (+ `default`) |
| `akuma-kernel-glue` / `-core` (forwarding layers) | 22 / 9 |
| all `crates/*/Cargo.toml` | 193 |

## Findings

1. **Two parallel feature vocabularies.** 16 names exist in both root and `amd64/` with the same
   job and *different forwarding lists*, written by hand twice: `smp-shared`, `fs-cache`,
   `syscall-debug-info`, `no-tests`, `net-noalloc`, the 7 `sc-*` (`sc-epoll sc-eventfd sc-timerfd
   sc-pidfd sc-aio sc-sysv-ipc sc-containers`) and four `no-bkl-*`. Examples of drift already
   present: root `smp-shared` also turns on `no-bkl-vfs` and `no-bkl-irq` forwards through
   different crates than amd64's; root `syscall-debug-info` lacks the `dep:` entries amd64 needs;
   `no-tests` in root forwards to 6 crates, in amd64 it is `[]` (a cfg flag only).
2. **The six `no-bkl-*` switches are, in every shipped build, one switch.** `smp-shared` enables
   all six (root) or four (amd64) and `smp-shared` is in both `default`s. They gate 76 `cfg` sites
   (`kernel_no_bkl_{network 28, vfs 17, process 9, mm 8, drivers 7, irq 7}`) via per-crate
   `build.rs`. Independent use is only the A/B drivers (`scripts/cargo_runner.sh` comment,
   `scripts/validate_fork_smp.py`) — and `akuma_bkl::policy` already has *runtime* toggles for the
   same phases. **Not dead; do not delete blind.** Candidate: keep the runtime toggles as the A/B
   mechanism and fold the compile-time six into `smp-shared`.
3. **Eleven diagnostic/instrumentation flags**, all off by default: `leak-instr`, `pmm-instr`,
   `bkl-profile`, `net-profile`, `read-profile`, `syscall-debug-info`, `reboot-trace` (amd64,
   *default-on*), `pmm-forensics`, `mm-forensics`, `net-waker-park`, `net-direct-waker`. Referenced
   from scripts/docs: `leak-instr` 0, `pmm-instr` 0, `net-direct-waker` 0, `reboot-trace` 0,
   `pmm-forensics` 1, `mm-forensics` 1. Zero references = candidates to confirm with the owner and
   delete or fold into one `instr` feature.
4. **Bundles that are really build targets, not features:** `devbox` (`rump-default userspace-sshd
   sc-reboot`), `devbox-smoltcp` (`userspace-sshd smp-shared`), `taoxichuan` (`no-tests extreme
   userspace-sshd`). Referenced 24–107 times, so they stay, but `build-profiles.md` already says
   "a build target is profile + feature set": these are the named feature sets.
5. **`rump` now has three spellings:** `rump` (compiled in; *is in root `default`*), `rump-default`
   (starts it), `rump-tests`. With D1 (universal optional feature) this wants one place in the
   shared spine and the same name on amd64, off by default there.
6. **amd64 has switches AArch64 lacks and vice versa** (`linux-file-io`, `console-notify`,
   `no-tests`-as-flag on amd64; `smoltcp`, `sound`, `many-sessions`, `platform-firecracker`,
   `extreme`, `sc-reboot`, `userspace-sshd` on root). Under the device-names rule (`console disk net
   smp shell`) these are the arch-specific remainder or should be named the same.
7. **Features unification risk** (research doc §7) is a direct consequence of 1: two lists, so a
   flag added to one kernel is easy to forget on the other.

## Proposed simplifications (none applied)

| # | change | risk | gate |
|---|---|---|---|
| F1 | One shared feature table: a `akuma-kernel-features`-style doc + a check script that asserts the 16 common names forward the same crates in root and `amd64/` | none (a checker) | script in CI/pre-commit |
| F2 | Fold the six compile-time `no-bkl-*` into `smp-shared`; A/B stays on the runtime toggles | medium: 76 cfg sites, SMP behaviour | AArch64 boot suite byte-identical; amd64 TCG SMP=1 826/0; SMP gates need KVM/metal |
| F3 | Delete zero-reference instrumentation after owner review; merge the rest into one `instr` feature | low-medium | build matrix |
| F4 | Make `sc-*` one group feature `linux-extra-syscalls` (still individually settable for `extreme-size`) | low | `extreme-size` image size must not move |
| F5 | `rump` per D1: one name, off by default on amd64, `cargo tree` leak check | low | tree check |

## Background

[`docs/reference/subsystems/config-flags.md`](../reference/subsystems/config-flags.md) ·
[`docs/reference/build-profiles.md`](../reference/build-profiles.md) ·
research doc §7 (feature unification) and §9.
