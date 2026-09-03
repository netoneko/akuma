# AWS metal host for Firecracker: the terraform project, and what it decided

**Date opened:** 2026-08-21
**Project:** `../akuma-terraform/` (its own git repo,
`github.com/netoneko/akuma-terraform`)
**Status:** **delivered.** `m6g.metal` in `ap-northeast-1`, Firecracker v1.16.1
under KVM in VHE mode, the FDT dumped at 1/2/4/8 vCPUs
(`docs/reference/firecracker/fdt/`) — and, as of the second session on
2026-08-21, **Akuma itself boots on it and answers SSH**, at 1 and 2 vCPUs
(§10).
**Design it serves:** `docs/archive/FIRECRACKER_PORT.md` §7 (AWS metal) and §5
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

## 7. Four traps worth remembering

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

**A region you have not opted into reports expired credentials.** Every EC2 call
against a non-enabled opt-in region (`il-central-1`, `eu-south-1`,
`eu-central-2`, …) fails with `AuthFailure: AWS was not able to validate the
provided access credentials` — and since this project runs on temporary SSO
credentials, "they expired" is the obvious and wrong conclusion. The test is to
call a *second* region: if `eu-central-1` answers at the same instant, the
credentials are fine and the region is the problem.
`aws account list-regions --region-opt-status-contains ENABLED
ENABLED_BY_DEFAULT` settles it outright. The nastier variant: the same call with
`--filters Name=instance-type,...` prints the error on **stderr** and an empty
list on stdout, so a script that discards stderr concludes "this region offers no
Graviton metal" — a wrong answer that looks like a measurement. Full table and
the enable-permission problem in §10.1.

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

## 9. Eight bugs found on the way, all in this project's own tooling

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

### The second session's five, 2026-08-21

The first three were found while building the FDT dump. These five were found
taking the same project from "Akuma has never booted here" to an SSH session,
and they share a different shape: **each one only fires on the second run.** The
bootstrap was verified once, on a fresh host, and every one of these was invisible
until something ran twice — a reboot, a re-push, a rebuild.

**4. `dnsmasq` cannot reopen its own log, so networking dies on every reboot but
the first.** `akuma-fc-net.service` failed with

```
dnsmasq: cannot open log /var/tmp/dnsmasq.log: Permission denied
```

leaving tap0 present but no DHCP, no NAT and no ssh forward — which reads as a
networking bug in the guest, not a host unit that never started. The host runs
`fs.protected_regular = 2`, which refuses an open-for-write of an *existing*
regular file in a world-writable sticky directory when the file's owner is not
the opener. **Root is not exempt** — protecting root from file-planting in `/tmp`
is the entire point of the sysctl. dnsmasq creates the log as root and then
`fchown`s it to `nobody`, so run 1 (file absent, created) succeeds and every
later run (file present, owned by `nobody`) fails. `/var/log` and `/run` are
root-owned and not sticky; the log and pid file live there now.

**5. `rustup target add` without `--toolchain` installs it on the wrong
toolchain.** The Akuma tree pins `channel = "nightly"`, but stage 50 runs from
`/`, outside any cargo project, where the pin is invisible — so the bare-metal
std landed on **stable** and the kernel build got nightly. The failure is 15
crates deep and names neither rustup nor the pin:

```
error[E0463]: can't find crate for `core`
note: the `aarch64-unknown-none` target may not be installed
help: consider building the standard library from source with -Zbuild-std
```

and `rustup target list --installed`, also run outside the project, cheerfully
confirms the target is present. Both toolchains get it now.

**6. `/usr/bin/rsync` on macOS 15 is openrsync, and rejects `--info`.** It
prints a usage dump and transfers nothing. Worse, the flag error is *not* what
made this expensive: `--delete-before` had already deleted the previous tree, so
a failed push leaves **no** source on the host rather than a stale one. The next
build failed with `overlays/devbox-firecracker/build.sh: No such file or
directory` — which looks like a missing script, not a dead transfer. §10's
table has what replaced the transport.

