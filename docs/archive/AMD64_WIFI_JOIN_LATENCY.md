# ryzen: the wifi station takes minutes to join (open observation)

**Date:** 2026-10-22. **Status: OPEN — measured, not explained.** Written down for
further study at the user's request; nothing was changed.

## What was seen

Entry 28 (`rtw89wifi … kvmtest kvmlinux`, `smp=8`) booted on the ryzen metal and
the box was unreachable over wifi for several minutes: the Mac's ARP sweep for the
station's fixed MAC (`02:41:4b:55:4d:41`) found nothing for roughly five minutes
after the one-shot was armed, then the box answered on its usual `.159`. The same
shape was noted at every earlier ryzen boot (`overlays/ryzen/README.md`: "boot 2: 184 s
outside Linux").

The boot log of that boot (`klog-176.all` on p3; the kernel's 64 KiB `dmesg` ring
had already wrapped by the time anyone looked) says where the time goes after the
kernel is up:

| step | what the log shows |
|---|---|
| firmware load + `join1 (start)` | `fw ready … 46564 us`; `join1`: 18 337 ops, 36 ms |
| channel sweep | 13 `chan (switch)` lines, ~26 ms each, but every third costs **~110 ms** and has `polls … (1 timed out)` |
| scan | `scan: ~220–290 beacons, 24 networks` — **seven times** (about one round per 10–15 s) while `wifiauto` (`/bin/wifi auto`, herd service, `restart_delay` 5 s) runs |
| associate | `assoc: accepted, aid 1`, `join3 (associated)` 2.4 ms, then `DHCP configured`, roughly **70–80 s after sshd started** (sshd's `[PSTATS]` age was 83.6 s on the first line after DHCP) |

The radio itself is not slow: the join steps are milliseconds. The delay is the
repeated scan rounds that precede the association.

## Not ruled out (hypotheses, none tested)

- the known network is on a channel the first rounds do not dwell on long enough
  (24 networks, most on channel 1; the sweep spends the most time on the channels
  with a timed-out poll);
- `wifi auto` needs the target to appear in N consecutive scans, or backs off
  after a failed attempt, and the first attempts fail for a reason the log does not
  name;
- the 110 ms / `1 timed out` channel switches indicate a firmware-command timeout
  per third switch — each costs only 85 ms, but it is a symptom that something in
  the switch sequence is waiting on an event that does not come;
- unrelated to the KVM run: the guest runs inside `kvm::late` before `herd`, takes
  about a second of guest time, and is over before the first scan.

## Next step

Timestamp the scan rounds (the log has no wall clock before DHCP/SNTP) — a
`uptime_us` prefix on `[rtw] scan:` and on `wifi auto`'s decision lines would turn
"seven rounds, ~80 s" into a measured schedule — then read `userspace/wifi`'s
selection rule against it.

## Background

[`../../overlays/ryzen/README.md`](../../overlays/ryzen/README.md) (the wifi work, W0-W5) ·
[`AKUMA_AMD64_ON_RYZEN_LAPTOP.md`](AKUMA_AMD64_ON_RYZEN_LAPTOP.md)
