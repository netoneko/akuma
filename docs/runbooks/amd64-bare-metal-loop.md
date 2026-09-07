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
   ... hpbox.deploy()                       # box lands on a commit + your patch
   ... cargo build -p akuma-amd64 --target x86_64-unknown-none --release
   ... sh amd64/mkdisk.sh
   ... cp to /boot/akuma/{akuma-amd64,root.img}; grub-reboot "Akuma/amd64"
ssh -F /dev/null -p 22 root@192.168.1.123 "reboot"   # -> Akuma
ssh akuma "<test>"
```

**`/root/akuma` on the box is a git checkout** (since 2026-09-07), not the
snapshot the older parts of this runbook were written against. That is what
makes `hpbox.deploy()` possible and it is why there is no file-copying step
above any more.

`reboot -f`, not `reboot`: busybox `reboot` opens `/proc` to find init and
refuses without it. `/proc` exists as an empty directory now, but `-f` skips the
check entirely and is what makes this unattended.

A helper that knows both personalities lives at
[`scripts/utils/hpbox.py`](../../scripts/utils/hpbox.py): `which_system()`,
`wait_for()`, `reboot_to()`, `deploy()`, `ubuntu()`, `akuma()`, plus a CLI
(`python3 scripts/utils/hpbox.py which` / `wait akuma` / `ak '<cmd>'` /
`ub '<cmd>'` / `reboot-to ubuntu`). **Ask which system is running — never
assume.** Every confusing failure in this loop has started with talking to the
wrong one.

## Two machines at once (the fast lane)

**Run local QEMU and the box's Firecracker in parallel, not one after the
other.** They share nothing but the source, they take minutes each, and run
together the cost is the slower of the two instead of the sum — in practice the
local TCG boot, because the box builds and boots under KVM while the laptop is
emulating x86 on Apple Silicon.

```bash
python3 scripts/utils/amd64_trials.py --sync origin/<branch>
python3 scripts/utils/amd64_trials.py --smp 4 --grep 'block:'
python3 scripts/utils/amd64_trials.py --local-only        # laptop only
```

Exit status is 0 only if every trial that ran reported `0 failed`. A boot that
produced **no** tally is reported as `NO TALLY`, not as a pass: silence means it
hung or died, and the two must never read alike.

### Why both, and not just the faster one

They fail differently, and that is the point rather than a caveat:

| | local QEMU | box Firecracker |
|---|---|---|
| CPU | TCG, emulated | KVM, a real vCPU |
| entry | PVH, `-M microvm` | PVH |
| devices | virtio-MMIO, slirp | virtio-MMIO, the box's own disk |
| timing | wrong, and slow | close to the metal |

A change that breaks one and not the other is the interesting case. Timing bugs
and anything touching the scheduler or the clock will show on KVM and hide under
TCG; anything touching device discovery tends to do the reverse.

**Neither of these reboots the box.** Firecracker runs on the *Ubuntu*
personality, so the fast lane costs no reboot and does not disturb whatever is
running. Bare metal is a separate, slower step you take once the fast lane is
green:

```
fast lane  →  amd64_trials.py            (no reboot, ~minutes)
   then    →  hpbox.stage()              (build, image, arm GRUB)
   then    →  hpbox.reboot_to("akuma")   (the metal, ~a minute each way)
