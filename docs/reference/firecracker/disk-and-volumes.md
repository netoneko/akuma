# Drives under Firecracker, and how local maps to AWS

**Stability: C** — the local half is verified; the AWS half is reasoned, not run.
Every AWS claim below is marked *(untested)* where it has not been executed.

## 1. What Firecracker actually wants

One line of config per drive:

```json
{ "drive_id": "rootfs", "path_on_host": "/var/tmp/disk.img",
  "is_root_device": true, "is_read_only": false, "io_engine": "Sync" }
```

`path_on_host` is a path **on the KVM host**, and that is the whole contract.
Firecracker has no concept of mounts, images, or volumes — it opens that path and
exposes it to the guest as a virtio-blk device. Akuma then sees a block device
whose entire capacity is one bare ext2 filesystem, which is exactly what
`scripts/create_disk.sh` produces (no partition table).

`drives` is a **required** key in the single-JSON form even when empty;
Firecracker v1.16 fails with ``missing field `drives` `` otherwise.

Consequences worth internalising:

- **The host must not mount the filesystem it hands to the guest.** Two
  independent writers on one ext2 image corrupts it. Format it, unmount it, then
  give the path to Firecracker.
- `is_read_only: true` works and is genuinely read-only end to end. Useful when
  the image lives somewhere you cannot write.

## 2. Local (Lima) — why there is a copy step

Verified. On macOS the KVM host is a Lima VM, so there are two filesystems:

```
macOS  ~/github.com/netoneko/akuma/disk.img
   │
   │  Lima virtiofs mount — READ-ONLY
   ▼
guest  /Users/netoneko/github.com/netoneko/akuma/disk.img   (ro)
guest  /var/tmp/disk.img                                    (rw, guest's own ext4)
   │
   ▼  path_on_host
Firecracker → virtio-blk → Akuma
```

`mount` in the guest reports `lima-… on /Users/netoneko type virtiofs (ro,relatime)`,
and `touch` there fails with `Read-only file system`. So `overlays/devbox-firecracker/run.sh`
stages the image into `/var/tmp` before booting.

**That copy is scaffolding for the Lima sandwich and nothing else.** It exists
because the host filesystem is read-only across a VM boundary. Booting read-only
directly off the virtiofs path also works and skips the copy — both were tried,
and both reach the same point.

## 3. AWS metal — no sandwich, no copy

*(untested)* On `.metal` the instance **is** the KVM host. There is no filesystem
boundary to cross, so the copy step disappears entirely and `run.sh --local`
passes paths straight through.

The layout you'd expect, and it is the right one:

```
EBS root volume   /dev/nvme0n1   AL2023 or Ubuntu + the firecracker binary
extra volume      /dev/nvme1n1   Akuma's ext2 root
```

Worth knowing: "install Firecracker on the root volume" is barely a step.
Firecracker is a single static binary — the release tarball is ~7 MB and
`install -m0755` is the entire installation. A stock AMI needs nothing else.

### 3.1 Two ways to present the extra volume

**Variant A — hand Firecracker the raw device.** *(untested; verify before relying on it)*

```bash
sudo mkfs.ext2 /dev/nvme1n1          # bare filesystem, no partition table
# do NOT mount it
# path_on_host: "/dev/nvme1n1"
```

This is the shape with the fewest layers and it matches what Akuma already
expects. **The thing to check first** is whether Firecracker sizes the drive
correctly: on Linux, `stat()` reports length 0 for a block device, and the size
must come from a `BLKGETSIZE64` ioctl instead. If Firecracker derives capacity
from file metadata, a raw device would appear as 0 bytes and fail in a confusing
way. Test with `--no-net` and read the `[Block] Capacity:` line — Akuma prints it,
so this is a one-boot check.

**Variant B — a file on a formatted volume.** *(the safe default)*

```bash
sudo mkfs.ext4 /dev/nvme1n1
sudo mkdir -p /srv && sudo mount /dev/nvme1n1 /srv
# build or copy Akuma's image to /srv/disk.img
# path_on_host: "/srv/disk.img"
```

One more filesystem in the path, marginally slower, but it is exactly what was
verified locally, it lets several microVMs share a volume with a file each, and it
makes keeping a pristine image plus copy-on-launch trivial.

Start with B, try A as an optimisation.

### 3.2 EBS versus instance store

*(untested)* This choice matters more than it looks, because Akuma's heavy
workload is I/O-bound in a specific way: `docs/archive/` records the self-host
compile as **ext2 + mmap bound rather than CPU bound.**

- `c6g.metal` / `m6g.metal` are **EBS-only.** A gp3 volume has a provisioned IOPS
  ceiling, and metadata-heavy small reads will hit it.
- `c6gd.metal` / `m6gd.metal` add **local NVMe instance store**, which is far
  faster for this access pattern — but is **ephemeral**: wiped on stop, and gone
  if the instance is reclaimed. On spot, treat it as scratch and keep the golden
  image on EBS or S3.

If a self-host build is going to run under Firecracker, the `d` variants are
worth the price difference. For boot-and-console work, plain EBS is fine.

### 3.3 `io_engine`

*(untested on AWS)* `Sync` is the default and what everything here was verified
against. `Async` uses io_uring and Firecracker's own docs claim **up to 30× total
IOPS** for reads on NVMe; it needs host kernel ≥ 5.10.51 and is still labelled a
developer preview.

Given §3.2, this is probably the single cheapest performance lever on AWS. Opt in
with `FC_IO_ENGINE=Async` in `run.sh` once block I/O works at all (§4 of the
platform reference — it currently does not).

### 3.4 Getting the image onto the instance

Three options, in rough order of preference:

1. **Build it there.** `scripts/create_disk.sh` + `scripts/populate_disk.sh` on a
   64-core metal instance is fast, and avoids moving 2 GB.
2. **S3.** Upload once, `aws s3 cp` on each launch. Best for a golden image.
3. **`scp`.** Fine for a one-off, tedious at 2 GB.

## 4. Mapping table

| Concern | Local (Lima) — verified | AWS metal — *(untested)* |
|---|---|---|
| KVM host | a Lima VM | the instance itself |
| Host filesystem visible to KVM host | virtiofs, **read-only** | n/a — same machine |
| Staging step | copy to guest `/var/tmp` | **none** |
| Akuma's ext2 | 2 GB file | extra EBS volume, raw (A) or file on it (B) |
| Read-write | needs the copy | native |
| `io_engine` | `Sync` | `Async` worth testing |
| Backing store speed | host SSD via two VM layers | EBS (capped) or instance-store NVMe |
| `run.sh` invocation | default (`--via-lima`) | `--local` |
