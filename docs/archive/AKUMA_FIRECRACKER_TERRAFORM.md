# AWS metal host for Firecracker: the terraform project, and what it decided

**Date opened:** 2026-08-21
**Project:** `../akuma-terraform/` (its own git repo,
`github.com/netoneko/akuma-terraform`)
**Status:** **applied and delivering.** `m6g.metal` running in `ap-northeast-1`,
Firecracker v1.16.1 under KVM in VHE mode, Alpine microVM booting, and the FDT
dumped at 1/2/4/8 vCPUs. The artifacts are in `docs/reference/firecracker/fdt/`.
**Design it serves:** `proposals/FIRECRACKER_PORT.md` §7 (AWS metal) and §5
(FDT-derived device map)
**Prior art:** `docs/archive/AKUMA_FIRECRACKER_KVM.md` (the local Lima boot),
`overlays/devbox-firecracker/`

This is the progress log for standing up a real aarch64 KVM host on AWS. It exists
because `FIRECRACKER_PORT.md` §7 said "the prep worth doing now is the part that
makes the metal hours short", and this is that prep — plus the decisions that
turned out to be forced rather than chosen.

---

## 1. What the project is for

One deliverable above all others: **the FDT Firecracker actually hands a guest.**

Everything Akuma currently knows about Firecracker's aarch64 memory map was read
out of Firecracker's *source* (`FIRECRACKER_PORT.md` §2), and the whole
`platform-firecracker` device map is a compile-time table in `src/platform.rs`
pinned to one vCPU as a consequence. §5 concluded the map has to become
FDT-derived, because the GIC redistributor base is
`0x3FFF_0000 - vcpu_count * 0x2_0000` and no build-time constant can express a
runtime `SMP=N`.

The FDT is the runtime description of that same machine. Dumping it per vCPU count
turns the formula into measured evidence and gives the refactor something to be
tested against.

Secondary: a 64-core aarch64 Linux box that can build the kernel natively and boot
it under Firecracker, with the tap/DHCP/NAT the microVM needs already up.

## 2. Decisions, and which were forced

| Decision | Outcome | Forced? |
|---|---|---|
| `.metal` instance | required | **Forced.** Non-metal EC2 instances are guests, and EC2 offers nested virt on no architecture, so `/dev/kvm` exists nowhere else. |
| Region | `ap-northeast-1` (Tokyo) | Chosen. Japan was the requirement; Tokyo is $0.0064/hr cheaper than Osaka for m6g.metal and carries more families. |
| Instance type | `m6g.metal`, $3.168/hr on-demand | Chosen from a measured price sweep (§3). It is the exact row in Firecracker's tested-platform table. |
| On-demand, not spot | on-demand | Chosen, at the user's direction: spot can be reclaimed mid-debug, and `instance_market_options` also blocks stop/start. |
| Host OS | Ubuntu 24.04 arm64 | Chosen. `overlays/devbox-firecracker/guest-setup.sh` is apt-based; keeping the family means the overlay runs unmodified with `--local`. |
| Firecracker version | `v1.16.1`, pinned | **Forced.** `src/platform.rs`'s constants were read from that release and the map has moved between versions. |
| No IAM roles / no SSM | SSH with a key pair | **Forced.** These credentials cannot create an IAM role, and SSM Session Manager needs an instance profile (§5b). |
| Root image built on the host | natively, from an OCI image or a tarball | Chosen (§5). |
| ECR, no instance profile | token minted on the workstation | **Forced** by the available permissions (§5b) — and a better default anyway. |
| One drive only | root ext2 | **Forced by the kernel** (§6). |

## 3. Price sweep, 2026-08-21

aarch64 `.metal`, on-demand, from the AWS price list API:

| Type | Tokyo | Osaka | vCPU / RAM | CPU |
|---|---|---|---|---|
| **m6g.metal** | **$3.1680** | $3.1744 | 64 / 256 GiB | Graviton2 |
| m7g.metal | $3.3728 | $3.3728 | 64 / 256 GiB | Graviton3 |
| c6g.metal | $2.7392 | $2.7520 | 64 / 128 GiB | Graviton2 |
| c7g.metal | $2.9107 | $2.9104 | 64 / 128 GiB | Graviton3 |
| a1.metal | $0.5140 | n/a | 16 / 32 GiB | Graviton1 |

