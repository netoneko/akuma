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
| `ecolib.py` | metadata server (per guest), apk mirror, QEMU guest (SLIRP, hostfwd/guestfwd) |
| `launch.py`, `verify.py` | the launcher and the stage-1 checks |
| `refdemo.py` | stage 2: build and run the reference services in the boxes, run their acceptance suite from guest 2 |

## Design decisions

* **Network: QEMU user networking (SLIRP) per guest, no privileges.** The guest gets the host's internet through
  QEMU's NAT (what a box needs for a model API, crates.io, Maven Central). Three QEMU features do the rest:
  `hostfwd` (host loopback port -> guest ssh and, for guest 1, :8081/:8082), `guestfwd` (the guest's 169.254.169.254:80 ->
  that guest's own metadata server, one server per guest) and `host=169.254.0.2` (the host as a guest sees it, which is
  where guest 2 reaches guest 1's services: `http://169.254.0.2:<forwarded port>`; a guest reaches its own services the
  same way). The SLIRP network is 169.254.0.0/16 because `guestfwd` only accepts an address inside it. It also maps to
  Firecracker's shape (a forward per service, a metadata endpoint). An earlier bridge + unprivileged-namespace design
  worked but had no internet and was replaced (see `docs/archive/AKUMA_ECOSYSTEM_DEMO.md`).
* **Metadata** is the real address `169.254.169.254:80`, IMDSv2, **one server and data set per guest**.
* **Bootstrap integrity**: the script comes from `/latest/user-data`, its sha256 from `/latest/meta-data/bootstrap-sha256`;
  `eco-bootstrap` refuses a mismatch. Both come from the same service, so this guards the script's storage and transit, not a
  compromised service. The kernel has no `/proc/cmdline`, so the boot line cannot carry the digest.
* **Secrets** are random per run, held in the metadata server's memory, written root-only (700/600) once, and passed by
  `eco-box exec NAME ...` as environment variables of the box NAME only. They are never in an image or a log; `verify` greps for them.
* **apk** inside a box talks to a pinned mirror served by the launcher (`http://169.254.0.2:8000/v3.22/{main,community}`), the
  same files the images were built from. A package that was not cached 404s.
* **Output from a box** goes through a file (E1): `eco-box exec` reads it from the container's upper directory.

## Denver QEMU experiment

The launcher can use the same herd bootstrap on an experimental amd64 Denver kernel:

```sh
cargo build -p denver --target x86_64-unknown-none --release --features containers,codex
python3 overlays/ecosystem-demo/build-images.py all
export ECO_KERNEL="$PWD/target/x86_64-unknown-none/release/denver"
python3 overlays/ecosystem-demo/launch.py verify
python3 overlays/ecosystem-demo/boxed-codex-probe.py
python3 overlays/ecosystem-demo/execthread-probe.py --out /var/tmp/akuma-eco/denver-ready
python3 overlays/ecosystem-demo/refdemo.py --ref ../akuma-demo-reference-impl --scala-interpreted --scala-assembly
```

The interpreter option permits scala-cli compilation after an observed JVM crash. The assembly option packages the board in the guest and starts it directly with Java, avoiding the known non-leader-thread exec failure in `scala-cli run`; it does not fix that kernel defect. The reference acceptance suite and Git round trips passed with these options and a reused Git server binary. `--git-server-bin PATH` reuses a previously built reference binary and reports its digest, skipping the in-guest Cargo build. The launcher supplies the host's current Unix time to each guest for TLS certificate validation. It uses snapshot disks; work inside the VMs is discarded at teardown. The `containers` profile is experimental. Box mapping identities are limited to 255 distinct mounts per boot and 24-bit inode numbers; repeated box recreation can exhaust the limit. This is not a production isolation guarantee. The boxed Codex probe supplies local binaries and a model cache to a disposable guest, uses a fake model, and disables the sandbox. The demo images and driver do not yet launch the planned real boxed Codex sessions, and actual provider credentials and models have not been validated. See `docs/archive/DENVER_BOX_WIRING.md` and `docs/archive/DENVER_CODEX_TOOLS.md`.

`execthread-probe.py` builds the static musl threaded-exec reproducer and tries successful and failed execs both on the guest root and inside a box, each in a fresh VM. It checks the result marker, SSH survival and serial faults; intermittent passes do not establish that threaded exec is fixed. It requires the Rust `x86_64-unknown-linux-musl` target.
