# overlays/ryzen-akuma-dev — develop Akuma on Akuma, guests on KVM

**This machine only** (`akuma-ryzen`, entry with `kvmctl`); generalize later. Procedure:
[`docs/runbooks/kvm-dev-loop-on-akuma.md`](../../docs/runbooks/kvm-dev-loop-on-akuma.md).

The loop: edit and build the kernel (or taoxichuan) **inside Akuma**, boot the result as a guest of
Akuma's own hypervisor (`/dev/kvm0`) with a **second NVMe partition as the guest's root**, check it over
ssh, and only then consider a real reboot. Claude Code on the box already has Remote Control enabled
(by the user), so the session can be driven from the app while it builds; nothing here sets that up.

| file | runs on | does |
|---|---|---|
| `env.sh` | Akuma | settings every script sources: `PART` (**no default**), ports, paths |
| `vm-dev-part.conf.tpl` | Akuma | the herd service template: `vm_disk = /dev/nvme0n1p@PART@`, outbound, 2 vCPUs, forward `2224:22` |
| `guest-up.sh [ELF]` | Akuma | stop every other VM, install the kernel to `/boot/akuma-guest-dev`, render and start `vm-dev-part`, wait for an ssh round trip |
| `guest-down.sh` | Akuma | stop and disable the guest |
| `cycle.sh [kbuild args]` | Akuma | `kbuild` → `guest-up.sh` → ssh check → host fault grep → `VERDICT PASS/FAIL` (report in `/root/dev-cycle/ryzen-akuma-dev/`) |
| `seed-part.sh IMG N --yes-destroy-partition` | **Pop**, root | `dd` a root image onto the partition and grow it. The only step that cannot happen inside Akuma (no nvme node) |

Open items (nothing here has been run yet): which partition number; `taoxichuan` as the guest kernel (set `GUEST_KERNEL`/pass its ELF).