Spot, for the record, was much cheaper — Osaka `c6gd.metal` $0.3136/hr,
`m6g.metal` $0.3174/hr, Tokyo `r6g.metal` $0.3891/hr — but spot is not used, see
§2.

`m6g.metal` was chosen over the cheaper `c6g.metal` because it is literally the
entry in Firecracker's tested-platform table ("Graviton 2 — m6g.metal"), so a
failure on it is an Akuma bug rather than an unsupported-host question. 256 GiB
also removes memory as a variable for the mmap-heavy self-host workloads.

### a1.metal: KVM yes, Firecracker unvalidated

Worth recording precisely, because it is a 6x cost difference.

- **KVM: yes.** `.metal` means bare metal, so the OS owns EL2; Graviton1
  (Cortex-A72) has the virtualization extensions and KVM runs there in nVHE mode.
- **Firecracker: not on the tested list.** Firecracker's supported-platform table
  names Graviton 2 (m6g.metal), Graviton 3 (m7g.metal) and Graviton 4
  (m8g.metal-*). Graviton1 and the a1 family are absent.

So it is plausible and unproven. It is also a ~$0.10 experiment — the bootstrap
either reaches `state/DONE` or it does not:

```bash
bin/tf.sh apply -var 'instance_type=a1.metal'
```

**Deferred, not attempted.** Worth doing before any long-running work settles on
the $3.17/hr box.

## 4. How the FDT gets read

Firecracker builds the device tree in-process and has no dump facility, so the
only way to see the FDT it hands a guest is to boot something that exposes
`/sys/firmware/fdt`. The project therefore builds an Alpine aarch64 microVM and
boots it with `init=` pointed at a script that base64s the DTB to the serial
console; the host reassembles it, checks the `d00dfeed` header magic, and renders
a `.dts` with `dtc`.

It sweeps `vcpu_count` ∈ {1, 2, 4, 8}, once each, because that is the axis the
GIC redistributor base moves along.

The Alpine guest earns its keep twice: it is also the **control** for networking.
`AKUMA_FIRECRACKER_KVM.md` §5.1 recorded inbound RX never reaching Akuma while TX
was correct on the wire — a known-good Linux guest on the same `tap0`/dnsmasq/NAT
is what distinguishes "the host side is wrong" from "Akuma's virtio-net is wrong".
(Commits `07359427`..`a9a185b5` have since worked on that path; the control
remains useful.)

Artifacts, per vCPU count: `fdt-vcpuN.dtb`, `fdt-vcpuN.dts`,
`guestinfo-vcpuN.txt` (`/proc/interrupts`, `/proc/iomem`, `/proc/cmdline`),
`console-vcpuN.log`, plus one `summary.txt` that prints the `memory`, `intc`,
`pl011` and `virtio_mmio` nodes with the expected v1.16.1 values underneath for
comparison, and `hostinfo.txt` (MIDR_EL1, CPU features, the `started at EL2`
proof, Firecracker version).

## 5. Why the root image is built on the host, from a tarball

`scripts/populate_disk.sh` drives a **privileged Docker container** to loop-mount
the ext2 image, because macOS cannot loop-mount ext2 at all. On a Linux host
running as root with e2fsprogs, `mkfs.ext2` and `mount -o loop` work directly —
so the host-side path uses them and Docker is not a dependency at all.

The rootfs travels as a `tar.xz` that is *already the finished image root*:
`bin/package-rootfs.sh` merges `bootstrap/bin` + `bootstrap/usr` with
`overlays/devbox/rootfs/etc` (the devbox recipe takes `/etc` from the overlay
only — `overlays/devbox/bootstrap.sh` step 3 wipes the base `/etc` first), injects
the operator's ed25519 public key, and writes a manifest recording the git rev.
The host then only has to `mkfs.ext2 -b 4096 -L AKUMA`, untar, and `e2fsck -fn`.

`models/` (508 MB), `music/` (479 MB) and `archives/` (53 MB) are never packaged.
They are 987 MB of the 1.2 GB in `bootstrap/` and no part of a bootable image.

The `e2fsck -fn` on a freshly built image is deliberate: it separates "the image
was built wrong" from "Akuma's ext2 driver corrupted it", a distinction that has
cost real debugging time on this project.

## 5b. ECR, and why the host has no IAM role

