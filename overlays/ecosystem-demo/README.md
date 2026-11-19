# Ecosystem demo overlay (stage 1: images, launcher, verification)

Three reproducible amd64 root images (rust, scala, python), a host-side metadata service and a launcher
that boots two guests of the **main amd64 kernel** on one network segment under QEMU+KVM, then proves
each toolchain works in its own `box`. No demo application code and no agents: see
[`docs/archive/AKUMA_ECOSYSTEM_DEMO.md`](../../docs/archive/AKUMA_ECOSYSTEM_DEMO.md) for the plan, the outcome
and the gaps.

## Reproduce

```sh
cargo build -p akuma --target x86_64-unknown-none --release --no-default-features --features headless,no-tests
overlays/ecosystem-demo/build-images.py fetch            # once: pin Alpine v3.22 packages + tool sources (needs internet)
overlays/ecosystem-demo/build-images.py all              # roots, guest1.img, guest2.img, MANIFEST.json  -> $ECO_WORK/out
overlays/ecosystem-demo/build-images.py repro            # build twice, compare every digest
overlays/ecosystem-demo/launch.py verify                 # boot both guests, run every check, tear down
overlays/ecosystem-demo/launch.py up                     # boot both guests and stay up
```

`$ECO_WORK` defaults to `/var/tmp/akuma-eco` (needs roughly 5 GB). `build-images.py all` needs `amd64/mkdisk.sh`'s
output once, for the akuma userspace binaries (herd, sshd, ssh, box); it runs it if `$ECO_WORK/base.img` is absent.
`--with-cargo-cache` vendors `images/rust-deps` into the rust root (off by default).

## What is where

| path | what |
|---|---|
| `build-images.py` | fetch/lock, build roots (apk.static into a directory, no root, no docker), deterministic tar layers, assemble the guest disks |
| `images/spec.json`, `images/apk-lock.json` | what goes in each root; sha256 of every cached apk and APKINDEX |
| `images/rust-deps/` | optional vendored crate set (Cargo.lock committed) |
| `common/` | files every box root gets (`ws-init`: the empty git workspace) |
| `guest/` | the host root's overlay: `instance-key`, `eco-bootstrap`, `eco-box`, herd services |
| `bootstrap/bootstrap.sh` | the script the metadata service delivers; installs extra ssh keys, per-agent prompts and secrets |
| `ecolib.py` | namespace, bridge/taps, DHCP, metadata server, apk mirror, QEMU guest |
| `launch.py`, `verify.py` | the launcher and the stage-1 checks |

## Design decisions

* **Network: a bridge + tap per guest inside an unprivileged user+net namespace** (`unshare --map-root-user --net`).
  Not SLIRP (each guest would sit behind its own private NAT and guest 2 could not reach guest 1), not
  `-netdev socket` (the kernel supports one NIC per guest, so a second NIC for the segment is not available, and one NIC must
  also carry the metadata service). Not a host bridge: no sudo here, and the namespace tears everything down with it.
  Cost: the guests have no internet (E3 in `proposals/FUTURE_WORK.md`).
* **Metadata** is served on the real address `169.254.169.254:80` (bridge-local), IMDSv2, **one data set per guest chosen by
  peer address**, which is how the real service tells instances apart. "Per guest" is `MetadataServer.add_guest`.
* **Bootstrap integrity**: the script comes from `/latest/user-data`, its sha256 from `/latest/meta-data/bootstrap-sha256`;
  `eco-bootstrap` refuses a mismatch. Both come from the same service, so this guards the script's storage and transit, not a
  compromised service. The kernel has no `/proc/cmdline`, so the boot line cannot carry the digest.
* **Secrets** are random per run, held in the metadata server's memory, written root-only (700/600) once, and passed by
  `eco-box exec NAME ...` as environment variables of the box NAME only. They are never in an image or a log; `verify` greps for them.
* **apk** inside a box talks to a pinned mirror served from the namespace (`http://10.77.0.1:8000/v3.22/{main,community}`), the
  same files the images were built from. A package that was not cached 404s.
* **Output from a box** goes through a file (E1): `eco-box exec` reads it from the container's upper directory.
