# devbox-firecracker-aws

Akuma as a Firecracker microVM on an **AWS bare-metal host**, and the packaging
that gets the root filesystem there.

The sibling of `overlays/devbox-firecracker` (Firecracker on a local Lima VM),
`overlays/devbox-smoltcp` (QEMU, smoltcp, real SMP) and `overlays/devbox` (QEMU,
rump).

- **Infrastructure** lives in a separate repo, `akuma-terraform`: the metal
  instance, the tap/DHCP/NAT, the Firecracker install, the FDT dump.
- **This overlay** owns the part that belongs in the Akuma tree: turning
  `bootstrap/` + `overlays/devbox/rootfs` into something a remote host can turn
  into an ext2 image.
- Progress and decisions: `docs/archive/AKUMA_FIRECRACKER_TERRAFORM.md`
- Measured platform constants: `docs/reference/firecracker/fdt/`

## Why a separate overlay from `devbox-firecracker`

`devbox-firecracker` assumes the KVM host is a Lima VM on the same machine, so
the image is `limactl copy`'d in and the disk is built locally with
`scripts/populate_disk.sh`. Neither holds on AWS:

- **`scripts/populate_disk.sh` needs Docker**, because macOS cannot loop-mount
  ext2 at all. The AWS host is Linux and runs as root, so `mkfs.ext2` and
  `mount -o loop` work directly and the container is pure overhead.
- **The image has to cross the internet.** `bootstrap/` is 1.2 GB, of which
  987 MB is demo payload (`models/`, `music/`, `archives/`) that no bootable
  image copies. Shipping the 175 MB that matters — and, over a registry, only the
  layers that changed — is a different problem from copying a file into a local VM.

## Files

| File | What |
|---|---|
| `Dockerfile` | the root filesystem as an OCI image: `FROM scratch`, one `COPY` per top-level tree so `bin/` (163 MB, near-static) becomes its own cached layer |
| `build-rootfs-image.sh` | the canonical merge, and the only place that decides which `/etc` wins. Emits an OCI image, a `tar.xz`, or a staged directory |

## What the merge stages beyond the file trees

`bootstrap/` is a set of file trees; a *bootable, usable* image needs four things
that are not in any of them. Each is ported from the step of
`overlays/devbox/bootstrap.sh` that does the same job for the QEMU disk — that
script is the reference, and divergence between the two is a bug.

| Step | What | Devbox equivalent | Skip with |
|---|---|---|---|
| 1b | **busybox applet symlinks** | step 4 | — |
| 1c | apk database, **real `git`**, C toolchain (`clang lld gcc binutils make musl-dev`) | steps 5-7 | `DEVBOX_APK_PACKAGES=false` |
| 1d | **nightly Rust** into `/usr/local` | step 7b | `DEVBOX_NIGHTLY_RUST=false` |
| 1e | cargo network policy | step 7c | — |

`DEVBOX_RUST_TOOLCHAIN=false` drops the C toolchain; `DEVBOX_STABLE_RUST=true`
adds apk's `rust`/`cargo` alongside nightly.

**The applet links are the load-bearing one.** `bootstrap/bin` ships the busybox
*binary* and no applet links at all — on a normal system `ls`, `ps` and `cat` are
each a symlink to it, and the devbox creates those inside the mounted ext2 image
rather than in `bootstrap/bin`. An image built without that step boots, accepts
ssh, and answers **every** command with "not found", which reads as an empty
disk rather than as missing symlinks.

The list comes from `busybox --list` (305 applets), never from a hand-written
one: the static fallback inherited from `bootstrap.sh` omits `ls`. The binary is
aarch64, so on a foreign-arch workstation the builder runs it under docker to
read the list, and only falls back — loudly — if that is unavailable.

Existing non-symlink binaries are never clobbered, so `vi`, `top`, `git`, `tcc`,
`meow` and `curl` keep the real thing.

## Two gaps this image still has

**No wall clock.** Firecracker exposes no RTC on aarch64 and the kernel has no
`clock_settime`, so the guest boots at epoch 0 and every TLS certificate reads as
`certificate is not yet valid`. In-guest `git clone` over HTTPS and `cargo fetch`
are therefore impossible regardless of connectivity —
[`../../docs/archive/MISSING_NTP_SYSCALLS.md`](../../docs/archive/MISSING_NTP_SYSCALLS.md).
Work around it by vendoring (`cargo vendor` on the host, staged into the image
with a `[source.crates-io]` replacement) — that path is verified: a full kernel
build runs offline in the guest.

