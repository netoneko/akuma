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
| **size** | `size` (inherits `release`) | `scripts/build_size.sh` | 882 KB text | smoltcp + built-in SSH + `kernel-tls` | Slimmer image for constrained VMs. Drops `neko` and `tls-rsa` (RSA-only HTTPS breaks; SSH is Ed25519-only and unaffected). Keeps every `sc-*` family. |
| **extreme-size** | `extreme-size` (inherits `size`) | `scripts/build_extreme_size.sh` | 665 KB text | smoltcp + built-in SSH, **no HTTPS** | 4 MB RAM floor target. Same codegen knobs as `size`; the *only* discriminator is the `extreme` feature, since both profiles use `opt-level = "z"`. Drops `kernel-tls` entirely (no in-kernel `curl https://`), `neko`, `tls-rsa`, tighter stack/heap constants via `cfg(kernel_profile_extreme)`. **Does not compile at `d3f28d6` — see [Known breakage](#known-breakage-extreme-size-at-d3f28d6).** |
| **release-smp** | `release-smp` (inherits `release`) | `cargo build --profile release-smp --features smp` | 2.9 MB | smoltcp + built-in SSH | Multikernel / one-kernel-per-core (see `docs/reference/subsystems/smp.md`). Off by default — `cargo build --release` is byte-for-byte single-core; this target adds secondary-core bringup, PSCI `CPU_ON`, the inter-core message bus. |
| **release-smp-shared** | `release-smp-shared` (inherits `release`) | `cargo build --profile release-smp-shared --features smp-shared` | 4.0 MB | smoltcp + built-in SSH | Real (shared-kernel) SMP — one shared kernel across cores (see `docs/reference/subsystems/smp-shared.md`). The **inverse** of `release-smp`: all cores share one kernel/PMM/heap/run-queue under real locks. Mutually exclusive with `smp` (build.rs panics if both). |
| **devbox** | `devbox` (inherits `release`) | `scripts/build_devbox.sh` / `overlays/devbox/run.sh` | 1.4 MB | **rump only** (no smoltcp, no built-in SSH) | *(deferred — see `devbox-smoltcp`.)* Rump-stack workstation image: NetBSD rump as box 0's default stack, built-in SSH dropped. `--no-default-features`, so smoltcp (and `kernel-tls`/`tls-rsa`/built-in SSH) is compiled out. |
| **devbox-smoltcp** (default devbox) | `release-smp-shared` | `scripts/build_devbox_smoltcp.sh` / `overlays/devbox/run-smoltcp.sh` | 1.7 MB | smoltcp (native) + userspace `/bin/sshd`, **no built-in SSH** | The **default** "develop inside Akuma" image (2026-07-19). Native smoltcp stack for box 0 + real shared-kernel SMP (`SMP=N`); built-in SSH dropped (`userspace-sshd`) so the userspace `/bin/sshd` (herd) over smoltcp is the only sshd. Keeps the default feature set (smoltcp/`kernel-tls` stay in). rump_server work is deferred. |

The `size`/`extreme-size` figures are linked `text` (2026-08-07 / 2026-08-02);
the rest are on-disk ELF size as of 2026-07-18. Rebuild locally for current
numbers — they drift with every feature/dependency change. See
[Measuring an image](#measuring-an-image) for how to get `text`/`data`/`bss`
rather than file size, which for these profiles is the more useful figure.

## Measuring an image

File size is not image size: `release`'s 4.4 MB on disk is 3.3 MB of `text`, and
an unstripped build inflates the file without changing a byte of code.

```bash
llvm-size target/aarch64-unknown-none/size/akuma
```

`llvm-size`/`llvm-nm` are not on `PATH` — they ship in the toolchain:
`~/.rustup/toolchains/nightly-*/lib/rustlib/*/bin/`.

Measured 2026-08-07 at `d3f28d6` (`extreme-size` from the 2026-08-02 build, the
last one that compiled):

| profile | text | data | bss |
|---|---|---|---|
| `release` | 3,311,872 | 173,876 | 146,480 |
| `size` | 881,975 | 32,984 | 83,952 |
| `extreme-size` | 665,487 | 32,456 | 52,000 |

### Attributing bytes to subsystems

`size` and `extreme-size` set `strip = "symbols"`, so byte attribution needs a
build that keeps them. Override per-invocation rather than editing `Cargo.toml`:

```bash
scripts/build_size.sh --config 'profile.size.strip=false'
scripts/symbol_sizes.py target/aarch64-unknown-none/size/akuma --top 30
```

**Read the output as a floor, not an answer.** Both profiles use `lto = true` +
`codegen-units = 1`, which attributes inlined code to the symbol it was inlined
*into*. A small group usually means "inlined into its callers", not "cheap" — the
first run of this measurement reported `akuma-ssh-crypto` at 1.1 KB across 10
symbols, which is impossible for Ed25519 + curve25519 + ChaCha20. The primitives
were in the third-party crates, attributed elsewhere. Cross-check anything
surprising against the raw listing:

```bash
llvm-nm --print-size --size-sort --demangle <image> | tail -50
```

Also watch for single symbols dominating a subsystem:
`curve25519_dalek::…::ED25519_BASEPOINT_TABLE_INNER_DOC_HIDDEN` is 30,720 bytes
of rodata by itself.

## Debug-info variant (opt-in, off by default)

`release-smp-shared-debug` inherits `release-smp-shared` and adds `debug =
true` — full DWARF, for source-level `lldb` debugging against the gdbstub
(see [`scripts/multi-vm.md`](scripts/multi-vm.md) — there is no host `gdb` in
this environment, so `lldb` targeting QEMU's gdbstub is the debugger of
record) instead of raw PC/LR addresses.

```bash
cargo build --profile release-smp-shared-debug --features smp-shared,devbox-smoltcp,no-tests
```

Nothing selects this profile unless asked for by name — `release-smp-shared`
itself is byte-for-byte unaffected. **Only use it on a bug you can already
reproduce reliably.** `lockprobe.py` deliberately symbolicates off `.symtab`
alone and skips DWARF for exactly the opposite reason this profile exists:
measured here, DWARF shifts the *loaded* image (`text`, via `llvm-size`) by
+102,720 bytes (1,331,276 → 1,433,996 at `9a9eb04`) — `data`/`bss` unaffected.
That is enough to move a timing-sensitive race, so reach for this profile only
once you're past the hunting phase and just want lldb to show source lines and
locals instead of addresses.

## What `userspace-sshd` actually does

`userspace-sshd` (used by `devbox` and `devbox-smoltcp`) does **not** compile the
built-in SSH server out, and does not measurably shrink the image. It only sets

```rust
pub const ENABLE_USERSPACE_SSHD: bool = cfg!(feature = "userspace-sshd");  // src/config.rs:775
```

a runtime `const`, which dead-codes the *startup* branch at `src/main.rs:1409`.
The SSH code stays reachable through three other paths:

| site | reference |
|---|---|
| `src/main.rs:1404` | `ssh::init_host_key()` — unconditional under `smoltcp` |
| `src/main.rs:1827` | `ssh::server::stats()` — the main-loop status line |
| `src/shell/mod.rs:24` | `use crate::ssh::protocol::SshChannelStream` |

The last is the real coupling: the in-kernel shell is written against the SSH
channel stream, so dropping SSH means giving the shell another transport.
Compiling SSH out is a `#[cfg]`-gating change, not a feature flip. (The rump
`devbox` *does* lose it, but as a side effect of dropping `smoltcp` — the built-in
server is smoltcp-only, so `#[cfg(feature = "smoltcp")]` takes it.)

### In-kernel SSH vs. userspace sshd, by the numbers

Relevant when deciding what belongs in a 4 MB image. Measured on the `size`
profile:

| | bytes |
|---|---|
| in-kernel SSH, attributed symbols (`akuma::ssh` + `akuma-ssh*`) | 34,853 |
| crypto shared by SSH and `kernel-tls` (`curve25519_dalek`, `ed25519_dalek`, `sha2`, `aes`, …) | 63,580 |
| `bootstrap/bin/sshd`, loadable image (`PT_LOAD` memsz, static musl) | 145,148 |
| `bootstrap/bin/sshd`, on disk | 152,120 |

Two profile-specific consequences:

- **On `size` (keeps `kernel-tls`)** the crypto serves both SSH and outbound
  HTTPS, so it cannot be charged to SSH. Removing the built-in server saves ≤34 KB
  of ~882 KB text.
- **On `extreme-size` (drops `kernel-tls`)** that same crypto is SSH-only, so
  SSH's real cost is much closer to 34 + 62 KB — and correspondingly, the
  `kernel-tls` drop saves less than its own footprint suggests, because SSH keeps
  most of the crypto alive regardless.

Either way, the kernel image shrinking does not mean the *system* footprint
shrinks: the userspace replacement is a 142 KB loadable image plus runtime heap,
thread stacks, page tables, the ext2 disk to hold it, and herd supervision —
against a 4 MB floor whose boot-stack reservation is itself derived from the
linked image size in `linker.ld`. Runtime RSS of `/bin/sshd` is unmeasured; that
is the missing number for deciding.

## Known breakage: `extreme-size` at `d3f28d6`

`scripts/build_extreme_size.sh` fails with 15 × `E0433: failed to resolve: could
not find file_page_cache in the crate root`.

`src/main.rs:45` declares `mod file_page_cache;` behind
`#[cfg(feature = "sc-framebuffer")]`, but the module is called unconditionally
from `src/pmm.rs:791`, `src/fs.rs:128`, `src/vfs/mod.rs:272,276`,
`src/main.rs:1531`, and ~10 sites in `src/exceptions.rs`. `extreme-size` builds
`--no-default-features --features no-tests,smoltcp,extreme`, which excludes
`sc-framebuffer` — so the declaration disappears while every call site remains.
Introduced by `37be208`. `release` and `size` are unaffected (both keep
`sc-framebuffer`).

Fix is either gating the call sites to match or moving the `mod` declaration out
from behind `sc-framebuffer`. For measurement only,
`scripts/build_extreme_size.sh --features sc-framebuffer` compiles — but it adds
the page cache to the image, so it is not a substitute for the fix.

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
- `archive/LINE_COUNT_ANALYSIS.md` — the line-count half of the same investigation
  (2026-08-07): why lines and bytes disagree about what the kernel is spending.
- `scripts/symbol_sizes.py` — per-subsystem byte attribution; `scripts/cloc_akuma.py` — production-vs-test line counts.