The image root is packaged as an OCI image (`FROM scratch` plus the merged
rootfs — no distro, no shell, just the ext2 contents in registry form) and pushed
to ECR. The host pulls it, `docker save`s it, and extracts the layers in manifest
order into the mounted ext2.

**`docker save` + manual layer extraction, not `docker create` + `docker export`.**
Export goes through a container, and Docker populates `/etc/resolv.conf`,
`/etc/hosts` and `/etc/hostname` in the container layer — which would silently
shadow the `resolv.conf` that `overlays/devbox/rootfs` ships and that the guest's
DNS depends on. The image is `FROM scratch` with only `COPY` layers, so there are
no whiteouts and layer order alone reproduces it exactly.

ECR is the default transport because of layer dedup: `bootstrap/bin` is 163 MB and
changes rarely, so as its own layer it uploads once and a later `/etc`-only change
pushes kilobytes. The tarball route re-uploads all 175 MB, but needs no registry
and no container runtime, so it stays as `--from tarball`.

### No instance profile

The obvious design is an EC2 role carrying the AWS-managed
`AmazonEC2ContainerRegistryReadOnly`. **The credentials this project runs under
cannot create or attach one**, so it is not available. A managed *policy* existing
does not help on its own: it has to hang off a role, the role has to be wrapped in
an instance profile, and attaching that to an instance is a further permission
again.

What works with **zero IAM**: `aws ecr get-authorization-token` on the workstation
returns a 12-hour registry credential, piped straight into `docker login` on the
host over SSH. Nothing long-lived is left on the box. This is the better default
regardless of permissions — a short-lived token beats a standing role on a box
that exists for an afternoon.

The specific permission checks, and what an admin would need to grant for the role
route instead, are in the private `akuma-terraform` repo (`ecr.tf`) rather than
here. This document is in a public repo; an account's IAM posture is not something
to write down in one.

The repository URL is derived from `aws_caller_identity` + `var.region` +
`var.ecr_repository_name` rather than committed, so no account id lives in either
repo. The login region is parsed out of the URL, not taken from `var.region`, so a
cross-region registry works unchanged — which mattered for about ten minutes when
the repository existed in Osaka and the host in Tokyo.

### The ed25519 key

`overlays/devbox/rootfs/etc/sshd/sshd.conf` was changed to
`disable_key_verification = false` on 2026-08-21, so an image with no
`/etc/sshd/authorized_keys` refuses every connection. The packager injects
`~/.ssh/id_ed25519.pub` (which already matches `bootstrap/etc/sshd/authorized_keys`
in the tree) and **rejects a non-ed25519 key at package time** — `userspace/sshd`
(`userspace/sshd/src/keys.rs`) implements exactly one key type, so an RSA key
there is unparseable rather than merely weaker. `bin/ssh-microvm.sh` passes `-i`
with `IdentitiesOnly=yes` for the same reason: ssh offering an unreadable key
looks identical to a rejected one.

## 6. Deferred: the boot partition

The self-host loop wants a second small ext2 image as a second virtio-blk drive —
Akuma rebuilds its kernel in-VM, writes the flattened image to
`/boot/akuma-fc.bin` there, and the host loop-mounts it, checks the ARM64 `Image`
magic and that `text_offset` puts `_boot` at `0x8030_0000`, then boots what Akuma
built.

**Blocked in the kernel, not in the infrastructure.**
`crates/akuma-virtio/src/block.rs:234` keeps a single global
`BLOCK_DEVICE: Spinlock<Option<VirtioBlockDevice>>`, and `init()` binds the
*first* virtio-blk it probes. A second drive would be created by Firecracker,
walked past by the probe, and never mounted — so attaching one now would add a
slot and no capability. It was written and then deliberately removed from
`60-akuma-image.sh`; the generated config has one drive.

Order of work when it is picked up:

1. A second device slot in `akuma-virtio`'s block driver (or a small table in
   place of the single global), plus a VFS mount point. Per `CLAUDE.md`, this
   needs a boot-suite self-test in `src/process_tests.rs`.
2. `60-akuma-image.sh`: create `akuma-boot.img` (64 MB, ext2, label `AKUMABOOT`)
   **only if absent** — it is the one image whose contents come *from* the guest,
   so a root rebuild must not discard it.