```

Going straight to the metal for a change the fast lane would have caught is the
single most expensive habit in this loop.

### Getting the source onto the box

**`hpbox.deploy()`.** One call, and it handles the case that used to need
judgement — a fix that is neither pushed nor committed:

1. fetch, and hard-reset the box to the newest local commit the remote has;
2. `git apply --3way` everything after that — unpushed commits and the working
   tree together, as one patch against the commit it just landed on.

So the box ends at *a named commit plus a named patch*, which is a state you can
report and reproduce. A hard reset is right here and nowhere else: `/root/akuma`
is a deployment checkout, never authored on, so there is nothing to destroy.
(The project rule against `git reset` is about the developer's repository.)

`git apply --3way`, not `patch -p1`: it understands renames and deletions and
merges a hunk whose context moved. `hpbox.send_files([...])` remains for when a
patch will not apply and you know exactly which files you want overwritten —
but note it **cannot express a deletion**, so a file you removed locally stays
on the box and keeps compiling.

The failure every one of these must avoid is the same one: sync too little, the
build succeeds against stale source, and the bug you just fixed is still there
while the evidence says it is not. `deploy` cannot fail that way quietly —
either the reset lands or it errors, either the patch applies or it conflicts.

One-time setup, already done: the box needs
`git config --global --add safe.directory /root/akuma`, without which git exits
128 with "detected dubious ownership" and prints **nothing to stdout** — so a
caller reading only stdout sees an empty success.

## Rules that cost time to learn

- **Never copy the whole tree.** Vendored submodules make it ~37 GB. Use
  `hpbox.deploy()`, which moves a commit id and a patch — bytes, not gigabytes.
- **`pkill -f <pattern>` over ssh kills your own session** when the pattern
  appears in the script you sent — it is in the argv. Use
  `for p in $(pgrep -x qemu-system-x86); do kill -9 $p; done` (comm truncates
  to 15 chars, so `-x qemu-system-x86` is the whole name).
- **The box's source tree is a snapshot, not a checkout.** It drifts. If a
  build fails on a symbol you just added, sync the crate, not just the file.
- **`cargo … | tail -3 && echo OK` always prints OK** — the pipeline's status is
  `tail`'s. Grep for `^error` instead.
- **cargo's freshness check reads mtimes**, so any transport that preserves the
  laptop's mtimes will lie to it.
  Syncing a file whose laptop mtime is *older* than the box's existing build
  artifacts leaves cargo convinced nothing changed, so it links the stale rlib.
  Measured 2026-09-06: `crates/akuma-cpu/src/lib.rs` was byte-identical on both
  sides (`md5sum` agreed) and visibly contained `pub fn invlpg`, and the build
  still failed with `cannot find function invlpg in module akuma_cpu::tlb` —
  because the *compiled* `akuma-cpu` was from before it existed. A source file
  you can `grep` for the symbol is not evidence the symbol is in the rlib.
  After any sync: `find crates amd64 -type f \( -name '*.rs' -o -name '*.toml' \)
  -exec touch {} +`.
- **The Akuma client key lives in `target/`, which is disposable.**
  `amd64/mkdisk.sh` generates `target/x86_64-unknown-none/release/amd64-ssh-test-key`
  once and stages its `.pub` into the image as `etc/sshd/authorized_keys`. So the
  key that opens a running Akuma is **whichever machine built that image** — and
  the box builds its own. A laptop `cargo clean` (or a first build on a new
  machine) silently regenerates the laptop's key and locks it out of the image on
  the box, with no symptom but `Permission denied (publickey)` from a host whose
  sshd is plainly answering. It is not a key you can re-derive: fetch the box's
  copy from `/root/akuma/target/x86_64-unknown-none/release/amd64-ssh-test-key`
  while Ubuntu is up. If Akuma is up and rejecting you, that file is unreachable
  and the only way back is a reboot into Ubuntu.
- **Firecracker's VM json can point at a tap that does not exist**, and the only
  symptom is one failed self-test: `net: the netpoll daemon is being scheduled`.
  Check `ip link show tap0` before believing a networking regression; the
  documented no-NIC baseline simply drops `network-interfaces`.

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

Until 2026-09-06 this **silently returned only the last 4 KiB**: `sys_syslog`
clamped every read to its staging buffer while `SIZE_BUFFER` advertised the full
ring, so `dmesg` asked for 64 KiB, got 4 KiB, and reported no short read. Fifteen
sixteenths of every boot was unreachable, and the `[rtl]` stall dumps push a lot
of bytes — a whole xHCI bring-up trace fitted inside what had already been lost.
If a diagnostic you know was printed is missing from `dmesg`, check you are
running a kernel newer than that before believing the kernel never printed it.

**Boot output is what fills the ring.** 64 KiB is roughly one boot; a long-running
box with a stalling NIC overwrites the boot in minutes. Read `dmesg` early, or
`ssh akuma "dmesg" > boot.log` on the laptop before poking at anything.

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

## The userspace probes — and why they are the ones to reach for

`userspace/forktest/c_stress/` holds ~40 small static C probes that have been
**calibrated against real Linux**: each one's header says what a correct kernel
prints, and most carry a `docker run --platform linux/arm64 … alpine /<probe>`
line so the same binary can be run on Linux and on Akuma and the two answers
compared. That is what makes them worth more than anything written fresh for a
bug — they encode what "correct" is, they were each written because something
here was silently wrong, and their verdicts are already known-good on two
kernels.

**They are architecture-neutral C.** Not one contains an `asm` block, a page-size
constant or a syscall number, so the only aarch64-specific thing about them was
the compiler name in the runner. Build them for this target with
`x86_64-linux-musl-gcc`; `scripts/mem_suite.py --arch x86_64` does it for you and
writes the binaries to `c_stress/x86_64/` (per-architecture subdirectories, so
the two arches cannot silently run each other's build).

Two transports, because this box has two rigs and they differ in what the guest
can reach:

```bash
# ssh transport — the QEMU guest, and the aarch64 devbox
SMP=1 SSH_PORT=2244 INIT=/bin/sshd sh amd64/run.sh &
python3 scripts/mem_suite.py --port 2244 --arch x86_64 \
    -i target/x86_64-unknown-none/release/amd64-ssh-test-key

