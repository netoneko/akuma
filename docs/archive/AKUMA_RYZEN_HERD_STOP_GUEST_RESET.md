# The ryzen reset on `herd stop goose-glm` (2026-11-02)

**Stability: C**: open, seen once, cause unknown.

## What happened

The ryzen was running Akuma (amd64 bare metal `f22e0d5d`, 192.168.1.159, up about 24,400 s). Two things were running
that both carry the name GLM, and they are not the same thing:

- herd's **`goose-glm` service**: `/usr/local/bin/goose-glm-serve`, goose as an ACP server (HTTP + WebSocket on :3284) on
  the Akuma **host**, `restart = true`.
- a **Taoxichuan guest** under Akuma's hypervisor: `herd vm-run /root/tx/glm/vm.spec` (pid 1433), started by hand, not a
  herd service, with goose and GLM inside it on forward 2224. Both of its relay slots were held by stuck ssh sessions.

To stop the guest, `herd stop goose-glm` was run over ssh to the host. That stops the host-side goose server, not the
guest. The ssh session dropped with "No route to host"; 192.168.1.159 stopped answering ping; about a minute later the
machine was up in Pop!_OS (systemd-boot's default), so it had reset, not just lost wifi.

## Evidence

- `/var/log/ryzen/klog-255.all` (klogd's copy of the console, flushed every few seconds) ends at 00:09 with routine
  `[PSTATS]`/`[FAULTSTAT]` lines, a `[herd] Reloading config...` and touchpad/wifi status: no panic, no `[Fault]`, nothing
  from herd about the stop. Whatever the kernel printed in its last seconds did not reach the disk.
- `[bkls>]` BKL-spin warnings precede the end, but there are 1,774 of them across the boot, so they are not a sign of
  this.
- `/var/log/ryzen/herd-255.log` ends with earlier `vm-dev` start/stop pairs, which worked the same evening.
- Read-only `e2fsck -fn` of the Akuma partition from Pop: HTREE-index errors on three directories (inodes 23, 37 and 329,
  `/var/log/ryzen`) and "large directories without `LARGE_DIR`". These look like Akuma's ext2 writing directories whose
  index flag it does not maintain, i.e. older than this reset; not repaired.

## So the trigger is a SIGTERM to a host process

herd stopping a service sends it SIGTERM. The process was goose (a tokio program with worker threads and an open
listening socket), on a host that had also been running a guest for 5,270 s. The same path (`herd stop`) on `vm-dev`
had worked. First thing to try: `herd stop goose-glm` on a freshly booted box, without a guest.

## Not done

No reproduction yet, and no fix. After the reboot back into Akuma the GLM guest's disk was updated with the guest down
(it is not a service, so it did not restart).

## Background

- [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md): the "No private syscall" row, which needed the image update.
- `docs/reference/subsystems/kvm.md`: VMs as services.
