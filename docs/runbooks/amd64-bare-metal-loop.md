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
| `skiptests` | skip the ~200-check self-test suite, go straight to `init`. Still does the `init_*` calls the suite happens to also perform. For a trusted build, cuts a chunk off every reboot |
| `netprobe` | a live NIC status line every 2 s from inside the netpoll daemon. **Off by default now — `dmesg` over ssh replaces it and it scrolled the TV** |
| `nosmp` | single core. Quietens the `[BKL] stuck: cpu N …` chatter while cornering something |
| `ip=<addr>[/<prefix>][,<gw>[,<dns>]]` | override the built-in `192.168.1.220` for one boot |
| `strace` | trace every syscall (framebuffer only) |

Read the kernel log over ssh: `ssh akuma "dmesg"` (a 64 KiB ring in `serial.rs`,
served by `syslog(2)`). The `mem: heap …/… KiB, pmm … MiB free` line every 10 s
is in there, not on the TV. **Pipe on the laptop, not the guest** — `cmd | cmd`
still fails on the box — so `ssh akuma "dmesg" | grep mem:`.

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
| `date` says 1970 → `apk`: *server certificate not trusted* | was: no wall clock. **Fixed** — `clock::sync_tick` keeps retrying SNTP until the clock sets itself. If `date` is still 1970, `ssh akuma "dmesg" \| grep clock:` for the reason |
| `cmd \| cmd`: *can't create pipe* | fds 0/1/2 are handled by number below `fd.rs`'s table (`FIRST_FILE_FD = 3`), so `dup2` onto them has nowhere to land. Still open |
| `ls`/`apk`: *Out of memory* | was: 64 MiB fixed heap, exhausted by `apk`'s file caches. **Raised to 512 MiB.** `ps`/`top` still need a real procfs; `free` works (`/proc/meminfo` is synthesised) |
| `wget https://` : *socketpair* | busybox shells out to `ssl_client`. Use `/bin/hget` instead — TLS in-process |
| `nslookup`: *Bad file descriptor* | `write()` on a connected UDP socket. DNS itself works (`wget http://…` resolves) |
| pings to `192.168.1.220` time out | `.220` is only the **pre-DHCP fallback**; a lease overrides it. The probe line says the real address |
| every Akuma boot crashes before sshd — even a known-good kernel — after a driver touched a bus-master device | a device left **running with DMA active** (an xHCI/AHCI controller whose bring-up faulted mid-way) keeps scribbling on RAM across a warm `reboot`; UEFI does not fully re-init it. **Fix: full power cycle** (hold the power button ~5 s, or pull the plug). A PCI driver here must (a) mask legacy INTx (`pci::enable_full(.., mask_intx=true)`) — an unmasked INTx lands on an unhandled IDT vector — and (b) `HCRST` / halt the controller on **every** bring-up error path. Since 2026-09-06 the kernel also defends itself: `xhci::quiesce_all` clears `BUS_MASTER` on every boot right after the PCI scan, and `xhci::shutdown` runs before the machine reset. Neither can save the boot whose image was *already* corrupted during load, so the power cycle stays the recovery |
| a "disarmed" GRUB entry still drove the USB controller | until 2026-09-06 the xHCI self-test was gated only on the controller being *present*, so dropping `root=/dev/sda1` stopped the kernel mounting the disk but not bringing the controller up. There was no way to boot that kernel without driving it. **Fixed** — the bring-up now needs `usb` or `root=/dev/sda1` on the command line, and says so in the verdict when it skips |

## The spare disk (persistence — USB/xHCI, in progress)

`/dev/sda` is a spare 1 TB drive (Seagate ST1000LM035) in a **USB-to-SATA
enclosure** (ASMedia `174c:55aa`). The drive cannot move to SATA (screwed into a
caddy that will not open), so persistence is over USB. `sda1` (LBA 2048, 64 GiB)
is **ext2, label `AKUMA`** — formatted from Ubuntu 2026-09-06. Keep the enclosure
**off the USB hub** — straight into a rear port it does 134 MB/s and enumerates on
xHCI; behind the hub sustained writes drop it off the bus.

Driver: `akuma-xhci` + `akuma-usb-storage` (pure, host-tested) + `amd64/src/xhci.rs`
(MMIO/DMA). Two command-line tokens, and the difference matters on a machine
that crash-loops when this goes wrong:

| token | effect |
|---|---|
| `usb` | bring the controller up and run the self-test (READ CAPACITY, the MBR, the `sda1` superblock, a `WRITE(10)` round trip into `sda2`). Root stays the RAM image, so sshd comes up either way and you can read `dmesg`. **Start here** |
| `root=/dev/sda1` | the above, plus mount `sda1` as the persistent root. Falls back to the RAM image on any probe failure |

Neither token: the controller is not touched at all.

Full plan: `docs/archive/AKUMA_SELF_HEALING_PORT.md` § "A proper disk".

### Iterating the USB driver

**Do not iterate this driver on the metal.** A wrong bring-up costs a cold
reboot and a power cycle, and the failure it produces — a box that restarts
before printing anything — carries no information. Use the QEMU rig:

```sh
amd64/run-xhci.sh                 # q35 + qemu-xhci + usb-storage, log in target/
EXTRA=skiptests amd64/run-xhci.sh # straight to init
```

It builds its own fixture (`amd64/mkusbdisk.py`: MBR, ext2 `sda1` at LBA 2048,
scratch `sda2` at LBA 134217728, sparse so 64 GiB costs ~256 MiB), so all four
disk checks are live rather than skipped. It found the bug that had survived a
whole session of metal reboots — a Configure Endpoint command that also claimed
EP0, which the controller answers with `TRB Error` — on its first run.

What it models is a *correct* controller, so it catches every way the driver is
wrong about the spec and none of the ways a particular controller is wrong about
it. Past that, in increasing fidelity and all runnable on the box under KVM
without touching the metal's own boot:

| rig | what it adds | contained by |
|---|---|---|
| `-device qemu-xhci -device usb-storage,drive=/dev/sdb` | the **real partition table and filesystem** | the VM |
| `-device usb-host,vendorid=0x174c,productid=0x55aa` on `qemu-xhci` | the **real ASMedia enclosure** — its descriptors, stalls and quirks | the VM |
| `-device vfio-pci,host=00:14.0` | the **real Intel controller**, including the BIOS/SMM handoff | the IOMMU — which is strictly *more* protection than the metal has |

The last one needs VT-d on and the controller unbound from `xhci_hcd`, and is
the only rig that can reproduce a handoff or controller-quirk bug. It is also
the only one where a runaway DMA is caught rather than landing in RAM.

### Reading a bring-up that died

`init` announces each step *before* it runs it (`[xhci] .. <step>`), because
several of them can take the machine down in a way that reaches no exception
handler: a config-space write can reset the box, the BIOS handoff can enter SMM,
and a wrong DMA address makes the controller scribble on the page tables. **The
last line printed is the diagnosis.** It also prints the physical address of
every DMA structure — all must be non-zero and below 4 GiB, and a value around
550 GiB means `virt_to_phys` translated a `.bss` static through the physmap
window instead of the kernel-image window.

A process stuck in `D` state on the enclosure cannot be killed — reboot the box.

## Background

`docs/archive/AKUMA_AMD64_ON_HP_500_502NJ.md` — the whole bring-up, including
the three wrong diagnoses of the receive stall and what finally settled it.