# console transport — QEMU **and** the box's Firecracker, in parallel
python3 scripts/utils/amd64_mem_trials.py
python3 scripts/utils/amd64_mem_trials.py --smp 4 --only mmap_stress,cowstale
```

`hpbox.firecracker` boots with `"network-interfaces": []`, so **there is nothing
to ssh to on that arm** — the probes ride in on the disk image (`debugfs` writes
them to `/probes/`) and report on the serial console.
`scripts/utils/amd64_mem_trials.py` does that on both machines at once and
imports `mem_suite.verdict` rather than re-deriving it, so "did this probe pass"
has one definition across both transports. In particular a **silent probe is
never a pass** — that rule was learned the hard way and a second copy of it is a
second place to get it wrong.

### Reading a result on this target

The probes assume a Linux-complete guest, so several fail here for reasons that
have nothing to do with what they test. `amd64_mem_trials.py` carries that list
as `EXPECTED_FAIL` with a diagnosed reason for each, and reports a probe that
starts *passing* as a surprise rather than silently accepting it — an entry that
goes green means the gap it names has been closed and the entry should go.

As of 2026-09-07, after the `akuma-mmap` region table landed (B1/B2):

| probe | verdict | why |
|---|---|---|
| `mmap_stress` | **PASS** | |
| `madvshared` | **PASS** | |
| `shmanon` | **PASS** | was failing — `MAP_SHARED\|MAP_ANONYMOUS` now survives `fork` as one object |
| `cowstale` | **PASS** | |
| `mmapsum` | known | `pread64` (x86_64 17) is not implemented here |
| `mmap_file` | known | file-backed `mmap` is `ENOSYS` by design — no page cache |
| `mprotectlb` | known | needs a `SIGSEGV` handler; no signal delivery on this target |
| `mremapmove` | known | `mremap` is not implemented |
| `eager_mprotect_probe` | known | a killed child exits `128+SIGSEGV` instead of reporting a *signalled* status, so its `WIFSIGNALED` check never fires |
| `smapsdirty` | known | no `/proc/self/smaps`, no `MADV_FREE` |

The last two are worth reading twice, because both look like memory bugs and
neither is. `mprotect` **does** work here — verified directly:
`mmap` RW, touch, `mprotect(PROT_READ)`, write ⇒ the process dies with 139. What
those two probes actually need is *signals*, which is trunk A2.

### Two gaps in the guest shell that will bite any harness

Found while porting the suite, and neither is a memory bug:

- **`2>&1` fails.** Any command carrying it answers `/bin/sh: 1: Bad file
  descriptor` and returns 1, so a harness that appends it — as `mem_suite.py`
  used to — gets zero probes run and ten identical failures. The redirect was
  redundant anyway (both streams reach the same place); it is gone.
- **busybox here has no `base64` applet**, and the failure is silent in the worst
  way: `base64 -d > /tmp/x` leaves a **zero-byte** file, because the shell
  creates the redirect target before discovering the command does not exist.
  Every probe then "runs" and prints nothing. `mem_suite.push` probes for the
  applet once and sends raw bytes when it is missing — safe on both, since ssh
  with no `-t` allocates no pty and the channel is 8-bit clean.

There is also no `/dev` at all on this target, so `dd if=/dev/zero` cannot be
used to stage a test file; `mem_suite.stage` writes the bytes over the ssh
channel instead.

## Known-broken, so you do not rediscover them

| symptom | cause |
|---|---|
| `date` says 1970 → `apk`: *server certificate not trusted* | was: no wall clock. **Fixed** — `clock::sync_tick` keeps retrying SNTP until the clock sets itself. If `date` is still 1970, `ssh akuma "dmesg" \| grep clock:` for the reason |
| `cmd \| cmd`: *can't create pipe* | was: fds 0/1/2 were handled by number below `fd.rs`'s table (`FIRST_FILE_FD = 3`), so `dup2` onto them had nowhere to land — and `pipe`/`pipe2`/`dup2`/`dup3` were not dispatched at all. **Fixed 2026-09-06.** A bound 0/1/2 in the process's own descriptor row now wins over the by-number console default. Verified on the metal: a 20-stage pipeline runs |
| `ls`/`apk`: *Out of memory* | was: 64 MiB fixed heap, exhausted by `apk`'s file caches. **Raised to 512 MiB.** `ps`/`top` still need a real procfs; `free` works (`/proc/meminfo` is synthesised) |
| `wget https://` : *socketpair* | busybox shells out to `ssl_client`. Use `/bin/hget` instead — TLS in-process |
| `nslookup`: *Bad file descriptor* | `write()` on a connected UDP socket. DNS itself works (`wget http://…` resolves) |
| pings to `192.168.1.220` time out | `.220` is only the **pre-DHCP fallback**; a lease overrides it. The probe line says the real address |
| every Akuma boot crashes before sshd — even a known-good kernel — after a driver touched a bus-master device | a device left **running with DMA active** (an xHCI/AHCI controller whose bring-up faulted mid-way) keeps scribbling on RAM across a warm `reboot`; UEFI does not fully re-init it. **Fix: full power cycle** (hold the power button ~5 s, or pull the plug). A PCI driver here must (a) mask legacy INTx (`pci::enable_full(.., mask_intx=true)`) — an unmasked INTx lands on an unhandled IDT vector — and (b) `HCRST` / halt the controller on **every** bring-up error path. Since 2026-09-06 the kernel also defends itself: `xhci::quiesce_all` clears `BUS_MASTER` on every boot right after the PCI scan, and `xhci::shutdown` runs before the machine reset. Neither can save the boot whose image was *already* corrupted during load, so the power cycle stays the recovery |
| a "disarmed" GRUB entry still drove the USB controller | until 2026-09-06 the xHCI self-test was gated only on the controller being *present*, so dropping `root=/dev/sda1` stopped the kernel mounting the disk but not bringing the controller up. There was no way to boot that kernel without driving it. **Fixed** — the bring-up now needs `usb` or `root=/dev/sda1` on the command line, and says so in the verdict when it skips |

