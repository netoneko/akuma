# Build system

How the kernel is built across the build targets, how the disk image is
staged, and where artifacts land. For the **full feature/env-knob/debug-knob
tables**, see [`subsystems/config-flags.md`](subsystems/config-flags.md) —
this doc does not duplicate them. For a "which one do I build/run"
comparison (sizes, networking, purpose), see
[`build-profiles.md`](build-profiles.md).

> **Stability: A (stable).** The profile/feature pairing model has been
> settled since the size/extreme/devbox split. The recurring lesson:
> **profile = codegen, feature = behaviour** — a profile sets opt-level/LTO,
> and the `scripts/build_*.sh` wrapper pairs it with `--no-default-features`
> plus a specific feature set. Edit one without the other and a build
> silently drops a syscall family or a network stack.

## The profile / feature pairing model

Codegen lives in `[profile.*]` (`Cargo.toml:73-105`); behaviour comes from
`[features]`. There are only three profiles now — `release`, `extreme-size`,
and `release-debug` (a DWARF-debug variant of `release`, not tied to any
wrapper script). The `size` profile and the one-kernel-per-core "multikernel"
(`smp` feature, `release-smp`/`release-smp-shared` profiles) were both removed
2026-08-10 — see `docs/archive/TRIM_FAT_MULTIKERNEL.md` and
`docs/archive/TRIM_FAT_PROFILES_AND_ACCEPTANCE.md`. Real (shared-kernel) SMP,
`smp-shared`, is now in `default` — it is not a separate profile or an opt-in
CLI feature anymore.

| Target | Profile | Codegen | Wrapper script | Feature set |
|---|---|---|---|---|
| `release` | `release` | `panic=abort` | (plain `cargo run --release`) | full `default` (includes `smp-shared`) |
| `extreme-size` | `extreme-size` | inherits `release` + `opt-level=z`, LTO, `codegen-units=1`, `strip`, `panic=immediate-abort` | `scripts/build_extreme_size.sh` (+ `extreme` feature) | `--no-default-features`; `no-tests,smoltcp,extreme,userspace-sshd` |
| `devbox` (rump-only, deferred) | `release` | inherits release | `scripts/build_devbox.sh` / `overlays/devbox/run.sh` | `--no-default-features`; `devbox,sound,no-tests,rump-tests`, all `sc-*` |
| `devbox-smoltcp` (default devbox) | `release` | inherits release | `scripts/build_devbox_smoltcp.sh` / `overlays/devbox/run-smoltcp.sh` | `default` + `devbox-smoltcp,no-tests` (**no** `--no-default-features`) |

`devbox-smoltcp` is the default "develop inside Akuma" image; it layers its
feature on top of plain `release` rather than defining its own profile, so a
target is not always 1:1 with a profile.

The `devbox` feature is the meta-feature `["rump-default", "userspace-sshd"]`
— rump becomes the default stack for box 0. `devbox-smoltcp` is
`["userspace-sshd", "smp-shared"]` — it keeps smoltcp and adds real SMP.
`userspace-sshd` no longer drops a "built-in SSH" on either target: the
in-kernel SSH server (and the shell/editor behind it) was deleted outright on
2026-08-10 (`docs/archive/BUILTIN_SSH_REMOVAL.md`), so every profile's SSH is
now the userspace `/bin/sshd`; the feature only toggles whether herd or
`AUTO_START_SSHD` starts it. `extreme` is **not** a syscall gate: it's the
discriminator `build.rs` reads (via `CARGO_FEATURE_EXTREME`) to emit
`cfg(kernel_profile_extreme)` for tighter `IMAGE_SIZE`/stack knobs (forwarded
to `akuma-exec`/`akuma-ext2`).

When adding a `sc-*` syscall family, **every** `--no-default-features`
wrapper must re-add it or that build silently drops it — see
[`../runbooks/add-syscall-feature.md`](../runbooks/add-syscall-feature.md).

## Output layout

All profiles target `aarch64-unknown-none` and emit a single `akuma` ELF:

```
target/aarch64-unknown-none/<profile>/akuma        # ELF
target/aarch64-unknown-none/<profile>/akuma.bin    # flat Image (rust-objcopy)
```

`scripts/cargo_runner.sh` is the Cargo runner (set in `.cargo/config.toml`);
it `rust-objcopy`s the ELF to a flat binary and enforces a per-profile size
guard before booting QEMU (`scripts/cargo_runner.sh:100-108`): **1 MB**
(`extreme-size`), **4 MB** (everything else, including `release`). Oversize
aborts the boot.

## Disk image lifecycle

1. **Create** — `scripts/create_disk.sh [size_mb]` writes a zero-filled
   raw image (`DISK` env, default `disk.img`) and formats it ext2 (`-b 4096
   -L AKUMA`) via `mkfs.ext2`. macOS needs e2fsprogs from Homebrew.
2. **Populate** — `scripts/populate_disk.sh` mounts the image in a Docker
   container and copies `bootstrap/` into it. Flags (full table in
   [`subsystems/config-flags.md`](subsystems/config-flags.md)):
   - `--bin-only` / `--etc-only` — re-stage a single subtree (fast dev loop).
   - `--with-apk` — stage Alpine busybox world + apk symlinks.
   - `--with-musl-dev` — stage `musl-dev` (crt objects + headers) + extract
     `libtcc1.tar`; disk boots ready for `tcc -static`.
   - `--with-rust-toolchain` — download a nightly musl-host rustc/cargo/rust-src
     + `aarch64-unknown-none` target std into `/usr/local` (for self-hosting).
   - `--overlay DIR` — overlay-only: layer `DIR/.` over an already-populated
     image (used by `overlays/devbox`).
   - `--full-busybox` — generate the full busybox applet symlink set in `/bin`.
3. **Boot** — `scripts/cargo_runner.sh` (the Cargo runner) wraps QEMU. Reads
   the env vars in [`subsystems/config-flags.md`](subsystems/config-flags.md)
   "Env vars": `MEMORY`, `DISK`, `INSTANCE`, `SNAPSHOT`, `RUMP_NIC`,
   `SSH_PORT`/`RUMP_SSH_PORT`, `HVF`, `GDB`, `SMP`. INSTANCE>0 auto-snapshots
   the disk so parallel boots don't corrupt `disk.img`. Size guard runs first.

## Userspace build

`userspace/build.sh` builds the userspace workspace (`userspace/Cargo.toml`,
excluded from the kernel workspace). See
[`../userspace/`](../userspace/) for per-binary docs.

| Flag | Effect |
|---|---|
| `--<name>-only` | Build a single member (e.g. `--meow-only`, `--tcc-only`, `--apk-tools-only`) |
| `--with-forktest` | Also build `userspace/forktest` |
| `--force-rebuild` | `cargo clean -p <member>` first (for members whose build.rs only declares `rerun-if-changed=build.rs`) |

Special cases: `meow` ships size-optimized (rebuilds `core`/`alloc` with
`panic=immediate-abort`, `-Crelocation-model=static`); `tcc` ships only
`libtcc1.tar` (musl sysroot is **not** shipped — install on Akuma with
`apk add musl-dev`); `apk-tools`/`libakuma`/`libakuma-tls`/`nca`
produce no binary in `/bin` (their build.rs deploys directly).

Built binaries land in `bootstrap/bin/` (consumed by `populate_disk.sh`).

## Self-host loop

`scripts/loop_selfhost_kernelbuild.py` SSHes into a running VM (port 2322)
and runs `cargo build --release -p akuma -j1` inside it, looping up to 12×
and bailing after 3 deterministic failures on the same crate. Used to shake
out flaky rustc crashes under the devbox. See
[`../runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md).

## Background

- `archive/OPTIONAL_SMOLTCP.md` — why smoltcp was made optional (the devbox).
- `archive/RUST_TOOLCHAIN_ISSUES.md` — why apk stable rust is used over nightly.
- `archive/AKUMA_SELF_HOSTING.md` — the `fs-cache` feature + self-host design.