3. A second `drives` entry, **after** `rootfs`. Firecracker assigns virtio-mmio
   addresses in device-creation order and the block driver takes the first blk it
   finds, so the order is load-bearing.
4. `bin/fetch-kernel.sh` on the workstation to extract and verify the result.

## 7. Three traps worth remembering

**A same-named profile in `~/.aws/config` shadows static keys elsewhere.**
Credentials live in a separate file, but the SDK still consults `~/.aws/config`
for the profile of that name and prefers what it finds there — so bare
`terraform` fails with *"Token has expired and refresh failed"* even with
perfectly good keys in the credentials file. `bin/tf.sh` points `AWS_CONFIG_FILE`
at an empty file so the shadowing entry is invisible. Worth knowing generally: the
error names expiry, so it sends you to refresh credentials that were never the
ones being used.

**Docker's container layer shadows the image's `/etc`.** See §5b: `docker export`
would have quietly replaced the devbox `resolv.conf`. This is the sort of thing
that surfaces later as "DNS broke in the guest" with no connection to the image
build.

**`user_data` has a 16 KiB ceiling and base64 is the wrong lever.** The bootstrap
scripts are ~25 KB of shell. Embedding them base64-encoded inside the cloud-config
(the obvious `encoding: b64` route) inflated them by a third *before* gzip and
landed within ~900 bytes of the cap. Passing them as plain text and gzipping the
whole document brings it to ~10 KB decoded. Same content, one third the size,
because base64 does not compress.

## 8. Results

Applied 2026-08-21. `m6g.metal` in `ap-northeast-1a`, Ubuntu 24.04 arm64,
host kernel `6.17.0-1019-aws`.

### The host is what it needed to be

```
MIDR_EL1:  0x413fd0c1          Neoverse N1 -> Graviton2
nproc:     64
/dev/kvm:  crw-rw---- root kvm 10, 232
dmesg:     CPU: All CPU(s) started at EL2
kvm [1]:   VHE mode initialized successfully
kvm [1]:   IPA Size Limit: 48 bits
```

`started at EL2` and `VHE mode` are the lines that matter. Note **VHE**, not the
nVHE the Lima nested-virt host ran in — this is L0 hardware.

### The measurement that was the point

`intc` `reg` from the FDT Firecracker emitted, per vCPU count:

| vCPUs | GICD | GICR base | GICR size | predicted base |
|---|---|---|---|---|
| 1 | `0x3fff0000` / `0x10000` | `0x3ffd0000` | `0x20000` | `0x3ffd0000` ✓ |
| 2 | `0x3fff0000` / `0x10000` | `0x3ffb0000` | `0x40000` | `0x3ffb0000` ✓ |
| 4 | `0x3fff0000` / `0x10000` | `0x3ff70000` | `0x80000` | `0x3ff70000` ✓ |
| 8 | `0x3fff0000` / `0x10000` | `0x3fef0000` | `0x100000` | `0x3fef0000` ✓ |

`get_redists_addr(n) = 0x3fff0000 - n * 0x20000`, measured. This is the fact that
makes `FIRECRACKER_PORT.md` §5's structural choice not a preference: no build-time
constant can hold an address that moves with a runtime `SMP=N`.

Also confirmed, from the same blobs:

- **GICD is 64 KiB**, so the single 4 KiB page the device VA map used to reserve
  was wrong — the `GICD_IROUTER` aliasing fix was necessary, not merely tidy.
- **virtio INTIDs start at SPI 0 → INTID 32.** `virtio_mmio@40003000` has
  `interrupts = <0x00 0x00 0x01>`; the next two are SPI 1 and 2. QEMU virt starts
  at 48.
- **virtio-mmio stride `0x1000`, one node per configured device** — three nodes
  for three devices, not QEMU's eight `0x200`-spaced slots in one page.
- **Serial at `0x40002000`, SPI 3 → INTID 35 — advertised as `ns16550a`, not a
  PL011.** Akuma drives it as a PL011 and TX works by coincidence (PL011 `DR` and
  16550 `THR` are both at offset `0x00`); status reads do not line up (`FR` at
  `0x18` vs `LSR` at `0x05`). See `docs/reference/firecracker/fdt/README.md`.