**7. `bin/package-rootfs.sh` died on `set -u` with no arguments.**
`"${ARGS[@]}"` on an empty array is an unbound-variable error on bash 3.2 —
which is `/bin/bash` on macOS — so the *default* invocation was the broken one:
`ARGS[@]: unbound variable`. `${ARGS[@]+"${ARGS[@]}"}` expands to nothing
instead.

**8. Stages 60 and 62 were never on the host.** `build-image.sh` uploaded 45 MB
over a 53 KB/s link — 13 minutes — and *then* failed with
`sudo: /opt/akuma/bin/60-akuma-image.sh: command not found`. This is trap 2 of
this document made concrete: the pending `user_data` change that stages them is
still unapplied in terraform state, and the by-hand fixes done to the live host
in session 1 did not cover these two. They were scp'd up. **The ordering is the
lesson** — check the remote prerequisite *before* the slow upload, not after.

## 10. Akuma booted here, 2026-08-21 (second session)

The four items §10.2 lists as "not done" were the point of this session. The
first is done.

### What ran

The instance was **started, not re-applied.** `terraform plan` showed
`0 to add, 12 to change, 0 to destroy` — the volume shrink 200 -> 10 GiB reads as
an **in-place** `ModifyVolume`, not the reprovision the session-1 handoff
predicted, and EBS cannot shrink a volume, so that apply would fail at the API
rather than rebuild the box. The 12 changes are the shrink, the default tags and
the `user_data` staging of stages 60/62. None of them were needed to boot Akuma,
and applying would have cost a metal stop/start, so the box was started with
`aws ec2 start-instances` and left otherwise untouched. **The pending changes are
still pending.**

Pipeline, in the order it actually worked:

| Step | Result |
|---|---|
| source to the host | **git clone, 1.8 s** — not rsync (§9 bug 6) |
| kernel build, 64 cores | 27 s; `_boot at 0x80300000`, `text_offset=0x100000 image_size=0x402000`, 3.19 MB flat |
| rootfs package (workstation) | 174 MiB root -> 45 MB `tar.xz`, 42 s |
| upload to host | **13 minutes** over a 53 KB/s link |
| ext2 image build | 1024 MB, label AKUMA, `e2fsck -fn` clean, `authorized_keys` present |
| boot, 1 vCPU | `PASSED=292 FAILED=0 POISON=0`, 3491 lines |
| boot, 2 vCPUs | `PASSED=302 FAILED=0 POISON=0`, 4361 lines |
| SSH from the workstation | works at both |

### The three things it settled

**1. Inbound RX works.** This was the one open question, and the DHCP handshake
alone answers it — `DHCPREQUEST` cannot come from a guest that never received the
offer:

```
[SmolNet] DHCP configured
[SmolNet] IP: 10.0.2.15/24
Heartbeat ... rx posted=13 fail=0 recvd=12
```

Then `ssh -p 4444 root@<eip>` returned
`Akuma akuma 0.0.7 fab3e50-release-smp-shared aarch64 Linux`. Measured inbound
throughput, guest pulling over HTTP from the host: **20 MiB in 1.32 s, ~15 MB/s.**

**2. `vcpu_count = 2` works, and the FDT map is why.** The handoff listed "1 vCPU
only, and it fails silently above that" as an open blocker. It is not open: the
FDT-derived map (`crates/akuma-firecracker`, consumed by
`platform::install_fdt_device_map`) landed in the meantime and moves the
redistributor at run time.

```
vcpus=1  [Platform] FDT device map: GICR=0x3ffd0000
vcpus=2  [Platform] FDT device map: GICR=0x3ffb0000 (moved from bootstrap literal)
         [SMP-shared] probed 2 core(s) / CPU_ON core 1 (mpidr=0x1) -> ok
```

