# simple-kernel-devbox

The most minimal Akuma rootfs image that is immediately useful after boot:
**sshd + busybox + nightly rust/cargo** (plus the C toolchain cargo needs to
link). It is the rootfs half of
[`bootstrap/public/kernel-bootstrap-with-local-ai/index.md`](../../bootstrap/public/kernel-bootstrap-with-local-ai/index.md)
— the guide a local AI agent follows to **build the kernel from this checkout
and boot it** on QEMU or Firecracker. Build the kernel, build this image, boot,
ssh in.

The sibling of `overlays/devbox` (rump) and `overlays/devbox-smoltcp` — a strict
subset of the smoltcp devbox with three differences:

| | devbox-smoltcp | **simple-kernel-devbox** |
|---|---|---|
| purpose | human dogfooding workstation | throwaway target for an AI-built kernel |
| `/bin` | full `bootstrap/bin` set | exactly `herd sshd busybox apk sh` |
| image | 6 GB (`devbox.img`) | 2.5 GB default (`simple-kernel-devbox.img`) |
| rust toolchain | nightly (+ optional apk stable) | nightly only |
| git | apk `git` | none (`apk add git` in-guest if needed) |
| pubkey | staged by hand afterwards | **staged automatically** at build time |

## Quick start

```bash
# 0. Host deps: docker, qemu, e2fsprogs (mac: brew install ...), rustup nightly
#    (rust-toolchain.toml is auto-installed by rustup on first cargo command).

# 1. Build the kernel from this checkout (userspace sshd via herd + SMP):
scripts/build_devbox_smoltcp.sh

# 2. Build the minimal rootfs (needs Docker; auto-stages ~/.ssh/id_ed25519.pub):
overlays/simple-kernel-devbox/bootstrap.sh

# 3. Boot (forwards host :2222 -> guest :22):
overlays/simple-kernel-devbox/run.sh

# 4. Connect:
ssh -o StrictHostKeyChecking=no -p 2222 root@localhost
# first boot generates the host key; after an image REBUILD:
ssh-keygen -R "[localhost]:2222"
```

## What lands on the image

- `/bin/herd` — supervisor; starts the (only) sshd at boot.
- `/bin/sshd` — userspace sshd, publickey auth only (ed25519; the default
  build ignores `disable_key_verification`).
- `/bin/busybox` — full applet symlink set (`sh`, `vi`, `ps`, ...).
- `/bin/apk` + `/etc/apk/{arch,repositories,keys}` — in-guest `apk add` works
  (keys are committed in this overlay's rootfs).
- `/usr/local/bin/{rustc,cargo}` — nightly from static.rust-lang.org
  (musl host), plus `rust-std` for `aarch64-unknown-none` and `rust-src`, so
  in-guest `-Z build-std` kernel builds work. First on `PATH` (the kernel's
  `DEFAULT_ENV` puts `/usr/local` ahead of `/usr`).
- `clang/lld/gcc/binutils/make/musl-dev` — the C toolchain cargo cannot link
  without.
- `/etc/ssl/certs/ca-certificates.crt` — CA roots so `cargo fetch`/`curl https`
  verify peers.
- `/etc` comes **only** from this overlay's `rootfs/` (herd sshd conf, sshd
  conf, resolv.conf, apk config, `/root/WELCOME.txt`).

NOT on the image (on purpose): `rump_server` (the smoltcp kernel never spawns
it), `git`, `curl`, `tcc`, demo content, the repo clone.

## Knobs (`bootstrap.sh`)

| Var | Default | Meaning |
|-----|---------|---------|
| `SIMPLE_KERNEL_DISK` | `simple-kernel-devbox.img` | output image |
| `SIMPLE_KERNEL_DISK_MB` | `2560` | image size (toolchain ~1.6 GB + build headroom) |
| `SIMPLE_KERNEL_BUILD_USERSPACE` | `true` | rebuild herd+sshd; `false` reuses `bootstrap/bin` |
| `SIMPLE_KERNEL_SSH_PUBKEY` | `~/.ssh/id_ed25519.pub` | pubkey staged into `/etc/sshd/authorized_keys` (ed25519 only) |
| `SIMPLE_KERNEL_CA_CERTS` | `true` | TLS CA bundle (skip for offline builds) |
| `SIMPLE_KERNEL_RUST_TOOLCHAIN` | `true` | C toolchain; `false` leaves cargo unable to link |
| `SIMPLE_KERNEL_NIGHTLY_RUST` | `true` | nightly → `/usr/local`; `false` leaves no Rust |

Knobs (`run.sh`): `SIMPLE_KERNEL_DISK`, `MEMORY` (4096), `SMP` (2),
`INSTANCE` (0..98 — shifts host ports by 100·N and snapshots the disk),
`GDB`/`GDB_WAIT`. There is no custom-ssh-port knob — `cargo_runner.sh` fixes
`2222 + 100·INSTANCE`.

## Firecracker

Same image, Firecracker loader. Build the FC-shaped kernel and pass the disk:

```bash
FC_FEATURES=devbox-smoltcp,no-tests overlays/devbox-firecracker/build.sh   # -> akuma-fc.bin
FC_DISK=$PWD/simple-kernel-devbox.img overlays/devbox-firecracker/run.sh   # mac: via Lima (default)
FC_DISK=$PWD/simple-kernel-devbox.img overlays/devbox-firecracker/run.sh --local   # KVM metal
```

Firecracker needs a **KVM host that matches the kernel ISA** — here, AArch64:
an AWS `m6g.metal`, an ARM Linux box with `/dev/kvm`, or Lima nested-virt on
Apple Silicon (`overlays/devbox-firecracker/host-setup.sh` first). SSH lands on host port **4444** there, not 2222. See
[`docs/runbooks/run-on-firecracker.md`](../../docs/runbooks/run-on-firecracker.md).

## Verification

1. `overlays/simple-kernel-devbox/bootstrap.sh` completes; image exists.
2. `overlays/simple-kernel-devbox/run.sh`; in the serial log, in order:
   - `[Main] Built-in SSH server disabled` (no such server exists anymore, but
     the marker confirms the `userspace-sshd` feature applied)
   - `Network Initialization Done` (smoltcp up, DHCP lease `10.0.2.15`)
   - `[herd] Started sshd` and `[SSHD] Listening on ...`
3. `ssh -o StrictHostKeyChecking=no -p 2222 root@localhost` → busybox shell.
4. In-guest: `uname -a` (your kernel), `rustc --version` (nightly),
   `cargo new hello && cargo run --release` (full in-guest build).

## Related

- Guide for AI agents: `bootstrap/public/kernel-bootstrap-with-local-ai/index.md`
- Full devbox (rump): `overlays/devbox/` + `docs/runbooks/build-devbox.md`
- smoltcp devbox: `overlays/devbox-smoltcp/`, `scripts/build_devbox_smoltcp.sh`
- Firecracker: `overlays/devbox-firecracker/`
