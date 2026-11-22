# Main amd64 boot epoch (C7 investigation)

The gateprobe `time` check intermittently reports seconds since boot even when the
command line supplies `epoch=`. The main amd64 kernel did not consume that
parameter: it seeded from CMOS and attempted SNTP after DHCP. Unlike the Tokio
kernels, it had no command-line clock source. An unavailable RTC and an
unsuccessful boot-time network sync therefore leave realtime boot-relative.
The intermittent failure is the race between the probe's `time` check and the first SNTP answer on a boot whose CMOS
seed is absent or implausible; the gate passed whenever SNTP won it (about 6 boots in 7).

Both amd64 boot protocols now seed from a valid `epoch=` after the RTC seed and
before filesystem and init startup. SNTP still corrects the boot-supplied time;
invalid, zero, and overflowing values leave the existing clock unchanged. The
parser and initialization allocate nothing.

Verification: the parser's host tests and clippy pass; all 16 premerge builds passed. `scripts/gate.py --no-build --only
boot:akuma-amd64-gateprobe` in a loop: 40 consecutive boots, 0 `time` failures and `clock: seeded from epoch=` on every one
(7 of 40 failed before the change, 2026-11-22). The `akuma/amd64 time` flaky line is removed from `scripts/gate/xfail.txt` and C7
from `proposals/FUTURE_WORK.md`. The soak ran on the parent's machine (KVM); Codex's sandbox could not run it.

Background: [socket wait investigation](AMD64_SOCKET_WAIT_STARVATION.md).
