# Handoff prompt: kami frame latency and the tumblr deaths — round 5

Paste everything below the line into a fresh session started in this repo, on
the `kami` branch.

---

You're working in the Akuma kernel repo (`~/github.com/netoneko/akuma`), on the
`kami` branch. kami (`userspace/kami`) is a headless Chromium on the Linux
framebuffer; Chromium 152 runs on the amd64 kernel on the ryzen laptop's metal
and under Firecracker on the same laptop's Pop!_OS side. Round 4 made local
pages stable (Fixes 29-30). Round 4b (2026-10-09 evening) measured where the
key-to-frame time goes and tried to reproduce the tumblr deaths. Read these
first, in this order:

1. `userspace/kami/README.md` § "Frame latency: where the 120 ms goes" and
   § "Tumblr under Firecracker: the rig, and what did not reproduce".
2. `docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md` Fix 31 and the "Open"
   table (the last five rows are this round's).
3. `userspace/forktest/c_stress/wakelat.c` and `timerlat.c` headers, and
   `userspace/kami/probe/akuma/run-net.sh` (the Firecracker-with-network rig).

## Rules (from `CLAUDE.md` and the user; they override everything below)

- Justify every allocation; console output only through `safe_print!`/`tprint!`.
- No fork/multi-agent fan-out; never launch background agents without asking.
- You may commit checkpoints and push them to `litter` (the private
  `akuma-litter` remote), branch `kami`. Never push to `origin`, never rewrite
  history. Leave the user's untracked `proposals/` and
  `docs/handoff-kernel-rio-support.md` alone.
- Every behaviour change gets a probe run on **Linux first** (the same static
  binary runs on the ryzen's Pop!_OS, same CPU), then on Akuma. Never claim
  what you didn't run.
- The ryzen laptop is the user's; **do not reboot it** without asking. When it
  is in Akuma it answers ssh on port 2222 on a wifi DHCP lease (sweep for the
  `SSH-2.0-Akuma_0.1` banner; it was 192.168.1.159); when it is in Pop it
  answers `root@192.168.1.126:22`. Only one is up at a time. Entry 12/14 runs
  `netwatch`, which reboots Akuma into Pop after 120 s without a DNS answer —
  a tumblr load over the 65 KB/s wifi can trip it.

## The rig (ryzen Pop!_OS side, `root@192.168.1.126`)

`/root/cdp-probe/new/`: `kami-root.img` (Alpine `latest`, Chromium 152, python3,
`cdp.py`), the kernels `/root/cdp-probe/akuma-amd64.{head,fd1024}`, the
in-guest scripts and `run-tumblr.sh` (= the repo's `run-net.sh`). One run:

```
cd /root/cdp-probe/new
KERNEL=/root/cdp-probe/akuma-amd64.fd1024 SCRIPT=kami-once.sh KARGS=strace_err \
  VCPUS=8 MEM=6144 DUR=60 URL=https://www.tumblr.com/ KEYS=j,j,j,j sh run-tumblr.sh
```

`SCRIPT` is `tumblr-once.sh` (CDP driver, every event), `kami-once.sh` (kami
itself on a pty) or `probes-once.sh` (`wakelat` + `timerlat`). Artifacts land in
`out/` (`dmesg.txt`, `shot.png`, `chromium.stderr`, `kami-input.log`,
`kami.log`); the whole serial log is `tumblr-fc.log` (the `dmesg` ring is only
64 KB — grep the serial log). After a Pop reboot: `iptables -I FORWARD -i tap0
-j ACCEPT; iptables -I FORWARD -o tap0 -j ACCEPT` (the `akuma-dnsmasq`
container restarts by itself). Firecracker is `/home/netoneko/bin/firecracker`,
not on root's PATH. A wifi-like link: `tc qdisc add dev tap0 root netem rate
600kbit delay 30ms loss 3%` (and `del` after). Push a kernel built on the Mac
with `cargo build -p akuma-amd64 --target x86_64-unknown-none --release` by
`scp` to `/root/cdp-probe/`. Linux control for anything in-guest:
`docker run --rm -v ...:/p akuma-cdp-probe-new <cmd>` on the same box.

## Where it stands

- **Latency, measured** (same CPU, same Chromium, same kami, local page):
  Linux 72 ms median key->frame, Akuma metal 120, Akuma/KVM 160. Cross-thread
  wakes are fast (3 us). **Timed futex waits overshoot by a full 10 ms tick**
  (1 ms -> 10.0 ms median, 16 ms -> 26.3; Linux +0.12 ms); `nanosleep`/
  `epoll_pwait` overshoot 0.2-1.3 ms. Path: `amd64/src/futex.rs::wait` ->
  `sched::block_until_deadline` -> `akuma_threading::schedule_blocking`, tick
  resolution, and the data says one tick *beyond* the rounded-up deadline.
- **Tumblr**: loads under Firecracker (6/6 runs, 4 and 8 vCPUs, kami itself,
  throttled lossy tap, both kernels). On the metal the browser died twice
  (`SIGSEGV` at navigation of a 12-minute-idle browser; "exit status 191" 4 s
  after commit) and then produced no frames. Not reproduced off the metal.
- **Fixed in tree, uncommitted at the time of writing only if the commit
  below did not land**: `MAX_FDS` 256 -> 1024 (Fix 31); `scroll_try.py`
  scores frames in line order (cross-core clock skew gave false FAILs).

## This round, in order

1. **Timed waits.** Make `FUTEX_WAIT_BITSET`/`FUTEX_WAIT` with a deadline land
   within ~1 ms of it. First explain the extra tick (`timerlat` 16 ms -> 26 ms:
   is the deadline compared before or after the tick advances `uptime_us`?
   does the wake pass run before the scheduler picks?). Then the real fix:
   a one-shot LAPIC timer (or TSC-deadline) armed for the earliest pending
   deadline on the core, so a halted core wakes *at* the deadline, not at the
   next 10 ms tick. `amd64/src/lapic.rs` is periodic-only today. Gate:
   `timerlat` under Firecracker then on the metal (expect every row's median
   overshoot under 0.5 ms), then `probe/scroll_try.py`/`stable_try.py` on the
   metal: the target is key->frame at or below 90 ms. Watch `wakelat`'s
   `futex+busy` p90 (1.9 ms on the metal) for the no-wake-IPI cost; an IPI on
   wake is the second half of the same work.
2. **The declined signal frame** (`[signal] sig 5 declined: frame write to
   0x100002e38 failed`): why does `threading::get_sigaltstack(tid)` return a
   stack inside a read-only file mapping for that thread? Candidates: the
   slot's altstack not scrubbed on the `[unregister] stale tid` path, or a
   fork child inheriting the parent thread's altstack by slot. Write a probe
   (`sigaltstack` + `SA_ONSTACK` + a raised `SIGSEGV`, across `fork` and
   across `pthread_create`/exit churn), Linux first. If a delivery can be
   declined for a *handled* `SIGSEGV`, that is a browser death on its own.
3. **Reproduce the metal deaths without the metal.** Differences left: the
   real wifi driver's RX path (`rtw89`) instead of virtio-net, the framebuffer
   console, `smp=8` on real cores, and an idle-for-minutes browser being asked
   to navigate (the first death). Try the last under Firecracker first (start
   kami, wait 12 min, navigate). For the metal, ask the user before any boot;
   `klog-N.dmesg` on p3 is the only kernel record and it is a 64 KB ring,
   so a death is lost within ~2 min — raise that ring or save it more often
   before the next metal session.
4. Then the open table in the Chromium record.
