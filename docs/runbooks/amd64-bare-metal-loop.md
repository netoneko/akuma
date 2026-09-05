# The trashcan loop — editing Akuma/amd64 on real hardware without touching it

**Stability: B.** The loop itself is reliable; the machine it drives has a NIC
that needs restarting and no working wall clock.

The HP 500-502nj ("the trashcan", "the dumpster", "vaporwave") is one box that
boots two systems. This is how to change kernel code and see the result on real
silicon, from a laptop, with no keyboard and no photographs.

## The two personalities

| | address | how to reach it |
|---|---|---|
| **Ubuntu** — builds, stages, arms GRUB | `192.168.1.123:22` | `ssh -F /dev/null -p 22 root@192.168.1.123` |
| **Akuma** — the thing under test | `192.168.1.123:2222` | `ssh akuma` |

Same IP. `~/.ssh/config` has an `akuma` alias (port 2222, root, the test key,
no host checking) — so **plain `ssh root@192.168.1.123` reaches Akuma, not
Ubuntu.** Anything meant for the Ubuntu side must pass `-F /dev/null` or it
silently talks to the wrong operating system. That is not hypothetical: a build
once ran `cd /root/akuma` inside a kernel with no such directory and reported
`Function not implemented`.

Host checking is off for `akuma` on purpose: **sshd generates a new host key
every boot and nothing persists it**, so the fingerprint changes by design and a
`known_hosts` entry would be wrong rather than reassuring.

## The cycle

```
ssh akuma "reboot -f"          # Akuma resets itself -> Ubuntu (GRUB default)
   ... rsync changed files to root@192.168.1.123:/root/akuma/  (-F /dev/null!)
   ... cargo build -p akuma-amd64 --target x86_64-unknown-none --release
   ... sh amd64/mkdisk.sh
   ... cp to /boot/akuma/{akuma-amd64,root.img}; grub-reboot "Akuma/amd64"
ssh -F /dev/null -p 22 root@192.168.1.123 "reboot"   # -> Akuma
ssh akuma "<test>"
```

`reboot -f`, not `reboot`: busybox `reboot` opens `/proc` to find init and
refuses without it. `/proc` exists as an empty directory now, but `-f` skips the
check entirely and is what makes this unattended.

A helper that knows both personalities lives at
[`scripts/utils/hpbox.py`](../../scripts/utils/hpbox.py): `which_system()`,
`wait_for()`, `reboot_to()`, `push()`, `ubuntu()`, `akuma()`, plus a CLI
(`python3 scripts/utils/hpbox.py which` / `wait akuma` / `ak '<cmd>'` /
`ub '<cmd>'` / `reboot-to ubuntu`). **Ask which system is running — never
assume.** Every confusing failure in this loop has started with talking to the
wrong one.

## Rules that cost time to learn

- **Never rsync the whole tree.** Vendored submodules make it ~37 GB. Copy the
  files you changed: `rsync -a --relative <files> root@…:/root/akuma/`.
- **`pkill -f <pattern>` over ssh kills your own session** when the pattern
  appears in the script you sent — it is in the argv. Use
  `for p in $(pgrep -x qemu-system-x86); do kill -9 $p; done` (comm truncates
  to 15 chars, so `-x qemu-system-x86` is the whole name).
- **The box's source tree is a snapshot, not a checkout.** It drifts. If a
  build fails on a symbol you just added, sync the crate, not just the file.
- **`cargo … | tail -3 && echo OK` always prints OK** — the pipeline's status is
  `tail`'s. Grep for `^error` instead.

## Rigs on the box (no reboot needed)

Both run under KVM and exercise the *same* code, so most changes can be
validated without touching the metal at all:

- `/root/ovmf5.sh "<cmdline>"` — OVMF+GRUB q35, the **multiboot2 (bare-metal)**
  path. Serial to `/tmp/ovmf-serial.log`.
- `/root/qrun2.sh <log> "<cmdline>" [smp]` — microvm, the **PVH** path.
- `/root/taprun.sh <log> "<cmdline>" [smp]` — microvm on a **real tap**
  (`aktap0`, host `10.0.2.1/24`, guest `10.0.2.15`). The only way to test
  whether the kernel answers ARP/ICMP: QEMU's `-netdev user` cannot be pinged
  from the host at all. The tap has one consumer — kill the previous VM first.

What the rigs **cannot** test: the Realtek NIC (nothing emulates an RTL8168g),
and the PIT-based clock calibration on real timing.

## Boot options

```
multiboot2 /boot/akuma/akuma-amd64 init=/bin/sshd netprobe
```

| token | effect |
|---|---|
| `init=<path>` | what runs after the self-tests. `/bin/sshd` direct rather than `/bin/herd` — herd drains a service's stdout into a log file, so a supervised sshd fails *invisibly* on a framebuffer-only console |
| `netprobe` | a live NIC status line every 2 s from inside the netpoll daemon |
| `nosmp` | single core. Quietens the `[BKL] stuck: cpu N …` chatter while cornering something |
| `ip=<addr>[/<prefix>][,<gw>[,<dns>]]` | override the built-in `192.168.1.220` for one boot |
| `strace` | trace every syscall (framebuffer only) |

There is **exactly one** Akuma GRUB entry (`/etc/grub.d/45_akuma`), on purpose:
with three of them, a `grub-reboot` armed for one booted another, and the
`next_entry` was set *and* consumed. One entry means a one-shot resolves to it
or to Ubuntu, and the screen says which.

## Reading the probe

```
[probe] t=14s ticks=1484(cal) link=up/1000M/full ip=192.168.1.123/24 dhcp=leased
[probe]   rx=19 tx=4 drop=0 isr=0x4085 dry=0 kicks=1 polls=8 posted=0 rxfail=0 irq=0 laps=2837558
```

- `(cal)` vs `(GUESS)` — whether the LAPIC was calibrated against the PIT. A
  `GUESS` clock is ~6x fast and every network timeout is scaled by it.
- `ticks` frozen while `laps` climbs = the clock stopped; `laps` frozen = the
  scheduler stopped running netpoll; neither = the kernel died. Those three are
  indistinguishable without both numbers.
- `rx` stuck at exactly **16** is the known receive stall (16 = `RING_LEN`).
- `kicks=N` climbing means the machine is reachable **despite** that bug.

**Do not use `busybox ifconfig` to answer a network question here.** Its packet
counters come from `/proc/net/dev`, which this kernel fills with literal zeros —
it reads identically on a dead NIC and a busy one.

## Known-broken, so you do not rediscover them

| symptom | cause |
|---|---|
| `date` says 1970 → `apk`: *server certificate not trusted* | no wall clock; **every** cert is not-yet-valid. Check `date` before the CA bundle |
| `cmd \| cmd`: *can't create pipe* | fds 0/1/2 are handled by number below `fd.rs`'s table (`FIRST_FILE_FD = 3`), so `dup2` onto them has nowhere to land |
| `ps`, `top`, `free` | no procfs; `/proc` is an empty directory |
| `wget https://` : *socketpair* | busybox shells out to `ssl_client`. Use `/bin/hget` instead — TLS in-process |
| `nslookup`: *Bad file descriptor* | `write()` on a connected UDP socket. DNS itself works (`wget http://…` resolves) |
| pings to `192.168.1.220` time out | `.220` is only the **pre-DHCP fallback**; a lease overrides it. The probe line says the real address |

## Background

`docs/archive/AKUMA_AMD64_ON_HP_500_502NJ.md` — the whole bring-up, including
the three wrong diagnoses of the receive stall and what finally settled it.