**One herd service only.** Enabling a second service alongside `sshd` starves
sshd — it binds its port and never sends a banner —
[`../../docs/archive/HTTPD_STARVATION.md`](../../docs/archive/HTTPD_STARVATION.md).
This is why `httpd.conf` ships in `/etc/herd/available/`, not `enabled/`.

## Usage

```bash
# OCI image for a registry. NO default registry, by design.
overlays/devbox-firecracker-aws/build-rootfs-image.sh \
  --registry 1234.dkr.ecr.ap-northeast-1.amazonaws.com/netoneko/akuma --push

# No registry at all: a tarball to scp.
overlays/devbox-firecracker-aws/build-rootfs-image.sh --tarball

# Just look at what the merge produces.
overlays/devbox-firecracker-aws/build-rootfs-image.sh --stage-only /tmp/root
```

`--help` lists `--profile devbox|full`, `--with-box`, `--pubkey`.

From the `akuma-terraform` side, `bin/publish-rootfs.sh` and
`bin/package-rootfs.sh` are thin wrappers over this script — they supply the
registry from terraform's output and do the ECR login. The merge is not
duplicated there.

## Two deliberate refusals

**No default registry.** `--registry` is required for any image build. Without
it the script errors instead of tagging for Docker Hub, and no account id is
committed to this tree. `AKUMA_REGISTRY` works if you prefer an env var.

**No RSA key.** The script rejects anything that is not `ssh-ed25519` for
`/etc/sshd/authorized_keys`. `userspace/sshd` (`userspace/sshd/src/keys.rs`)
implements exactly one key type, so an RSA key there is unparseable rather than
merely weaker — and since `overlays/devbox/rootfs/etc/sshd/sshd.conf` sets
`disable_key_verification = false`, the failure would otherwise surface as an
unexplained auth rejection inside the guest.

## Profiles

| Profile | `/etc` from | Also includes |
|---|---|---|
| `devbox` (default) | `overlays/devbox/rootfs/etc` **only** | `bootstrap/{bin,usr}` |
| `full` | `bootstrap/etc` | `bootstrap/{bin,usr,root,public,tmp}` |

`devbox` taking `/etc` from the overlay alone mirrors
`overlays/devbox/bootstrap.sh` step 3, which wipes the base `/etc` before
overlaying, so nothing from `bootstrap/etc` is inherited unreviewed.

`models/`, `music/` and `archives/` are never included by either profile.

## Consuming it on the host

`akuma-terraform`'s `files/62-ecr-image.sh` (registry) or `files/60-akuma-image.sh`
(tarball). Both `mkfs.ext2 -b 4096 -L AKUMA`, extract, `e2fsck -fn`, and write a
Firecracker config beside the image.

The registry path uses **`docker save` plus manual layer extraction, not
`docker create` + `docker export`**: export goes through a container, and Docker
populates `/etc/resolv.conf`, `/etc/hosts` and `/etc/hostname` in the container
layer — which would silently shadow the `resolv.conf` this overlay ships and that
the guest's DNS depends on. A `FROM scratch` image with only `COPY` layers has no
whiteouts, so layer order alone reproduces it exactly.

## Known limits

- **`--vcpus 1` only**, still. The GIC redistributor base is
  `0x3fff0000 - vcpu_count * 0x20000` — now confirmed by measurement at 1/2/4/8
  vCPUs (`docs/reference/firecracker/fdt/`) — and Akuma's device map is a
  compile-time table pinned to one vCPU. The FDT-derived replacement is
  `proposals/FIRECRACKER_PORT.md` §5.
- **One drive.** A second small ext2 image, for carrying a kernel Akuma built
  in-VM back out to the host, is designed and deferred:
  `crates/akuma-virtio/src/block.rs` keeps a single global `BLOCK_DEVICE` and
  binds the first virtio-blk it probes, so a second drive would be walked past.
  See `docs/archive/AKUMA_FIRECRACKER_TERRAFORM.md` §6.
- **The kernel is built on the host**, not packaged here. `akuma-terraform`'s
  `bin/push-akuma.sh` rsyncs the source (excluding `bootstrap/`) and
  `overlays/devbox-firecracker/build.sh` runs there — natively, on 64 cores.