## The spare disk (persistence — USB/xHCI, working)

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

### Status on the metal (2026-09-06) — working

Bring-up **passes** on the real Intel controller: `209 passed, 0 failed`, and
with `root=/dev/sda1` the boot says `fs: ext2 mounted on sda1`. Persistence is
verified in both directions across a reboot — a file Ubuntu writes to
`/dev/sdb1` is read by Akuma, and a file Akuma writes is on the physical
partition when Ubuntu mounts it.

What had been wrong was the **port scan**, not the transport. `find_and_reset_port`
took the first connected port and stopped: on this box that is USB 2.0 port 8,
sitting in Polling with nothing usable on it, while the disk was on port 20,
connected and already enabled. It then tried to rescue port 8 with a **warm**
reset, a SuperSpeed-only bit that a USB 2.0 port ignores — hence a one-second
timeout and `port reset timeout` as the only symptom. Full account:
`docs/archive/AKUMA_AMD64_USB_XHCI.md`, last section.

Two userspace gaps stand between a persistent root and a comfortable one, and
**neither is a disk problem** — both fail the same way on the RAM image:

- ~~`echo x > file` returns ENOSYS and leaves a zero-length file.~~ **Fixed
  2026-09-06** with `dup2` — same fix as the `cmd | cmd` row above. `>>` was
  fixed with it and needed a second change: `open_flags` read neither
  `O_APPEND` nor `O_TRUNC`, so every `O_CREAT` open started from an empty
  buffer and `>>` silently behaved as `>`. That was unreachable while nothing
  could redirect, and a data-losing bug the moment something could.
- `mkdir` is ENOSYS — the syscall is not implemented. **Still open**, and now
  the most visible gap: `mkdirat` (258) *is* dispatched and works, so this is
  busybox calling the legacy `mkdir` (83), which is not in the table.

### Reading a bring-up that died

The verdict repeats every failed check by name under the tally (`FAILED: <name>`,
up to 16). That exists because this console has no scrollback: a suite of 200
checks scrolls the two that failed off the top long before the verdict appears,
and the tally alone says how many things are wrong and nothing about which.

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
