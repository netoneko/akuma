# Develop Akuma on Akuma: build in the box, test on KVM, root on a spare partition

**Stability: C — written before the first run; every step below is untested as a whole.** Overlay:
[`overlays/ryzen-akuma-dev/`](../../overlays/ryzen-akuma-dev/README.md) (this machine, `akuma-ryzen`, only).
Background: [`docs/archive/AKUMA_KVM_DEV_CYCLE.md`](../archive/AKUMA_KVM_DEV_CYCLE.md) (the goose-driven version of this
loop), [`docs/reference/subsystems/kvm.md`](../reference/subsystems/kvm.md) § VMs as services.

Goal: an agent (Claude Code, driven remotely) edits and builds a kernel inside Akuma, boots it as a guest of
Akuma's own hypervisor on a **dedicated NVMe partition**, and checks it — without rebooting the machine.

## 0. Prerequisites

- Akuma booted with the `kvmctl` entry (`/dev/kvm0` exists; `kvmmem=`/`kvmcores=` reserve guest RAM and cores).
- A spare partition `N` that holds nothing you want. **Not** p3 (host root; the hypervisor refuses it), p1/p6 (ESPs).
  Akuma has no nvme device node: list partitions from Pop (`lsblk -f /dev/nvme0n1`).
- Claude Code with Remote Control (already enabled).

## 1. Seed the partition (once, from Pop — destructive)

```sh
# a root image with /bin/sshd, busybox and /etc/sshd/authorized_keys; see scripts/kvm/akuma_guest_rootfs.sh
sh overlays/ryzen-akuma-dev/seed-part.sh /path/root.img N --yes-destroy-partition
```

## 2. Boot a guest on it (inside Akuma)

```sh
cd /src/github.com/netoneko/akuma/overlays/ryzen-akuma-dev
PART=N sh guest-up.sh                 # installs ./kbuild's ELF as /boot/akuma-guest-dev and starts vm-dev-part
PART=N sh guest-up.sh /path/to/taoxichuan   # a different guest kernel
```

Expect one line: `UP ready_in=<s> disk=/dev/nvme0n1pN`. The guest's console is `/var/log/herd/vm-dev-part.log`; its sshd
is host port 2224 (`/bin/ssh -p 2224 root@localhost …`, Akuma's own client: no `-o`).

## 3. The loop

```sh
PART=N sh cycle.sh            # kbuild -j 4 --features no-tests, boot, check, VERDICT PASS|FAIL
PART=N sh guest-down.sh       # when done (an enabled VM service takes the one kvm0 slot at every boot)
```

Only `/boot/akuma-guest-dev` is written; the host's `/boot/akuma-amd64*` are never touched. A candidate worth running on
the metal goes through `kinstall … /boot/akuma-amd64.verify` and the usual one-shot entry, by a human.

## 4. Claude Code with Remote Control

Already enabled on this box by the user; nothing in the overlay sets it up. The session that runs this loop can be
driven from the Claude app/web while `cycle.sh` builds and boots guests, which is what makes the loop hands-off. Note: a
long `kbuild` or a guest boot outlives an ssh drop, but a leaked `claude` keeps `/root/.claude.json.lock` — `kill -9` it and
`rmdir` the lock before starting another.

## Verify

- `guest-up.sh` prints `UP …` and `grep ^disk= /var/run/herd/vm-dev-part.vm` shows `/dev/nvme0n1pN`.
- `PART=N sh cycle.sh` ends `VERDICT PASS`; `gssh uname -a` names the build you just made.
- After `guest-down.sh`: `herd` lists `vm-dev-part` stopped, and the host's `uname -v` is unchanged.

## Traps

| symptom | cause |
|---|---|
| `EBUSY` / guest never starts | another VM holds `kvm0`; `guest-up.sh` stops the known ones — check `herd status` for any other |
| `refused: the spec's disk is not available` | PART is the host's partition, overlaps it, or is not a GPT partition number |
| no ssh round trip | read `/var/log/herd/vm-dev-part.log`; sshd must be `init=` in the guest's cmdline and the key in the image |
| `ssh: No such file` / `-o` read as a host | Akuma's ssh takes no `-o`; use `gssh` from `env.sh` |

## Background

[`docs/archive/AKUMA_KVM_DEV_CYCLE.md`](../archive/AKUMA_KVM_DEV_CYCLE.md), [`kvm-linux-guest.md`](kvm-linux-guest.md),
[`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) § "Working **on** the box".
