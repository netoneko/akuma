# `install.sh`: from a URL to an ssh prompt on Akuma (proposal, 2026-11-04)

**Status: proposal, not started.**

## Goal

One script, published at `https://akuma.sh/install.sh` and as the raw file on GitHub, that a local model (qwen
running under an agent harness on the user's machine) can run unattended to:

1. shallow-clone the repository,
2. build a kernel and a minimal ext2 image: `sshd`, busybox, and the user's public key,
3. boot it locally and print how to ssh in,
4. optionally (`--with-rust`) stage a nightly Rust toolchain on the image, so the guest can build Rust, Akuma itself
   included.

The reader is an agent, not a person, so it gets no prompts, one line of output per stage, every failure names its
stage and fix, and the exit codes are fixed and documented.

## What exists to build on

- **The smallest disk that gets you inside** is already written down, for the tokio kernel on amd64:
  `crates/akuma-tokio-kernel/README.md` § "The smallest disk". It covers `sshd` built `linux-abi`, a static busybox,
  `authorized_keys`, `mke2fs` + `debugfs -w`, and the QEMU `microvm` line. Shanghai (B20) followed it on the metal and
  recorded 14 deviations (`shanghai/RESULT.md`, commit `d4a6c7d9`). Three of them matter here: no `ssh-keygen` on
  that host, `mke2fs -d` and macOS xattrs, and `debugfs` without block device nodes.
- **The nightly toolchain on a disk** is `scripts/populate_disk.sh --with-rust-toolchain`: rustc, cargo, rust-std and
  rust-src from `static.rust-lang.org`, plus clang/lld/gcc/binutils/make/musl-dev via `apk --root`. It runs **inside
  Docker** and installs the aarch64 musl host toolchain. The install script can use neither as-is.
- **A shallow clone without submodules builds what we need.** None of the eight submodules (meow, tinycc, llama.cpp,
  native-cli-ai, the three rump trees, spleen) is a member of the userspace workspace or a dependency of Taoxichuan.
  That stays true only while someone checks it, which is step I0's gate.
- **The pinned toolchain** is `nightly-2026-09-19` (`rust-toolchain.toml`). The toolchain staged in the guest is the
  same date, so the guest can build this tree.
- **akuma.sh** is served from `bootstrap/public/` (`index.html`, `deck/`, `cgi-bin/`).

## Which kernel on which host

| host | kernel | accel | available |
|---|---|---|---|
| Linux x86_64 with `/dev/kvm` | Taoxichuan (amd64 tokio kernel), QEMU `microvm` | KVM | now |
| Linux x86_64 without KVM, Intel Mac | the same | TCG (slow, works) | now |
| Apple Silicon Mac, Linux arm64 | Taoxichuan aarch64, QEMU `virt` | HVF / KVM | after [`B21`](B21_TOKIO_KERNEL_AARCH64.md) A5–A6 |
| Apple Silicon Mac, Linux arm64, before B21 | the full AArch64 Akuma kernel (`cargo build --release`) | HVF / KVM | now, but its minimal disk is not written down (I4) |

Local qwen mostly runs on Apple Silicon, so the arm64 rows matter most. **Recommendation:** ship v1 with the full
AArch64 kernel on arm64 hosts and Taoxichuan on x86_64 hosts, and switch arm64 to Taoxichuan aarch64 when B21's A6
passes. The script chooses by `uname -sm`; `--kernel taoxichuan|akuma` and `--arch` override it.

## Interface

```sh
curl -fsSLO https://akuma.sh/install.sh && sh install.sh up            # clone, build, image, boot, wait for ssh
sh install.sh up --dir ~/akuma --port 2222 --with-rust                  # same, plus the nightly toolchain on the image
sh install.sh check                                                      # preflight only: what is missing and how to get it
sh install.sh ssh 'busybox uname -a'                                     # run a command in the guest
sh install.sh status | down                                              # is it up / stop the VM this script started
sh install.sh up --dry-run                                               # print every step, run none
```

Documented as download-then-run rather than `curl | sh`, so the agent (or the person) can read it first. POSIX `sh`,
no bashisms, no Python requirement.

The last lines of a successful `up` are machine-readable and stable:

```text
AKUMA_READY=1
AKUMA_SSH=ssh -i /home/u/akuma/state/key -p 2222 -o StrictHostKeyChecking=no root@127.0.0.1
AKUMA_STOP=sh /home/u/akuma/install.sh down
```

Exit codes: 0 ok, 2 bad arguments, 10 a host dependency is missing (`check` lists them), 20 clone, 30 build, 40
image, 50 boot, 60 the guest never answered ssh, 70 toolchain staging. Every stage logs to
`$DIR/logs/<stage>.log`, and on failure the script prints that path and the last 20 lines.

## Rules the script keeps

- **No `sudo`, no package installs, no Docker.** `check` prints the install command for each missing tool per OS
  (`brew install qemu e2fsprogs`, `apt install qemu-system-x86 e2fsprogs`, rustup's one-liner). An agent running
  privileged commands it fetched from the internet is the thing to avoid.
- **Everything under `--dir`** (default `~/akuma`): `src/` (the clone), `state/` (key, image, pidfile, console log),
  `logs/`, `cache/` (downloads). `down` and a rerun touch nothing else.
- **Idempotent stages** with stamp files in `state/`. A rerun after a failure resumes at the failed stage, and
  `--clean` starts over.
- **Every download is pinned by sha256.** Busybox, the Alpine packages and the Rust components come from a lock file
  in the repository (`scripts/install/lock.txt`: url, sha256, arch), so the script verifies what it fetched, and a
  changed upstream file is a loud failure instead of a different image.
- **Never touches another VM.** It picks the port (`--port`, default 2222, the next free one if taken), records its
  own QEMU pid, and stops only that pid. Never `pkill qemu`.
- **Readiness is an ssh round-trip**, never a log grep or an open port (the reason is in `CLAUDE.md` § Waiting for a
  VM). A `sh` loop around `ssh ... true` with a deadline does it.

## Stages

| stage | what | notes |
|---|---|---|
| `check` | `git`, `curl`, `cargo`/`rustup`, `qemu-system-<arch>`, `mke2fs` + `debugfs` (Homebrew's `e2fsprogs` path on macOS), `ssh`, `ssh-keygen`, disk space (2 GiB; 8 GiB with `--with-rust`), accel (`/dev/kvm` writable, or HVF on an arm64 Mac) | missing accel is a warning (TCG), not a failure |
| `clone` | `git clone --depth 1 --branch "$REF" --no-recurse-submodules https://github.com/netoneko/akuma src` | `--ref` defaults to the tag the published script was cut from, so the script and the tree agree |
| `build` | the kernel (`cargo build -p taoxichuan --release --target x86_64-unknown-none --features no-tests` on amd64), `sshd` (`linux-abi` for Taoxichuan; the default ABI for the full AArch64 kernel) | rustup installs the pinned nightly and targets from `rust-toolchain.toml` on first use |
| `fetch` | busybox static for the guest arch: Alpine's `busybox-static` package, a tar.gz stream unpacked with `tar` on any host | the kit README downloads busybox.net's x86_64 musl build; Alpine's package serves both arches through one code path (check busybox.net's arm64 coverage before choosing) |
| `key` | `ssh-keygen -t ed25519 -N '' -f state/key` unless `--key PUB` names an existing public key | |
| `image` | stage a root directory (`bin/sshd`, `bin/busybox`, `bin/sh -> busybox`, `etc/sshd/authorized_keys`), then `mke2fs -t ext2 -b 4096 -d root/ state/disk.img SIZE` | one command instead of a `debugfs` script. On macOS strip xattrs first (`xattr -cr root/`); Shanghai hit `mke2fs -d` copying them. The `debugfs -w` recipe stays as the fallback |
| `boot` | QEMU in the background: `microvm` for Taoxichuan (the README's line), `virt` for aarch64; `hostfwd` on the chosen port, console to `state/console.log`, pid to `state/qemu.pid`, the image as a throwaway overlay unless `--persist` | |
| `wait` | the ssh round-trip, 180 s deadline (TCG: 600 s) | |

### `--with-rust`

Adds a `toolchain` stage between `fetch` and `image`:

1. Read the channel manifest for the pinned date
   (`https://static.rust-lang.org/dist/2026-09-19/channel-rust-nightly.toml`); it carries a sha256 for every
   component, so this stage needs no hashes of its own in the lock file.
2. Download `rustc`, `cargo` and `rust-std` for `<arch>-unknown-linux-musl` (the guest runs musl; a glibc rustc does
   not run on Akuma), `rust-std` for `<arch>-unknown-none` (the kernel target), and `rust-src`.
3. Install each into `root/usr/local` by copying the paths its `manifest.in` lists. That is what the component's own
   `install.sh` does, done without bash.
4. **The linker.** Every crate with a `build.rs`, and every proc macro, links a host (musl) binary, which needs `cc`.
   `populate_disk.sh` got one from `apk --root` inside Docker. Here: a pinned set of Alpine packages (gcc, binutils,
   musl-dev, make, and their closure) in `lock.txt`, each unpacked with `tar`. The closure is resolved once, by a
   maintainer script that reads `APKINDEX`, and committed. The install script never resolves dependencies.
5. Image size: 8 GiB sparse.

Gate in the guest: `rustc -V` and `cargo -V` print the pinned nightly; `cargo new hello && cargo run` prints;
stretch, `cargo build -p taoxichuan --release --target x86_64-unknown-none` in the guest (the fixed-point procedure,
`scripts/kvm/taoxichuan/fp_tcg.py`).

## Steps

| step | what | gate |
|---|---|---|
| I0 | **Prove the clone.** A fresh `--depth 1 --no-recurse-submodules` clone builds Taoxichuan and `sshd` on the Mac and on Pop. Add that check as a script so it stays true | both builds from a fresh clone |
| I1 | **`install.sh` for x86_64 hosts**: `check`, `clone`, `build`, `fetch`, `key`, `image` (`mke2fs -d`), `boot`, `wait`, `down`, `status`, `ssh`; the lock file; exit codes | `up` from an empty dir on Pop (KVM) and on the Mac (TCG), then `down`; a second `up` resumes; `--dry-run` matches what `up` did |
| I2 | **`--with-rust`** on x86_64 hosts: the toolchain stage and the Alpine closure | the in-guest gate above |
| I3 | **Publish.** `bootstrap/public/install.sh` (a copy that a check keeps identical to `scripts/install.sh`), its sha256 next to it, and a short `bootstrap/public/install.txt` for the model: what the script does, the flags, the exit codes and the `AKUMA_*` lines | `curl https://akuma.sh/install.sh` returns the committed file |
| I4 | **arm64 hosts on the full AArch64 kernel.** Write the AArch64 version of "the smallest disk" (what the full kernel needs on disk to start `sshd` with no herd and no apk) and prove it on HVF; then the script's `virt` path | `up` on an Apple Silicon Mac from an empty dir |
| I5 | **The stranger test**, as B20 was: a local qwen session given only the URL, no repo knowledge, reaches `AKUMA_READY=1` and runs `busybox uname -a` in the guest. Then again with `--with-rust` and `cargo run` | the transcript, recorded in `docs/archive/` |
| I6 | **arm64 switches to Taoxichuan aarch64**, after B21 A6 | I5 rerun on the Mac |

Docs: a runbook, `docs/runbooks/install-script.md` (ending in **Verify**), a row in `docs/runbooks/README.md`, and
the README's quick start pointing at the URL.

## Risks

- **TCG speed.** An Intel Mac or a Linux host without KVM boots Taoxichuan under TCG. That is fine for ssh, but a
  guest `cargo build` there takes a long time. `check` says so, and `--with-rust` on TCG warns.
- **The pinned nightly ages.** The script, the image and the tree all name `nightly-2026-09-19`. That keeps the guest
  able to build the tree, and it means a toolchain bump touches `lock.txt` too. I0's check should fail if they
  disagree.
- **The Alpine closure is ours to keep.** If a pinned package disappears from the mirror, `--with-rust` breaks until
  someone reruns the maintainer script. Mirror the tarballs as release assets on GitHub to make this hold.
- **`mke2fs -d` portability.** e2fsprogs versions differ between Homebrew and distros; `-d` needs 1.43 or newer. `check`
  tests for it, and the `debugfs` fallback covers older ones.

## Open questions

1. **Hosting.** How does `bootstrap/public/` reach akuma.sh, and should `install.sh` and the lock file's tarballs be
   GitHub release assets instead (versioned, checksummed by GitHub)?
2. **Arm64 v1.** Ship I4 (the full AArch64 kernel on Macs now), or make arm64 hosts wait for B21 and skip I4?
3. **`--with-rust` scope.** Only the pinned nightly, or also `--rust-date` for another nightly (whose channel manifest
   supplies the hashes, but which may not build this tree)?
4. **Default `--ref`.** A tag cut per published script (reproducible), or `main` (newest, may be mid-change)?

## Background

[`crates/akuma-tokio-kernel/README.md`](../crates/akuma-tokio-kernel/README.md),
`shanghai/RESULT.md` (commit `d4a6c7d9`), `scripts/populate_disk.sh`,
[`B21_TOKIO_KERNEL_AARCH64.md`](B21_TOKIO_KERNEL_AARCH64.md),
[`TAOXICHUAN_FIXED_POINT.md`](../docs/archive/TAOXICHUAN_FIXED_POINT.md).
