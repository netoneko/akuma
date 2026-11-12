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
| **release** (default) | `release` | `cargo build --release` / `cargo run --release` | smoltcp (native) + userspace `/bin/sshd`; rump not compiled in (`--features rump` makes it available per box) | Day-to-day development image. Full `default` feature set: sound, real shared-kernel SMP (`smp-shared`), `linux-extra-syscalls` (every `sc-*` family). No in-kernel TLS/shell/editor — those were deleted 2026-08-10, see [below](#there-is-no-built-in-ssh-server). |
| **extreme-size** | `extreme-size` (inherits `release`) | `cargo build --profile extreme-size [--no-default-features --features …]` | whatever the feature set selects | **Codegen only**: `opt-level = "z"`, fat LTO, `codegen-units = 1`, stripped. Builds the same kernel as `--release` for a given feature set, just smaller. The 4 MB-floor `extreme` feature behind it was removed 2026-11-11 ([`../archive/EXTREME_SIZE_BUILD.md`](../archive/EXTREME_SIZE_BUILD.md)). |
| **devbox** | `release` | `scripts/build_devbox.sh` / `overlays/devbox/run.sh` | **rump only** (no smoltcp) | *(deferred — see `devbox-smoltcp`.)* Rump-stack workstation image: NetBSD rump as box 0's default stack. `--no-default-features`, so smoltcp is compiled out. |
| **devbox-smoltcp** (default devbox) | `release` | `scripts/build_devbox_smoltcp.sh` / `overlays/devbox/run-smoltcp.sh` | smoltcp (native) + userspace `/bin/sshd` | The **default** "develop inside Akuma" image (2026-07-19). Native smoltcp stack for box 0 + real shared-kernel SMP (`SMP=N`). Keeps the default feature set layered on top (`+= no-tests`, no `--no-default-features`). rump_server work is deferred. |

Binary sizes are intentionally not tabulated here — they drift with every
feature/dependency change. See [Measuring an image](#measuring-an-image) for
how to get current `text`/`data`/`bss` numbers rather than relying on a
snapshot in this doc.

## Measuring an image

File size is not image size: `release`'s 4.4 MB on disk is 3.3 MB of `text`, and
an unstripped build inflates the file without changing a byte of code.

```bash
llvm-size target/aarch64-unknown-none/release/akuma
```

`llvm-size`/`llvm-nm` are not on `PATH` — they ship in the toolchain:
`~/.rustup/toolchains/nightly-*/lib/rustlib/*/bin/`.

Numbers drift with every feature/dependency change — rebuild locally rather
than trusting a snapshot table here; the `size` profile these numbers used to
be measured against was removed 2026-08-10 (see
[Profiles were consolidated](#profiles-were-consolidated-2026-08-10)), so
`release` and `extreme-size` (the same feature set, different codegen) are the
only two worth comparing now.

### Attributing bytes to subsystems

`extreme-size` sets `strip = "symbols"`, so byte attribution needs a build
that keeps symbols. Override per-invocation rather than editing `Cargo.toml`:

```bash
cargo build --profile extreme-size --no-default-features --features no-tests,smoltcp \
    --config 'profile.extreme-size.strip=false'
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
cargo build --profile release-debug --features no-tests
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
every image serves SSH from the userspace `/bin/sshd`, started by herd. The
4 MB `extreme` build was the last holdout and lost it too once
`acceptance/archive/08` was verified passing at 4.0 MB over userspace sshd — see
[`../archive/BUILTIN_SSH_REMOVAL.md`](../archive/BUILTIN_SSH_REMOVAL.md).

The `userspace-sshd` feature (a herd-less startup path where the kernel spawned
`/bin/sshd` itself, `AUTO_START_SSHD`) went with the `extreme` feature on
2026-11-11: it was only ever selected together with it. `AUTO_START_HERD` is
`true` on every build.

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

The 4 MB `extreme` build, same commit, built both ways (this was the
measurement that motivated deleting the built-in server there too, rather than
leaving it as the one build that kept it):

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

Dropping herd as well (the since-removed `config::AUTO_START_SSHD` started
`/bin/sshd` without a supervisor) took 4.5 MB idle free RAM from 968 KB
to **2712 KB**. Full derivation, including the herd-less measurement and two
defects it surfaced: [`../archive/BUILTIN_SSH_REMOVAL.md`](../archive/BUILTIN_SSH_REMOVAL.md).

### Ports

`bootstrap/etc/herd/enabled/sshd.conf` now starts the userspace sshd on
**port 22** (host `2222`), so every profile answers on the documented port —
the userspace `/bin/sshd`, started by herd, is the only server on every
profile. A disk populated before 2026-08-10 still carries `--port 23`;
re-overlay `bootstrap/etc/`.

## Fixed: `extreme-size` build breakage (was broken at `d3f28d6`)

The (since-removed) `scripts/build_extreme_size.sh` used to fail with 17 × `E0433` (15 ×
`file_page_cache`, 2 × `container`). Both were `mod` declarations sitting under a
`#[cfg]` that belonged to another module; fixed on `fix-extreme-size`.

The page-cache one also had a silent second effect: the stray gate had been
un-gating `mod fw_cfg;` since June, so `release` and `size` were carrying
`fw_cfg` unconditionally. Both were corrected — `file_page_cache` became
unconditional and `fw_cfg` went back under `sc-framebuffer`. (`fw_cfg` and the
`sc-framebuffer` feature are both gone as of 2026-08-31 —
[`../archive/FRAMEBUFFER_REMOVED.md`](../archive/FRAMEBUFFER_REMOVED.md); this
paragraph is kept because the page-cache half of the fix still stands.)

Compiling the shared file-page cache into the 4 MB build cost **+9,888 B text
(+1.4 %)**, and it was live there (`[FPCACHE] entries=390 hits=12` on a 64 MB
boot with one `curl`).

Full write-up, plus the `curl` matrix (built-in vs `/bin/curl`, plain vs `-v`,
extreme vs release) and two open defects it surfaced:
[`docs/archive/EXTREME_SIZE_BUILD_FIX.md`](../archive/EXTREME_SIZE_BUILD_FIX.md).

## Feature deltas vs. default `release`

`release` builds with cargo's normal default feature resolution (shared-kernel
SMP and its BKL carve-outs are unconditional, not a feature): `smoltcp`, `sound`,
`linux-extra-syscalls` (the seven `sc-*` families) and `sc-reboot`. The ext2
block cache is not a feature: it is on in every build (the `fs-cache` feature
was folded in 2026-11-11).
There is no `neko`/`kernel-tls`/`tls-rsa` to drop anywhere — those features
were deleted from `Cargo.toml` entirely (commit `bade6ab` for TLS,
`e9d08f1` for the editor) rather than being opt-out. `devbox` passes
`--no-default-features` and explicitly re-adds only what it wants;
`devbox-smoltcp` layers on top of `default` instead:

| Target | Drops vs. default | Adds vs. default |
|---|---|---|
| `devbox` | `smoltcp` | `rump-default` (→ `rump`), `no-tests` |
| `devbox-smoltcp` | — (inherits default set) | `no-tests` |

### Listener backlog and the userspace sshd

Every build's listener backlog is **8** (`MAX_BACKLOG` in
`crates/akuma-net/src/socket.rs`): a listener is a pool of 8 pre-listening
sockets, and a 9th simultaneous *arrival* gets a RST however fast the server
accepts. The `many-sessions` feature that raised it to 32 (and the
`small-sockets` cap `no-tests` used to set) was removed 2026-11-11; the smoltcp
socket table is 256 everywhere.

The kernel no longer carries a mode for sshd. `/bin/sshd` defaults to a process
per session with `max_sessions` = 24, which a burst of connections cannot reach
through an 8-deep backlog (measured 8/8, 12/16, 17/24). Configure sshd for the
image instead:

- connect in smaller bursts, or lower sshd's `max_sessions` to what you need;
- on a low-RAM image, build the cooperative single-process sshd:

```bash
SSHD_FORK_SESSIONS=0 userspace/build.sh --sshd-only
```

- if a server really needs more simultaneous arrivals, raise `MAX_BACKLOG` and
  rebuild the kernel. Each entry is 32 KB of heap per listener, and
  `smoltcp_net::MAX_SOCKETS` must cover a full backlog plus the accepted
  connections.

See [`userspace/sshd/docs/PROCESS_PER_SESSION.md`](../../userspace/sshd/docs/PROCESS_PER_SESSION.md)
and [`docs/archive/EXTREME_SIZE_BUILD.md`](../archive/EXTREME_SIZE_BUILD.md).

`devbox-smoltcp` keeps the full
default feature set and only layers its feature on top, rather than starting
from `--no-default-features` (unlike `devbox`).

## Which one do I want?

- **Developing/debugging the kernel day to day** → `release` (`cargo run --release`).
- **A smaller image of the same kernel** → `--profile extreme-size` with the feature set you want. There is no longer a 4 MB-floor target.
- **Working inside Akuma as a Unix box (self-hosted toolchain, editor, daily use)** → `devbox-smoltcp` (the default devbox: native smoltcp + real SMP; `overlays/devbox/run-smoltcp.sh`). The rump `devbox` is deferred but still boots via `overlays/devbox/run.sh` (needs `RUMP_NIC=1`).
- **A lean amd64 kernel for a bare-metal test box** → `-p akuma --no-default-features --features headless` ([amd64 images](#amd64-images)).
- **Exercising real (shared-kernel) SMP** → nothing to do; it is in `--release`. Set `SMP=N` at run time. See `docs/reference/subsystems/smp-shared.md`.

## amd64 images

The amd64 kernel is the same package as the AArch64 one, built for the other target
(`-p akuma --target x86_64-unknown-none --release`); its feature table is in
[`subsystems/config-flags.md`](subsystems/config-flags.md#amd64-features). Its
device drivers are `crates/akuma-drivers-amd64`, one feature per driver, and the machine
half reaches each class through a registration slot (`amd64/src/drivers.rs`), so a driver
left out takes its probe with it and the class reads as absent.

| Image | Build | What it is |
|---|---|---|
| **default** (bare metal) | `cargo build -p akuma --target x86_64-unknown-none --release` | Every driver (`all-drivers`: `fbcon`, `hda`, `i8042`, `kvm`, `nvme`, `touchpad`, `wifi`, `xhci`) and the boot suite. The image for a machine that is used (HP box, ryzen). |
| **headless** (bare-metal test box) | `... --no-default-features --features headless`; boot under TCG with `NO_DEFAULT_FEATURES=1 FEATURES=headless sh amd64/run.sh` | The machine half's defaults, the boot suite, and only the storage and keyboard drivers (`storage-and-input`: `nvme`, `xhci`, `i8042`). No framebuffer console (the console is the serial port, GRUB's framebuffer is not touched), no splash, no `/dev/fb0`, no sound, no touchpad, no Wi-Fi (`/dev/wifi0`, the rtw89 card), no hypervisor (`/dev/kvm0`, `/dev/kvm`, `/dev/net/tun`), and none of their crates linked. For a test box with a serial line (trashcan, an Intel CPU: the hypervisor is AMD SVM only). |
| **server** (reached over ssh) | `... --no-default-features --features server` | `headless` without the interactive console (the `console` feature) and without the keyboard drivers: storage only (`storage`: `nvme`, `xhci`). The serial line carries the kernel log and init's stdout/stderr, and nothing reads it: no console pump task, no USB keyboard enumeration, init's stdin is EOF, and 301's `SPAWN_FLAG_CONSOLE` (herd's `console = true`) is `ENODEV`. For a machine whose console is ssh. |
| **no-tests** | `--features no-tests` (combines with `headless` or `server`) | Drops the boot suite (`akuma-kernel-tests-amd64` is not linked). |

`headless` keeps what a test box needs: the storage and keyboard drivers by feature
(`storage-and-input`: NVMe, xHCI, which is both the USB keyboard and USB storage, and the i8042),
and in the machine half the serial console, the GRUB-module ramdisk, virtio-blk and the RTL8169
NIC. A build with `--no-default-features` and nothing else has no NVMe, USB or i8042 at all; the
machine half still stops any xHCI controller the firmware left running
(`pci::quiesce_xhci_controllers`). A machine with no UART
(the multiboot2 reference machine) wants `fbcon`: a headless kernel there is silent.

Sizes (`llvm-size -A`, 2026-11-11, after the guest test programs lost their DWARF and Wi-Fi
and KVM became features):

| image | `.text` | `.rodata` | `.bss` |
|---|---:|---:|---:|
| default | 3,053,311 | 21,598,896 | 5,811,096 |
| headless | 2,452,479 | 249,752 | 1,149,848 |
| headless + no-tests | 2,255,855 | 154,576 | 1,118,360 |
| headless + no-tests (2026-11-12) | 2,166,383 | 154,312 | 955,224 |
| server + no-tests (2026-11-12) | 2,175,503 | 152,584 | 955,224 |

The default image's `.rodata` is almost all `akuma-fbcon`'s baked fonts. `server` is not
smaller than `headless` in `.text` (the pump is small, and inlining moved): what it removes is a
running task and the keyboard probes, not bytes.

## Profiles were consolidated (2026-08-10)

Five of the eight profiles were `inherits = "release"` with **no other keys** —
pure duplication that only bought a separate `target/` directory. Removed:
`size`, `release-smp`, `release-smp-shared`, `devbox`. A build target is now
**`--release` plus a feature set**, except the two profiles that carry real
codegen:

| profile | what it actually changes |
|---|---|
| `release` | `panic = "abort"` |
| `extreme-size` | `opt-level = "z"`, `lto`, `codegen-units = 1`, `strip` |
| `release-debug` | `debug = true` (DWARF for lldb) |

Practical cost: artifacts for different feature sets now share
`target/aarch64-unknown-none/release/`, so switching between e.g. plain release
and `--features no-tests` forces a rebuild instead of hitting a warm
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