`0x3ffb0000` = `0x3fff_0000 - 2 * 0x2_0000`, exactly §8's measured formula. The
`(moved from bootstrap literal)` suffix is the thing to grep for: its absence at
`vcpu_count > 1` means the parse fell back and the boot core is about to lose its
tick. **4 vCPUs is still untried here**; `fab3e50c` records
`smp=4 breaks disk in lima`, and this host is the right place to retry it without
Lima's overhead in the way.

**3. Where the CPU actually goes.** Three different "slow" claims came up and they
are three different things:

| | Measurement |
|---|---|
| guest `topd`, network thread, idle | 68.9% |
| host `firecracker` (all threads), guest idle | **2% of one core** |
| host `firecracker`, sustained 15 MB/s transfer | 65% of one core |
| operator link, workstation <-> Tokyo | **53 KB/s up, 34 KB/s down, ICMP blocked** |

Rows 1 and 2 are the same idle machine. Akuma's `topd` figure is a share of
*guest scheduler* time and the netpoll thread is what the scheduler parks on when
nothing else is runnable, so an idle guest credits it ~70% while costing the host
2%. **The host number is the honest one.** Row 4 is the operator's home uplink and
has nothing to do with Akuma — it is what made a 45 MB upload take 13 minutes.
The earlier "network eats 70% of CPU" observation was also taken under Lima,
where nested virt inflates everything; on metal it does not reproduce.

### Four findings that are not Firecracker's fault

Worth writing down because each looked like a platform bug and none was:

1. **The `devbox` image root has no busybox applet links.** Every command is
   `not found`; only `/bin/busybox <applet>` and `/bin/sh` resolve until someone
   runs `busybox --install`. There is nothing to fix in the packager's copy step —
   `bootstrap/bin` on the workstation holds **59 regular files and 1 symlink**, no
   applet links to preserve.
2. **No CA bundle.** `curl: (77) Error reading ca cert file
   /etc/ssl/certs/ca-certificates.crt` on any HTTPS fetch. It exists at
   `bootstrap/etc/ssl/certs/ca-certificates.crt`, but the `devbox` profile takes
   `/etc` from `overlays/devbox/rootfs/etc` **only** (§5), which has no `ssl/`.
   `--profile full` carries it.
3. **Kernel-path DNS points at nothing here.** `smoltcp_net.rs` has
   `QEMU_DNS_SERVER = 10.0.2.3`, SLIRP's forwarder. Under Firecracker the resolver
   is `20-net.sh`'s dnsmasq at `10.0.2.2`, which it also hands out as DHCP option
   6. The log prints `[SmolNet] DNS socket initialized (server: 10.0.2.3)` and
   nothing supersedes it.
4. **`/proc` is nearly empty, and it is what limits real binaries.** `fastfetch`
   2.67.1 built `-static` runs on Akuma and prints `Kernel: Akuma 0.0.7` — and
   little else, because `/proc` holds **only numeric PID directories and `boxes`**:
   no `cpuinfo`, `meminfo`, `uptime`, `version`. Same gap that blocks redis, so
   filling it pays twice. (Neither upstream fastfetch release is usable regardless
   of platform: `fastfetch-linux-aarch64` and `-polyfilled` are the same BuildID,
   both glibc-dynamic PIEs wanting `/lib/ld-linux-aarch64.so.1`. "Polyfilled"
   means an older glibc, not static.)

## 10.1 Region: measured, and why Tokyo stays for now

§2 recorded "Japan was the requirement". With the operator in Israel that costs
**253 ms RTT**, and the 45 MB rootfs upload took 13 minutes. Measured 2026-08-21,
TCP-connect time to `ec2.<region>.amazonaws.com` (~1 RTT), best of 3, plus
`m6g.metal` on-demand from the price list API:

| Region | RTT | `m6g.metal` | Usable on this account? |
|---|---|---|---|
| `il-central-1` Tel Aviv | **12 ms** | $2.8896 | **No — opt-in, not enabled** |
| `eu-south-1` Milan | 52 ms | $2.8672 | No — opt-in, not enabled |
| `eu-west-3` Paris | 59 ms | $2.8800 | yes |
| `eu-west-2` London | 68 ms | $2.8416 | yes |
| `eu-central-1` Frankfurt | 64-72 ms | $2.9440 | yes |
| `eu-north-1` Stockholm | 87 ms | **$2.6240** | yes |
| `eu-west-1` Ireland | 76-98 ms | $2.7520 | yes |
| `ap-northeast-1` Tokyo | **253 ms** | $3.1680 | current |
| `eu-central-2` Zurich | 56 ms | **none** | no Graviton metal at all |

**Tel Aviv is 21x closer and 9% cheaper than Tokyo, and cannot be used without an
account change.** It is an opt-in region and this account has not enabled it;
`aws account list-regions --region-opt-status-contains ENABLED ENABLED_BY_DEFAULT`
is the authority. Same for Milan.

**The trap that wasted time here, worth knowing:** a non-enabled opt-in region
does not answer "region not enabled". Every EC2 call returns

```
An error occurred (AuthFailure) when calling the DescribeInstanceTypeOfferings
operation: AWS was not able to validate the provided access credentials
```

which reads as expired credentials — and these *are* temporary SSO credentials,
so that is entirely plausible. It is not the credentials: `sts
get-caller-identity` and the same call against `eu-central-1` both succeed at the
same instant. **Test enabled-vs-expired by calling a second region, never by
re-authenticating.** Worse, a query filtered by instance type in such a region
returns an empty list on stderr, which reads as "this region has no Graviton
metal" if stderr is being discarded. Zurich is the one row above where the
absence is real.

Enabling `il-central-1` needs `account:EnableRegion`, which this SSO role
(`AWSReservedSSO_PowerUserAccess`) does not have — the same permission wall as
§5b's IAM roles. **That is the one action worth asking an admin for**: it turns a
253 ms link into a 12 ms one and cuts 9% off the hourly rate.

Failing that, `eu-west-3` (Paris, 59 ms) or `eu-central-1` (Frankfurt) are
available today at ~4x better latency and ~7% cheaper. `eu-north-1` (Stockholm)
is the cheapest usable at $2.6240 but 87 ms.

**None of this is why the upload was slow enough to matter.** The 45 MB tarball is
avoidable rather than accelerable — see §9 bug 6, and note that the host clones
this repo in 1.8 s while the workstation needed 13 minutes for the tarball. Fix
the transport before paying for a migration.

### a1.metal, for the deferred probe (§3) — and it constrains the region choice

| Region | `a1.metal` | vs `m6g.metal` there | RTT from Israel |
|---|---|---|---|
| N. Virginia | $0.4080 | — | 147 ms |
| Ireland `eu-west-1` | $0.4610 | 6.0x cheaper | 76-98 ms |
| **Frankfurt `eu-central-1`** | **$0.4660** | 6.3x cheaper | **64-72 ms** |
| Tokyo `ap-northeast-1` | $0.5140 | 6.2x cheaper | 253 ms |
| Tel Aviv, Paris, London, Stockholm, Milan | **not offered** | — | 12-87 ms |

`a1.metal` is **16 vCPU / 32 GiB**, Graviton1, "up to 10 Gigabit" — the whole
family is `medium` (1/2 GiB), `large` (2/4), `xlarge` (4/8), `2xlarge` (8/16),
`4xlarge` (16/32) and `metal` (16/32). Still unproven for Firecracker (§3).

**`a1.metal` is the floor: AWS sells no smaller aarch64 metal instance.** Every
bare-metal arm64 type, by size (`describe-instance-types --filters
bare-metal=true, processor-info.supported-architecture=arm64`):

