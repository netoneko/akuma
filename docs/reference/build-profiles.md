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

## The build targets

| Target | Profile | Build command | Binary size | Networking | Purpose |
|---|---|---|---|---|---|
| **release** (default) | `release` | `cargo build --release` / `cargo run --release` | 3.1 MB | smoltcp (native) + userspace `/bin/sshd` | Day-to-day development image. Full feature set: editor, sound, TLS (RSA + Ed25519), rump *available* (opt-in per box), all `sc-*` syscall families. |
| **extreme-size** | `extreme-size` (inherits `release`) | `scripts/build_extreme_size.sh` | 578 KB text | smoltcp + **built-in SSH (the only profile that keeps it)**, **no HTTPS** | 4 MB RAM floor target. Same codegen knobs as `size`; the *only* discriminator is the `extreme` feature, since both profiles use `opt-level = "z"`. Drops `kernel-tls` entirely (no in-kernel `curl https://`), `neko`, `tls-rsa`, tighter stack/heap constants via `cfg(kernel_profile_extreme)`. Compiles again since `fix-extreme-size` — see [Fixed](#fixed-extreme-size-build-breakage-was-broken-at-d3f28d6). |
| **release** carries this | `release` | `cargo build --release` (`SMP=N` at run time) | 4.0 MB | smoltcp + userspace `/bin/sshd` | **Real SMP, and the default since 2026-08-10** — `smp-shared` is in the default feature set. One shared kernel across all cores: one PMM/heap/run-queue under real cross-core locks, plus the six `no-bkl-*` carve-outs (see `docs/reference/subsystems/smp-shared.md`). This is what "SMP" means here. |
| **devbox** | `release` | `scripts/build_devbox.sh` / `overlays/devbox/run.sh` | 1.4 MB | **rump only** (no smoltcp, no built-in SSH) | *(deferred — see `devbox-smoltcp`.)* Rump-stack workstation image: NetBSD rump as box 0's default stack, built-in SSH dropped. `--no-default-features`, so smoltcp (and `kernel-tls`/`tls-rsa`/built-in SSH) is compiled out. |
| **devbox-smoltcp** (default devbox) | `release` | `scripts/build_devbox_smoltcp.sh` / `overlays/devbox/run-smoltcp.sh` | 1.7 MB | smoltcp (native) + userspace `/bin/sshd`, **no built-in SSH** | The **default** "develop inside Akuma" image (2026-07-19). Native smoltcp stack for box 0 + real shared-kernel SMP (`SMP=N`); built-in SSH dropped (`userspace-sshd`) so the userspace `/bin/sshd` (herd) over smoltcp is the only sshd. Keeps the default feature set (smoltcp/`kernel-tls` stay in). rump_server work is deferred. |

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

### What goes with it

The built-in SSH session is the only consumer of the **entire in-kernel shell**.
Gating the server alone orphans 118 items on extreme and 122 on
`devbox-smoltcp`, so these are gated on the same cfg: `src/ssh/`, `src/shell/`
(including all of `commands/`), `src/async_fs.rs`, `src/editor/` (neko),
`src/ssh_tests.rs`, `src/shell_tests.rs`, plus leaf helpers in `fs`, `vfs`,
`kernel_timer` and `akuma`. On a build with the boot suite on, only ~8 items go
dead without the gate — the tests are what keep the shell alive there.

Consequence worth knowing: **a non-extreme image has no in-kernel shell.** SSH
sessions get a userspace shell (`/bin/sh`) via `/bin/sshd`, and there is no
kernel-side fallback if `/bin/sshd` is missing from the disk.

### By the numbers (measured, not attributed)

`extreme-size`, same commit, built both ways:

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
the built-in server on `extreme-size`, the userspace one everywhere else. A disk
populated before 2026-08-10 still carries `--port 23`; re-overlay `bootstrap/etc/`.

**Known break on `extreme-size`.** That profile runs the built-in server on 22
*and* still lets herd start `/bin/sshd`, which now also binds 22. The kernel does
not reject the second bind — `[syscall] bind(fd=3, port=22)` succeeds with no
`EADDRINUSE` — and the result is a hung SSH session plus page-pool starvation on
a 4.5 MB box. See [`../archive/BUILTIN_SSH_REMOVAL.md`](../archive/BUILTIN_SSH_REMOVAL.md)
§ "Open: extreme double-binds port 22".

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

`release` builds with cargo's normal default feature resolution
(`neko, smoltcp, kernel-tls, tls-rsa, sound, rump, sc-aio, sc-sysv-ipc,
sc-framebuffer, sc-containers, sc-timerfd, sc-eventfd, sc-pidfd, sc-epoll`).
Every other target passes `--no-default-features` and explicitly re-adds only
what it wants:

| Target | Drops vs. default | Adds vs. default |
|---|---|---|
| `size` | `neko`, `tls-rsa`, `rump`, `sound`, `many-sessions` | `no-tests` |
| `extreme-size` | `neko`, `tls-rsa`, `kernel-tls`, `rump`, `sound`, `many-sessions` | `no-tests`, `extreme` |
| `devbox` | `smoltcp`, `kernel-tls`, `tls-rsa`, `many-sessions` | `devbox` (→ `rump-default` + `userspace-sshd`), `no-tests` |
| `devbox-smoltcp` | — (inherits default set) | `devbox-smoltcp` (→ `userspace-sshd` + `smp-shared`), `no-tests` |

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
from `--no-default-features` (unlike `size`/`extreme`/`devbox`).

## Which one do I want?

- **Developing/debugging the kernel day to day** → `release` (`cargo run --release`).
- **Testing a minimal-RAM boot path without going to the extreme** → `size`.
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

Profile/feature pairing is enforced only by the build scripts, `kernel_profile_size`
is also emitted for `extreme`, the size column is hand-maintained, and four of the
seven targets have no acceptance coverage. Catalogued with evidence in
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
