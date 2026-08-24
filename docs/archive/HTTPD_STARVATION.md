# httpd starves sshd: two herd services, only the first one works

**Date:** 2026-08-25
**Status:** OPEN — reproduced twice, root cause not established.
**Where:** AWS `m6g.metal` Firecracker host, `overlays/devbox-firecracker-aws`
image, kernel `2c1eb9d0` (`platform-firecracker`, smoltcp, 1 vCPU, 1024 MiB).

## Symptom

Enabling a second herd service made the first one stop answering, without an
error anywhere. Adding `/etc/herd/enabled/httpd.conf` alongside the existing
`sshd.conf`:

- **httpd worked.** `curl http://10.0.2.15:8080/` returned `404` in 0.25 ms
  (404 is correct — that image ships no `/public`).
- **sshd did not.** The TCP handshake completed, so every reachability check
  passed, but the server sent **zero bytes**: `nc 10.0.2.15 22 | od -c` printed
  an empty dump, and `ssh -vv` stalled forever right after
  `debug1: Local version string SSH-2.0-OpenSSH_9.9`, having never received a
  peer banner.

The port checks passing is what makes this expensive to diagnose: `nc -z`,
`ping` and the security group all say healthy, because smoltcp completes the
handshake on the listening socket before anything calls `accept`. The service
looks up from the outside and is dead from the inside.

## The log difference

Both processes spawn and both bind. Nothing errors.

```
[herd] Userspace supervisor starting...
[AS-NEW] pid=118 l0=0x943dd000 asid=0x55 via=spawn
[AS-NEW] pid=119 l0=0x94503000 asid=0x56 via=spawn
[syscall] bind(fd=3, port=8080, ip=0.0.0.0)
[syscall] bind(fd=3, port=22, ip=0.0.0.0)
```

Then silence — no further herd output at all.

With `httpd.conf` moved to `/etc/herd/available/`, the same image on the same
boot path produces the lines that were missing:

```
[herd] Userspace supervisor starting...
[herd] Starting service: sshd
[herd] Started sshd (pid=
[syscall] bind(fd=3, port=22, ip=0.0.0.0)
[herd] Reloading config...
```

and the banner arrives at once:

```
$ nc 10.0.2.15 22 | head -c 40
SSH-2.0-Akuma_0.1
```

**`[herd] Starting service:` and `[herd] Started` are absent in the two-service
case even though both processes demonstrably spawned and bound.** Whatever herd
does after the spawn did not complete for *either* service — including httpd,
which nonetheless served HTTP correctly. So the missing bookkeeping is not
simply "herd stopped at the second entry".

## What the ordering suggests

httpd binds 8080 **before** sshd binds 22 (`enabled/` is read in directory
order, and `httpd.conf` sorts ahead of `sshd.conf`). The service that binds
first is the one that works. That is consistent with a readiness/wake problem
rather than a spawn problem: both tasks are alive, one of them never becomes
runnable on its `accept`.

Candidate causes, none confirmed:

1. **Netpoll wake with two listening sockets.** The wait loop
   (`crates/akuma-net-yarn`, driven by `akuma_net::socket::wait_until` and the
   three `poll.rs` entry points) may deliver the readiness wake to only one
   listener. This is the hypothesis the ordering fits best, and
   `docs/reference/subsystems/syscalls/poll.md` § "The wait loop is one machine"
   is where to start.
2. **herd's supervision loop blocking.** The absent `Started` lines say herd
   itself did not get through its post-spawn path; if herd blocks, its
   `restart`/`max_retries` bookkeeping never runs either.
3. **`start_delay_ms` interaction.** Both services carried
   `start_delay_ms = 10000`; two concurrent delayed starts is a combination herd
   had never run before, since `sshd.conf` was the only enabled service.

Hypothesis 1 and 2 are distinguishable: if herd is blocked, a service started by
hand (not by herd) alongside a running httpd will still work.

## Reproduce

On a devbox-profile image, with `httpd` present in `/bin`:

```bash
# 1. Both enabled -> ssh hangs, http answers.
cat > /etc/herd/enabled/httpd.conf <<'CONF'
command = /bin/httpd
args = 8080
start_delay_ms = 10000
restart = true
max_retries = 0
CONF
# boot, then from the host:
curl -s -o /dev/null -w '%{http_code}\n' http://10.0.2.15:8080/   # 404 -- fine
nc 10.0.2.15 22 | head -c 40                                      # EMPTY -- the bug

# 2. Move it aside -> ssh works, on the same image.
mv /etc/herd/enabled/httpd.conf /etc/herd/available/
```

The image can be edited in place on a Linux host without a rebuild:

```bash
mount -o loop /opt/akuma/guest/akuma-devbox.img /mnt/akuma-chk
mv /mnt/akuma-chk/etc/herd/enabled/httpd.conf /mnt/akuma-chk/etc/herd/available/
sync && umount /mnt/akuma-chk
```

## Next steps

1. Start httpd **by hand** from an ssh session while sshd is running. If both
   then work, herd is implicated (hypothesis 2) and the kernel is not.
2. Swap the filenames so sshd binds first (`10-sshd.conf` / `20-httpd.conf`).
   If sshd then works and httpd hangs, the defect follows bind order and
   hypothesis 1 is confirmed as a readiness bug, not an httpd bug.
3. Two listeners in one process, or two hand-started processes with no herd at
   all, separates "herd" from "two listening sockets" outright.

Until this is understood, **`overlays/devbox/rootfs/etc/herd/enabled/httpd.conf`
should not ship enabled** — it costs the image its ssh access, which is the only
way in.

## 2026-08-25 update: QEMU repro of a harder sibling (same trigger)

Next step 1 above was run, in QEMU instead of Firecracker, and produced a
**harder failure with the same trigger** (second server started after a listener
is already bound):

- **SMP=1, QEMU HVF, `devbox-smoltcp,no-tests`, devbox.img, herd/sshd running**
  — starting `/bin/httpd <port>` and then any further server (`nginx`, a second
  `httpd`) from the SSH session froze the **whole kernel**: no ticks, no
  heartbeats, no panic, QEMU parked at ~3 % CPU. 3/3 freezes; the freezing
  syscall varied (`execve`, port-conflict `bind`).
- **SMP=4, same image, same day** — sshd + nginx + httpd + redis (four
  listeners) all ran and answered; no freeze. Single-core is the common factor
  with this doc's Firecracker host (1 vCPU).

Full conditions, freeze sites, what is ruled out (nginx itself, port conflict,
the 2026-06 HVF `isv` fix regressing), and the **GDB diagnosis** are in
[`SECOND_LISTENER_SMP1_FREEZE.md`](SECOND_LISTENER_SMP1_FREEZE.md): the frozen
thread is a socket waiter parked in `idle_halt` (the yield-less
`blocking_relax_net`) holding the only core preempt-disabled — ticks deliver,
the wake-pass runs, but the switch is suppressed and no running thread remains
to raise a voluntary reschedule. Same trigger, same single-core condition as
this doc's Firecracker host; whether the milder starvation here is the same
mechanism is the doc's remaining follow-up.

## Background

Found while deploying `main` (`2c1eb9d0`) to the AWS Firecracker host and adding
port 8080 access. The same deploy fixed the missing busybox applet links in the
`devbox-firecracker-aws` image (`overlays/devbox-firecracker-aws/build-rootfs-image.sh`) —
unrelated, but the two were diagnosed in the same session, and the applet fix is
why the guest had a shell to test any of this with.

Related: `docs/reference/subsystems/syscalls/poll.md`,
`docs/archive/AKUMA_FIRECRACKER_TERRAFORM.md`.
