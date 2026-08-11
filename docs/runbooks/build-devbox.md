# Build the devbox

Build and boot the Akuma devbox: a reproducible "develop inside Akuma" image
where the NetBSD rump stack is the only network stack and the only sshd is the
userspace one. Use this when you want to dogfood the rump path or run real
toolchains inside Akuma.

For architecture, see
[`../reference/subsystems/rump-stack.md`](../reference/subsystems/rump-stack.md)
and [`../reference/subsystems/networking.md`](../reference/subsystems/networking.md).
For all knobs, see
[`../reference/subsystems/config-flags.md`](../reference/subsystems/config-flags.md).

## Prerequisites

- Docker (the image build loop-mounts via a throwaway Alpine container).
- The Akuma repo.
- QEMU (invoked via `scripts/cargo_runner.sh`).
- Apple Silicon host recommended (HVF); `HVF=0` forces TCG.

## 1. Build the image

```bash
overlays/devbox/bootstrap.sh
```

This builds herd + sshd, creates a `DEVBOX_DISK_MB` (default 1024) ext2 image,
wipes `/etc`, overlays `overlays/devbox/rootfs/`, lays down full busybox
applets, stages the TLS CA bundle, installs real `git` via apk, and installs
the Rust + C toolchain via apk.

Env knobs (all optional):

| Var | Default | Effect |
|---|---|---|
| `DEVBOX_DISK` | `devbox.img` | Output image path. |
| `DEVBOX_DISK_MB` | `6144` | Image size in MB (bumped from 1024 for apk stable + nightly + C toolchain). |
| `DEVBOX_BUILD_USERSPACE` | `true` | `false` reuses existing `bootstrap/bin`. |
| `DEVBOX_CA_CERTS` | `true` | `false` skips the Mozilla CA bundle (offline builds). |
| `DEVBOX_GIT` | `true` | `false` keeps `git -> scratch` instead of apk git. |
| `DEVBOX_RUST_TOOLCHAIN` | `true` | `false` skips the apk rust + cargo + C toolchain. |
| `DEVBOX_NIGHTLY_RUST` | `true` | `false` skips the nightly toolchain (static.rust-lang.org) under `/usr/local`. |
| `DEVBOX_SOUNDTRACK` | `false` | `true` copies `bootstrap/music`. |

### What lands on the image

- `/bin/rump_server` — box 0's NetBSD stack.
- `/bin/herd` — supervisor (starts sshd).
- `/bin/sshd` — userspace sshd (the only sshd).
- `/bin/busybox` + full applet symlinks — the shell + coreutils.
- `/usr/bin/git` (apk) — real git; `/bin/git` → `/usr/bin/git`.
- `/usr/bin/rustc`, `/usr/bin/cargo` + clang/lld/gcc/binutils/make/musl-dev (apk stable).
- `/usr/local/bin/rustc`, `/usr/local/bin/cargo` (nightly, static.rust-lang.org) + rust-std for
  `aarch64-unknown-linux-musl` and `aarch64-unknown-none` + `rust-src`.
- `/etc/ssl/certs/ca-certificates.crt` — Mozilla roots, for userspace `curl https`.
- `/etc` comes **only** from `overlays/devbox/rootfs/`.

