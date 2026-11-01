# The ryzen reset when `herd stop goose-glm` stopped a running Taoxichuan guest (2026-11-02)

**Stability: C**: open, seen once, cause unknown.

## What happened

The ryzen was running Akuma (amd64 bare metal, 192.168.1.159) with one guest under Akuma's hypervisor: herd's
`goose-glm` service, a `herd vm-run /root/tx/glm/vm.spec` process (pid 1433) for the Taoxichuan K3 guest (4 vCPUs,
3 GiB, forward 2224). Akuma's uptime was about 24,400 s. Both of the relay's forwarded-connection slots to the guest were
held by stuck ssh sessions (`host_send_blocked` in the millions on each), so a new ssh to 2224 got no banner.

`herd stop goose-glm` was run over ssh to the host. The ssh session dropped with "No route to host"; 192.168.1.159 stopped
answering ping; about a minute later the machine was up in Pop!_OS (its default boot entry), so it had reset, not just lost
wifi.

## Evidence

- `/var/log/ryzen/klog-255.all` (klogd's copy of the console, flushed every few seconds) ends at 00:09 with routine
  `[PSTATS]`/`[FAULTSTAT]` lines, a `[herd] Reloading config...` and touchpad/wifi status: no panic, no `[Fault]`, nothing
  from herd about the stop. Whatever the kernel printed in its last seconds did not reach the disk.
- `[bkls>]` BKL-spin warnings precede the end, but there are 1,774 of them across the boot, so they are not a sign of
  this.
- `/var/log/ryzen/herd-255.log` ends with earlier `vm-dev` start/stop pairs (stopping `vm-dev`, also a VM, had worked the
  same evening).
- Read-only `e2fsck -fn` of the Akuma partition from Pop: HTREE-index errors on three directories (inodes 23, 37 and 329,
  `/var/log/ryzen`) and "large directories without `LARGE_DIR`". These look like Akuma's ext2 writing directories whose
  index flag it does not maintain, i.e. older than this reset; not repaired.

## What is different from the stops that worked

The guest had two relay connections open and blocked (`host_send_blocked`), and had been up 5,270 s. `vm-dev`'s stops that
evening had no stuck relay connections. A stop that tears down a relay with blocked sends is the first thing to try.

## Not done

No reproduction yet, and no fix. The GLM guest's disk image still carries the old sshd (the sshd swap that needed the stop is
why it was stopped); `/root/tx/rs.goose.orig` was updated before the stop.

## Background

- [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md): the "No private syscall" row, which needed the image update.
- `docs/reference/subsystems/kvm.md`: VMs as services.