- **PSCI v1.3**, and the tree describes every configured vCPU (`cpu@0..n`).
  Described is not running — secondaries are powered off awaiting PSCI wakeup, as
  §3 Q5 predicted.
- **One correction:** the FDT `memory` node starts at **`0x80200000`**, not
  `0x80000000`. Firecracker reserves the first 2 MiB (`SYSTEM_MEM_SIZE`) and the
  node describes only what follows, so 1024 MiB configured reads as
  `<0x0 0x80200000 0x0 0x3fe00000>`. That is why Akuma prints
  `[Memory] Detected from DTB: base=0x80200000`.

`docs/reference/firecracker/memory-map.md` moved **B → A** on the strength of
this, with the `memory`-node correction folded in.

## 9. Three bugs found on the way, all in this project's own tooling

Recorded because each one produced a failure that looked like something else.

**1. Alpine's `vmlinuz-virt` is not a raw ARM64 `Image`.** Firecracker rejected it
with `InvalidImageMagicNumber`. Since `CONFIG_EFI_ZBOOT` it is an EFI zboot PE
wrapper — `MZ`, then `zimg` at offset 4, a `u32` payload offset at 8, and the
compression name at 0x18 — with the real Image compressed inside. So offset 56,
where the ARM64 magic lives, held payload (measured: `0x818223cd`). Alpine 3.24.1
uses `gzip` at offset 51832; inflating that yields a 36 MB Image whose magic
checks out. The pre-boot magic assertion is what turned this into a one-line
diagnosis instead of a silent hang.

**2. Serial console line endings are CRLF.** The dump filters console lines to the
base64 alphabet before decoding. Doing that *before* `tr -d '\r'` discards every
single line, because each ends in a `\r` that is not in the character class — an
empty decode indistinguishable from a guest that never dumped anything. The FDT
had been sitting in the log the whole time.

**3. FDT header magic is big-endian; `od -tx4` is not.** With the CRLF fixed, the
integrity check reported `0xedfe0dd0` against an expected `0xd00dfeed` — the same
four bytes, read as a host-endian word. The blob was correct and the assertion was
wrong. Compare bytewise (`od -An -tx1`).

The shape these share: **every one of them was caught by a check that existed
specifically to catch it**, and each check paid for itself by being wrong in a
legible way rather than hanging. Worth keeping in mind when adding the next
assertion.

## 10. State, and what is next

Applied: 13 resources (VPC, IGW, subnet in an AZ that actually offers the type,
route table + association, SG with `tcp/22,2222,4444` from one `/32`, egress, key
pair, instance, EIP). The ECR repository is externally managed and deliberately
not in the plan. Bootstrap stages 10/20/30/40/50 all green; 60/62 are staged but
run on demand, since they need an artifact from the workstation.

Done: the FDT sweep, and `memory-map.md` promoted to grade A.

Not done yet:

1. **Akuma itself has not booted on this host.** The path is
   `bin/push-akuma.sh` → build on the host's 64 cores →
   `overlays/devbox-firecracker-aws/build-rootfs-image.sh` → `bin/build-image.sh`.
   Every piece is written; none of it has been run end to end here.
2. **The FDT-derived device map** (`FIRECRACKER_PORT.md` §5). The evidence it was
   waiting for now exists, including the `SMP=N` axis, so this is unblocked.
3. **The a1.metal probe** (§3), for a 6x cost reduction on everything after.
4. **The boot partition** (§6), which needs the kernel-side second block device
   first.

**Cost discipline.** On-demand at $3.168/hr is $76/day idle. Stop the instance
between sessions — the Elastic IP keeps the address and only the 200 GB gp3
volume bills (~$16/month):

```bash
aws ec2 stop-instances --region ap-northeast-1 \
  --instance-ids "$(bin/tf.sh output -raw instance_id)"
```

---

## Background

- `proposals/FIRECRACKER_PORT.md` — verified constants, the vCPU-dependent GIC
  redistributor, and why the device map must be FDT-derived
- `docs/archive/AKUMA_FIRECRACKER_KVM.md` — the first Firecracker boot and the
  five reset-value assumptions it exposed
- `docs/reference/firecracker/memory-map.md` — the map this project's dumps are
  meant to promote from B to A
- `docs/runbooks/run-on-firecracker.md` — the local (Lima) procedure
- `overlays/devbox-firecracker/` — the scripts this project's stage 20 mirrors
