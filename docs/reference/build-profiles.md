# Build profiles & distributions

At-a-glance comparison of Akuma's build targets. For the exhaustive
per-feature/per-knob breakdown, see
[`reference/subsystems/config-flags.md`](subsystems/config-flags.md) — this
doc answers "which one do I build/run", that one answers "what exactly does
each flag do".

A build target is always **profile + feature set**, selected together by a
`scripts/build_*.sh` script (or `cargo run`/`overlays/devbox/run.sh` for
distros meant to boot). There are only three Cargo profiles now — `release`,
`extreme-size`, and `release-debug` — since the 2026-08-10 consolidation (see
[Profiles were consolidated](#profiles-were-consolidated-2026-08-10) below);
`devbox` and `devbox-smoltcp` both build on plain `release` and are told
apart entirely by feature set, not profile.

## The build targets

| Target | Profile | Build command | Networking | Purpose |
|---|---|---|---|---|
| **release** (default) | `release` | `cargo build --release` / `cargo run --release` | smoltcp (native) + userspace `/bin/sshd`, rump *available* (opt-in per box) | Day-to-day development image. Full `default` feature set: sound, real shared-kernel SMP (`smp-shared`), all `sc-*` syscall families. No in-kernel TLS/shell/editor — those were deleted 2026-08-10, see [below](#there-is-no-built-in-ssh-server). |
| **extreme-size** | `extreme-size` (inherits `release`) | `scripts/build_extreme_size.sh` | smoltcp + userspace `/bin/sshd`, **no HTTPS** | 4 MB RAM floor target. `opt-level = "z"`, LTO, `codegen-units = 1`, tighter stack/heap constants via `cfg(kernel_profile_extreme)`. No in-kernel TLS anywhere in this tree (not extreme-specific — see below); use a userspace HTTPS tool. Compiles again since `fix-extreme-size` — see [Fixed](#fixed-extreme-size-build-breakage-was-broken-at-d3f28d6). |
| **devbox** | `release` | `scripts/build_devbox.sh` / `overlays/devbox/run.sh` | **rump only** (no smoltcp) | *(deferred — see `devbox-smoltcp`.)* Rump-stack workstation image: NetBSD rump as box 0's default stack. `--no-default-features`, so smoltcp is compiled out. |
| **devbox-smoltcp** (default devbox) | `release` | `scripts/build_devbox_smoltcp.sh` / `overlays/devbox/run-smoltcp.sh` | smoltcp (native) + userspace `/bin/sshd` | The **default** "develop inside Akuma" image (2026-07-19). Native smoltcp stack for box 0 + real shared-kernel SMP (`SMP=N`). Keeps the default feature set layered on top (`+= devbox-smoltcp,no-tests`, no `--no-default-features`). rump_server work is deferred. |

Binary sizes are intentionally not tabulated here — they drift with every
feature/dependency change. See [Measuring an image](#measuring-an-image) for
how to get current `text`/`data`/`bss` numbers rather than relying on a
snapshot in this doc.

## Measuring an image

File size is not image size: `release`'s 4.4 MB on disk is 3.3 MB of `text`, and
an unstripped build inflates the file without changing a byte of code.

```bash
llvm-size target/aarch64-unknown-none/extreme-size/akuma
```

`llvm-size`/`llvm-nm` are not on `PATH` — they ship in the toolchain:
`~/.rustup/toolchains/nightly-*/lib/rustlib/*/bin/`.

Numbers drift with every feature/dependency change — rebuild locally rather
than trusting a snapshot table here; the `size` profile these numbers used to
be measured against was removed 2026-08-10 (see
[Profiles were consolidated](#profiles-were-consolidated-2026-08-10)), so
`release` and `extreme-size` are the only two worth comparing now.

### Attributing bytes to subsystems

`extreme-size` sets `strip = "symbols"`, so byte attribution needs a build
that keeps symbols. Override per-invocation rather than editing `Cargo.toml`:

```bash
scripts/build_extreme_size.sh --config 'profile.extreme-size.strip=false'
scripts/symbol_sizes.py target/aarch64-unknown-none/extreme-size/akuma --top 30
```

**Read the output as a floor, not an answer.** `extreme-size` uses `lto = true`
+ `codegen-units = 1`, which attributes inlined code to the symbol it was
inlined *into*. A small group usually means "inlined into its callers", not
"cheap" — the
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

`release-debug` inherits `release` and adds `debug =
true` — full DWARF, for source-level `lldb` debugging against the gdbstub
(see [`scripts/multi-vm.md`](scripts/multi-vm.md) — there is no host `gdb` in
this environment, so `lldb` targeting QEMU's gdbstub is the debugger of
record) instead of raw PC/LR addresses.

```bash
cargo build --profile release-debug --features devbox-smoltcp,no-tests
```

Nothing selects this profile unless asked for by name — `release`
itself is byte-for-byte unaffected. **Only use it on a bug you can already
reproduce reliably.** `lockprobe.py` deliberately symbolicates off `.symtab`
alone and skips DWARF for exactly the opposite reason this profile exists:
measured here, DWARF shifts the *loaded* image (`text`, via `llvm-size`) by
+102,720 bytes (1,331,276 → 1,433,996 at `9a9eb04`) — `data`/`bss` unaffected.
That is enough to move a timing-sensitive race, so reach for this profile only
once you're past the hunting phase and just want lldb to show source lines and
locals instead of addresses.

## There is no built-in SSH server

Not in any profile. `crates/akuma-ssh`, `src/ssh/`, the in-kernel shell, the
in-kernel editor and all kernel cryptography were **deleted** on 2026-08-10;
every image serves SSH from the userspace `/bin/sshd`. The `extreme-size`
profile was the last holdout and lost it too once `acceptance/archive/08` was verified
passing at 4.0 MB over userspace sshd — see
[`../archive/BUILTIN_SSH_REMOVAL.md`](../archive/BUILTIN_SSH_REMOVAL.md).

> **`userspace-sshd` used to be a runtime switch and is no longer a gate at
> all.** It once only set `config::ENABLE_USERSPACE_SSHD`, skipping the *startup*
> branch while the whole SSH-2 server stayed linked in. Today it selects the
> herd-less startup path: `AUTO_START_HERD` off, kernel spawns
> `/bin/sshd --shell /bin/paws` directly (`AUTO_START_SSHD`). That is the
> `extreme-size` default and what makes the 4.0 MB floor work.

### What went with it

The built-in SSH session was the only consumer of the **entire in-kernel
shell**. Rather than leave it gated behind a cfg, the follow-up commit
(`e9d08f1`, "removed in-kernel ssh") deleted it outright: `src/ssh/`,
`src/shell/` (including all of `commands/`), `src/async_fs.rs`, `src/editor/`
(neko), `src/ssh_tests.rs`, `src/shell_tests.rs`, `crates/akuma-ssh`,
`crates/akuma-editor`, plus leaf helpers in `fs`, `vfs`, `kernel_timer` and
`akuma`. None of it exists in the tree anymore on any profile.

Consequence worth knowing: **no image has an in-kernel shell.** SSH sessions
get a userspace shell (`/bin/sh` or `/bin/paws`) via `/bin/sshd`, and there is
no kernel-side fallback if `/bin/sshd` is missing from the disk.

### By the numbers (measured, not attributed)

`extreme-size`, same commit, built both ways (this was the measurement that
motivated deleting the built-in server from `extreme-size` too, rather than
leaving it as the one profile that kept it):

| | built-in SSH | compiled out | delta |
|---|---|---|---|
| `.text` | 591,464 | 443,320 | −148,144 (−25%) |
| `.rodata` | 108,303 | 37,055 | −71,248 (−66%) |
| file on disk | 803,912 | 586,776 | **−217,136 (−27%)** |

Default `--release` (unstripped, no LTO, so the same source costs more):
**4,506,736 → 3,251,312 bytes, −1,255,424 (−27.9%)**.

Free RAM at the 4 MB floor, steady state with disk + herd: **456 KB → 764 KB
(+308 KB)**. That decomposes exactly as 213 KB of image plus 96 KB for the SSH
server's system-thread stack, which is never spawned. The image shrink converts
to free RAM 1:1 — the kernel image occupies PMM pages, so `Code+Stack` loses
precisely the pages `User pages` gains.

Dropping herd as well (possible only now that `config::AUTO_START_SSHD` can
start `/bin/sshd` without a supervisor) takes 4.5 MB idle free RAM from 968 KB
to **2712 KB**. Full derivation, including the herd-less measurement and two
defects it surfaced: [`../archive/BUILTIN_SSH_REMOVAL.md`](../archive/BUILTIN_SSH_REMOVAL.md).

### Ports

`bootstrap/etc/herd/enabled/sshd.conf` now starts the userspace sshd on
**port 22** (host `2222`), so every profile answers on the documented port —
the userspace `/bin/sshd` is the only server on every profile now, started
either by herd or directly by the kernel's `AUTO_START_SSHD` on `extreme-size`
(never both — `AUTO_START_HERD` is `!(extreme && userspace-sshd)`, so there is
no double-bind). A disk populated before 2026-08-10 still carries `--port 23`;
re-overlay `bootstrap/etc/`.

## Fixed: `extreme-size` build breakage (was broken at `d3f28d6`)

`scripts/build_extreme_size.sh` used to fail with 17 × `E0433` (15 ×
`file_page_cache`, 2 × `container`). Both were `mod` declarations sitting under a
`#[cfg]` that belonged to another module; fixed on `fix-extreme-size`.

The page-cache one also had a silent second effect: the stray gate had been
un-gating `mod fw_cfg;` since June, so `release` and `size` were carrying
`fw_cfg` unconditionally. Both are corrected — `file_page_cache` is now
unconditional and `fw_cfg` is back under `sc-framebuffer`.

Compiling the shared file-page cache into `extreme-size` costs **+9,888 B text
(+1.4 %)**, and it is live there (`[FPCACHE] entries=390 hits=12` on a 64 MB
boot with one `curl`).

Full write-up, plus the `curl` matrix (built-in vs `/bin/curl`, plain vs `-v`,
extreme vs release) and two open defects it surfaced:
[`docs/archive/EXTREME_SIZE_BUILD_FIX.md`](../archive/EXTREME_SIZE_BUILD_FIX.md).

## Feature deltas vs. default `release`

`release` builds with cargo's normal default feature resolution: `smp-shared`
(+ its six `no-bkl-*` carve-outs), `smoltcp`, `sound`, `rump`, `fs-cache`,
`many-sessions`, and all eight `sc-*` syscall families (`Cargo.toml:126-142`).
There is no `neko`/`kernel-tls`/`tls-rsa` to drop anywhere — those features
were deleted from `Cargo.toml` entirely (commit `bade6ab` for TLS,
`e9d08f1` for the editor) rather than being opt-out. `devbox`/`extreme-size`
pass `--no-default-features` and explicitly re-add only what they want;
`devbox-smoltcp` layers on top of `default` instead:

| Target | Drops vs. default | Adds vs. default |
|---|---|---|
| `extreme-size` | `smp-shared` (+ `no-bkl-*`), `sound`, `rump`, `fs-cache`, `many-sessions`, all `sc-*` | `no-tests`, `extreme`, `userspace-sshd` |
| `devbox` | `smoltcp`, `smp-shared` (+ `no-bkl-*`), `fs-cache`, `many-sessions` | `devbox` (→ `rump-default` + `userspace-sshd`), `rump-tests`, `no-tests` |
| `devbox-smoltcp` | — (inherits default set) | `devbox-smoltcp` (→ `userspace-sshd` + `smp-shared`, the latter a no-op since it's already default), `no-tests` |

### `many-sessions` and the userspace sshd

`many-sessions` (default since 2026-08-10) deepens the per-listener backlog from
8 to 32 and raises the smoltcp socket table on `small-sockets` builds, so a
server can absorb more than 8 *simultaneous arrivals*. It is the kernel half of
the process-per-session `/bin/sshd`; the userspace half is that binary's own
`fork-sessions` feature, also default-on. See
[`userspace/sshd/docs/PROCESS_PER_SESSION.md`](../../userspace/sshd/docs/PROCESS_PER_SESSION.md).

Cost: ~1 MB of heap per *listening* socket, plus ~44 KB of BSS where the socket
table also grows. Every `--no-default-features` target above therefore drops it
automatically, and `kernel_profile_extreme` overrides the constants back to 8/32
even if the feature is somehow enabled — a belt-and-braces guard so adding
`many-sessions` to `extreme-size`'s feature list later cannot quietly cost a
megabyte per listener against the 4 MB floor.

**If `extreme-size` (or any low-RAM image) shows memory pressure, build sshd
without its half too** — the kernel side is already off there, but the binary is
shared across images and defaults to a process per session:

```bash
SSHD_FORK_SESSIONS=0 userspace/build.sh --sshd-only
```

That reverts `/bin/sshd` to the single-process cooperative executor: one process
serving all sessions, no `fork()` per connection. See
[`docs/runbooks/build-extreme-size.md`](../runbooks/build-extreme-size.md).

`devbox-smoltcp` keeps the full
default feature set and only layers its feature on top, rather than starting
from `--no-default-features` (unlike `extreme-size`/`devbox`).

## Which one do I want?

- **Developing/debugging the kernel day to day** → `release` (`cargo run --release`).
- **Verifying the kernel still fits a 4 MB VM** → `extreme-size`. No in-kernel HTTPS; use a userspace tool if you need TLS.
- **Working inside Akuma as a Unix box (self-hosted toolchain, editor, daily use)** → `devbox-smoltcp` (the default devbox: native smoltcp + real SMP; `overlays/devbox/run-smoltcp.sh`). The rump `devbox` is deferred but still boots via `overlays/devbox/run.sh` (needs `RUMP_NIC=1`).
- **Exercising real (shared-kernel) SMP** → nothing to do; it is in `--release`. Set `SMP=N` at run time. See `docs/reference/subsystems/smp-shared.md`.

## Profiles were consolidated (2026-08-10)

Five of the eight profiles were `inherits = "release"` with **no other keys** —
pure duplication that only bought a separate `target/` directory. Removed:
`size`, `release-smp`, `release-smp-shared`, `devbox`. A build target is now
**`--release` plus a feature set**, except the two profiles that carry real
codegen:

| profile | what it actually changes |
|---|---|
| `release` | `panic = "abort"` |
| `extreme-size` | `opt-level = "z"`, `lto`, `codegen-units = 1`, `strip`, `panic = "immediate-abort"` |
| `release-debug` | `debug = true` (DWARF for lldb) |

Practical cost: artifacts for different feature sets now share
`target/aarch64-unknown-none/release/`, so switching between e.g. plain release
and `--features smp-shared` forces a rebuild instead of hitting a warm
per-profile cache.

## Known inconsistencies

Profile/feature pairing is enforced only by the build scripts, and some of
the four current targets have no acceptance coverage (`acceptance/05`, `10`,
`11`, `13` are the live playbooks — see `CLAUDE.md`). The `kernel_profile_size`
cfg this section used to flag as redundant no longer exists at all: it went
away along with the `size` profile in the same 2026-08-10 consolidation.
Catalogued with evidence (pre-consolidation) in
[`../archive/TRIM_FAT_PROFILES_AND_ACCEPTANCE.md`](../archive/TRIM_FAT_PROFILES_AND_ACCEPTANCE.md).

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
