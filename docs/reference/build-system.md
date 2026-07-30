# Build system

How the kernel is built across the seven build targets, how the disk image is
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

Codegen lives in `[profile.*]` (`Cargo.toml:77-123`); behaviour comes from
`[features]`. Most non-default profiles are paired with a `scripts/build_*.sh`
wrapper; the `--no-default-features` ones re-add a curated set, while the SMP
and `devbox-smoltcp` targets layer their feature on top of `default` instead.

| Target | Profile | Codegen | Wrapper script | Feature set |
|---|---|---|---|---|
| `release` | `release` | `panic=abort` | (plain `cargo run --release`) | full `default` |
| `size` | `size` | inherits release + `opt-level=z`, LTO, `codegen-units=1`, `strip`, `panic=immediate-abort` | `scripts/build_size.sh` | `--no-default-features`; `no-tests,smoltcp,kernel-tls`, all `sc-*` |
| `extreme-size` | `extreme-size` | inherits `size` | `scripts/build_extreme_size.sh` (+ `extreme` feature) | `--no-default-features`; `no-tests,smoltcp,extreme` — **drops every `sc-*` family and `kernel-tls`** |
| `release-smp` | `release-smp` | inherits release | (none — see drift note) | `default` + `smp` (passed on the CLI) |
| `release-smp-shared` | `release-smp-shared` | inherits release | (none — see drift note) | `default` + `smp-shared` (passed on the CLI) |
| `devbox` | `devbox` | inherits release | `scripts/build_devbox.sh` | `--no-default-features`; `devbox,neko,sound,no-tests,rump-tests`, all `sc-*` (drops `smoltcp`,`kernel-tls`,`tls-rsa`) |
| `devbox-smoltcp` | `release-smp-shared` | inherits release | `scripts/build_devbox_smoltcp.sh` | `default` + `devbox-smoltcp,no-tests` (**no** `--no-default-features`) |

`devbox-smoltcp` is the default devbox: it reuses the `release-smp-shared`
profile rather than defining its own, so a target is not always 1:1 with a
profile. `smp` and `smp-shared` are mutually exclusive — `build.rs` panics if
both are set.

The `devbox` feature is the meta-feature `["rump-default", "userspace-sshd"]`
— rump becomes the default stack for box 0 and the built-in smoltcp SSH is
dropped. `devbox-smoltcp` is `["userspace-sshd", "smp-shared"]` — it keeps
smoltcp and drops only the *built-in* SSH, leaving herd's `/bin/sshd` as the
only sshd. `extreme` is **not** a syscall gate: it's the discriminator
`build.rs` reads (via `CARGO_FEATURE_EXTREME`) to emit `cfg(kernel_profile_extreme)`
for tighter `IMAGE_SIZE`/stack knobs (forwarded to `akuma-exec`/`akuma-ext2`).

> **Drift:** `Cargo.toml:100`, `Cargo.toml:110`, `Cargo.toml:146`, and
> `overlays/devbox/README.md:142,211` reference `scripts/build_smp.sh` /
> `scripts/build_smp_shared.sh` / `scripts/run_smp.sh` and a "Phase 2 will
> build with `--no-default-features`" devbox plan. **Both are stale** — none
> of those SMP scripts exist (both SMP builds are invoked directly:
> `cargo build --profile release-smp --features smp` and
> `cargo build --profile release-smp-shared --features smp-shared`), and
> `scripts/build_devbox.sh` + `overlays/devbox/run.sh` already pass
> `--no-default-features` so smoltcp/`kernel-tls`/`tls-rsa` are compiled out
> entirely in the devbox today. See [`subsystems/config-flags.md`](subsystems/config-flags.md)
> "Drift note".

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
guard before booting QEMU (`scripts/cargo_runner.sh:82-105`): **1 MB**
(`size`), **4 MB** (`release-smp`, `release-smp-shared`), **4 MB** (everything
else, including `release`). Oversize aborts the boot.

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
`apk add musl-dev`); `apk-tools`/`libakuma`/`libakuma-tls`/`crush`/`nca`
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