> **Toolchain note (current truth, updated 2026-08-11):** the devbox ships
> **both** toolchains side by side. `bootstrap.sh` step 7 installs **apk
> stable** `rust`/`cargo` into `/usr` with `PATH=/usr/bin` set via
> `/etc/profile.d/rust.sh`. Step 7b installs **nightly**, downloaded straight
> from [`static.rust-lang.org/dist`](https://static.rust-lang.org/dist), into
> `/usr/local` — default on (`DEVBOX_NIGHTLY_RUST=true`). Step 7b does **not**
> touch `PATH`, so bare `cargo`/`rustc` still resolve to apk stable; invoke
> `/usr/local/bin/cargo` explicitly for nightly.
>
> Nightly `cargo` used to die instantly under HVF (`EC=0x0` at a constant
> `ELR`) — **root-caused and fixed 2026-08-06**: the trap was OpenSSL's
> `OPENSSL_cpuid_setup` armcaps probe executing `SM3SS1` (FEAT_SM3), which
> Apple Silicon's HVF lacks; the probe expects `SIGILL` to detect the missing
> feature, but the kernel's `EC=0x0` handler hard-killed the process instead
> of delivering it. Fixed by delivering `SIGILL` via `try_deliver_signal` in
> `src/exceptions.rs`. Re-verified 2026-08-11 on devbox-smoltcp: `cargo new`,
> `cargo build`, and running the resulting binary all completed cleanly. Full
> writeup: [`../archive/NIGHTLY_CARGO_HVF_SIGILL.md`](../archive/NIGHTLY_CARGO_HVF_SIGILL.md);
> also covered in [`selfhost-kernel-build.md`](selfhost-kernel-build.md) §6.
> (`docs/archive/RUST_TOOLCHAIN_ISSUES.md` is the original, now-superseded
> investigation — kept verbatim per repo convention, not the current state.)

## 2. Boot the kernel

```bash
overlays/devbox/run.sh
```

Equivalent build-only: `scripts/build_devbox.sh`.

`run.sh` sets `RUMP_NIC=1`, `MEMORY=4096`, unsets `SMP`, and runs:

```
cargo run --release --no-default-features --features \
  devbox,sound,no-tests,rump-tests,sc-aio,sc-sysv-ipc,sc-framebuffer,sc-containers,\
  sc-timerfd,sc-eventfd,sc-pidfd,sc-epoll
```

(There is no `devbox` Cargo *profile* — only `release`, `extreme-size`, and
`release-debug` exist; `devbox` is a feature layered on plain `release`.)

`--no-default-features` compiles **smoltcp out entirely**. Rump is the only
stack. The built-in SSH server, `kernel-tls`, and `tls-rsa` aren't part of
this trade anymore — they were deleted from the whole tree (not just this
profile) on 2026-08-10; see `docs/archive/BUILTIN_SSH_REMOVAL.md`.
`run.sh` env knobs: `DEVBOX_DISK`, `DEVBOX_MEMORY` (default 4096),
`RUMP_SSH_PORT` (default 2223).

## 3. Connect

```bash
ssh -o StrictHostKeyChecking=no -p 2223 root@localhost
```

`/etc/sshd/sshd.conf` sets `disable_key_verification = true` (local dev VM), so
no key setup is needed. The host key is auto-generated on first boot at
`/etc/sshd/id_ed25519`; after a rebuild run
`ssh-keygen -R "[localhost]:2223"`.

## Verify

In the QEMU serial log (stdout), watch for these markers **in order**:

```
[RUMP-SP] rump-default: box 0 stack=rump; spawning /bin/rump_server (--net --fd 3)...
[Main] Built-in SSH server disabled (ENABLE_USERSPACE_SSHD=true)
[RUMP-SP] box=0 proxy ready              # box 0's rump stack up, DHCP done (~5s)
[herd] Started sshd
[SSHD] Listening on ...
```

Then the SSH connection lands in a shell, and the log shows
`[RUMP-SP] accept -> box_fd=...` + `sendto`/`recvfrom` for the session — i.e.
the SSH session is running over rump with no box.

If you do NOT see `box 0 stack=rump` but instead see `no NIC1 (/dev/net/tap0)
— box 0 stays on native stack`, you forgot `RUMP_NIC=1` (use `run.sh`, which
sets it, or `export RUMP_NIC=1`).

## In-VM quick checks

```bash
cat /var/log/box/0/rump_server.log    # the rump server's own log
busybox ifconfig                       # virt0 should have 10.0.2.15 (DHCP)
curl https://ifconfig.me               # HTTPS over rump (verifies CA bundle)
```

## Common build failures

- **`bootstrap/bin/<binary> missing`** — `bootstrap.sh` checks for
  `rump_server herd sshd busybox sh`. Run `userspace/build.sh` first or set
  `DEVBOX_BUILD_USERSPACE=true`.
- **Docker mount/copy errors** — ensure Docker is running and the repo path is
  accessible.
- **Port 2223 busy** — `pkill -9 qemu-system-aarch64`, or set
  `RUMP_SSH_PORT=2224`.

## Background

- `overlays/devbox/README.md` — the canonical (but partly stale) overlay doc.
- `archive/OPTIONAL_SMOLTCP.md` — why/how smoltcp was compiled out.
- `archive/RUST_TOOLCHAIN_ISSUES.md` — the original nightly-cargo-crash
  investigation, superseded by `archive/NIGHTLY_CARGO_HVF_SIGILL.md` (root
  cause + fix, 2026-08-06) — both toolchains ship together now, see the
  toolchain note above.
- `selfhost-kernel-build.md` §6 — the fix in kernel-source terms
  (`src/exceptions.rs`, `try_deliver_signal`).