| vCPU | RAM | Types |
|---|---|---|
| **16** | **32 GiB** | **`a1.metal` — the only one** |
| 64 | 128-512 GiB | `c6g` `c6gd` `c7g` `c7gd` `g5g` `m6g` `m6gd` `m7g` `m7gd` `r6g` `r6gd` `r7g` `r7gd` |
| 96 | 192 GiB-1.5 TiB | `c8g` `c8gd` `i8g` `i8ge` `m8g` `m8gd` `r8g` `r8gd` `x8g` (all `.metal-24xl`) |
| 192 | 384 GiB-3 TiB | the `.metal-48xl` tier, plus `c9g` / `m9g` |

The `.metal-24xl` / `.metal-48xl` suffixes on the 8g/9g families are AWS's
partial-metal sizes — a bare-metal slice smaller than the whole host — but they
begin at 96 vCPU, so they are larger than the 64-core generation, not smaller.

**There is a 4x cliff and no rung in between**, and it is a price cliff too: from
`a1.metal` at $0.4660/hr (Frankfurt) the next bare-metal arm64 option is
`c6g.metal` at $2.4832 there, or $2.3347 in Ireland. **5.3x, in one step.**

This is what makes §3's deferred a1 probe worth more than its $0.10: `a1.metal` is
the *only* option below ~$2.33/hr, so that experiment decides whether cheap
iteration on this project is possible at all, or whether every hour of Firecracker
work costs $2.33 minimum. Do it before settling on a long-running box.

**`a1.4xlarge` is a trap: identical specs, $0.0004/hr cheaper, and it cannot run
this workload at all.** 16 vCPU and 32 GiB are the same numbers as `a1.metal`, so
a spec-and-price comparison picks it — but `BareMetal: false` means it is an EC2
guest, EC2 offers nested virtualization on no architecture, and therefore
`/dev/kvm` does not exist on it. That is §2's forced decision restated: for this
project the `.metal` suffix is the requirement and every other size in the family
is unusable regardless of price.

**a1 is an old generation and the newer regions do not carry it.** Confirmed two
independent ways: the price list returns nothing for those locations, and
`describe-instance-type-offerings` returns an empty AZ list in `eu-west-3`,
`eu-west-2` and `eu-north-1`. Among the regions this account has enabled, only
`eu-central-1` (AZs a, b) and `eu-west-1` (AZs a, c) offer it.

**So the two moves conflict.** "Get closer to Israel" points at Tel Aviv (12 ms),
Paris (59 ms) or London (68 ms) — **none of which have a1**. "Move to a1" points
at Frankfurt, Ireland, Tokyo or N. Virginia. The intersection is a single row:

> **`eu-central-1` (Frankfurt)** — a1.metal at $0.4660/hr *and* 64-72 ms, versus
> Tokyo's 253 ms. Not the closest region available, but the closest one that can
> ever run the cheap instance.

Pick the region against the a1 plan, not against latency alone: latency buys a
nicer session, a1 buys a 6x cheaper one, and only Frankfurt leaves both open.
Note also that a1's 16 vCPU / 32 GiB is a real step down from m6g.metal's
64 / 256 — §3 chose 256 GiB partly to remove memory as a variable for the
mmap-heavy self-host workloads, and 32 GiB puts it back.

## 10.2 State, and what is next

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

- `docs/archive/FIRECRACKER_PORT.md` — verified constants, the vCPU-dependent GIC
  redistributor, and why the device map must be FDT-derived
- `docs/archive/AKUMA_FIRECRACKER_KVM.md` — the first Firecracker boot and the
  five reset-value assumptions it exposed
- `docs/reference/firecracker/memory-map.md` — the map this project's dumps are
  meant to promote from B to A
- `docs/runbooks/run-on-firecracker.md` — the local (Lima) procedure
- `overlays/devbox-firecracker/` — the scripts this project's stage 20 mirrors
