# overlays/ryzen — Akuma/amd64 on the Lenovo laptop, by reboot loop

ryzen is an IdeaPad 5 2-in-1 16AHP9 (Ryzen 7 8845HS, 14 GiB RAM, SK hynix NVMe,
RTL8852CE wifi, no Ethernet, no serial port). It runs Pop!_OS, booted by
**systemd-boot**. Akuma is a **one-shot guest** on it: arm it from Pop and
reboot. Akuma boots once, runs with its root on the laptop's own SSD, writes
its kernel log there, and resets itself. Any reset, whether Akuma's own, the
watchdog's or the power button, lands back in Pop, where the log is read.

Why this shape, and the wifi work it exists for:
[`docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`](../../docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md).

## Status (2026-10-06)

| | |
|---|---|
| one-shot boot, back to Pop by itself | **works**, unattended (boot 2: 184 s outside Linux) |
| root on NVMe p3 (ex-Windows, 64 GiB ext2), logs in `/var/log/ryzen` | **works**, `e2fsck` clean, even after a hard power-off |
| framebuffer (1920×1200 at `0x4b0000000`) | works |
| hardware watchdog (`wdt`, AMD FCH) | **works**: a deliberately wedged kernel (`arm.sh 6`) was reset by the chipset in 60 s, `FIRED=1` seen from Pop |
| RTL8852CE firmware on p3 (`/lib/firmware/rtw89/`) | staged (`fetch-firmware.sh`); nothing loads it yet |
| wifi W1: firmware download (`rtw89`, entry 8) | **works**: `[rtw] fw ready v0.27.122`, 166 packets in 50 ms, card shut down again; the fix that got it there was Bus Master on the card's root port (survey doc § 5.2) |
| wifi W2: receive (`rtw89rx`, entry 9) | **works**: Linux's recorded start replayed (16 815 ops, 52 ms), then 12 s on channel 1: 1604 frames, 17 networks' beacons, the home network's among them (survey doc § 5.3) |
| wifi W3/W4: the station (`rtw89wifi`, entry 10) | **works**: joins the home network with WPA2-PSK, handshake in the kernel, keys installed (boot 16, survey doc § 5.4) |
| wifi W5: IP over wifi (entry 11) | **ssh works**: DHCP, SNTP, DNS, `ssh -p 2222 root@<address>` from the Mac (boot 20, survey doc § 5.6); outbound TCP refused (open) |
| wifi W0: Linux's bring-up traced | **done**: probe + interface-up through `fw ready` (`w0-trace.sh`, results in the survey doc § 5.1; traces in `~/.akuma/w0/` on the laptop) |
| wifi control (`/dev/wifi0`, `/etc/wifi`, `wifi`) | **works** against the simulated radio (entry 7, rehearsed); no real radio yet — [`docs/reference/subsystems/wifi.md`](../../docs/reference/subsystems/wifi.md) |
| network | **wifi** (entry 11): `python3 overlays/ryzen/wifi-ssh.py --key <key>` finds the station by its MAC and runs a command over ssh (port **2222**); the user's key is in p3's `/etc/sshd/authorized_keys`. Every other entry has no network; the log is the channel |
| wifi credentials | `~/.akuma/wifi/<network>` on the laptop holds the passphrase; never copied into the repo, docs or logs |

## Quick start

All of it runs on ryzen; `$W` is `/home/netoneko/akuma-metal`.

```sh
sh overlays/ryzen/send.sh                                    # on the laptop, only if work is unpushed
ssh ryzen 'runuser -u netoneko -- sh /home/netoneko/akuma-metal/build.sh'
ssh ryzen 'cd /home/netoneko/akuma-metal/akuma && DISK=nvme sh overlays/ryzen/qemu.sh 0 240 std'   # rehearse — must pass
ssh ryzen 'sh /home/netoneko/akuma-metal/install.sh'
ssh ryzen 'sh /home/netoneko/akuma-metal/akuma/overlays/ryzen/arm.sh'     # boots Akuma once; ~3 min later Pop is back
ssh ryzen 'mount -o ro /dev/nvme0n1p3 /mnt && ls /mnt/var/log/ryzen; umount /mnt'
```

**Rehearse before every metal boot.** A rehearsal costs two minutes on ryzen
itself. A metal boot that wedges costs someone walking to the laptop. Every bug
found so far on this machine was found by the rehearsal first, or could have been.

## Files

| file | runs on / as | does |
|---|---|---|
| `send.sh` | laptop | ships unpushed work (`amd64 crates userspace/herd overlays/ryzen`) as `$W/local.tar`, plus `build.sh`/`install.sh` |
| `build.sh` | ryzen, netoneko | fresh clone of `$BRANCH` (`ryzen-wifi`), `local.tar` over it, both kernels (`no-tests` and plain), `mkdisk.sh` root image, then `remove.list` and `rootfs/` applied to it. Log `$W/build.log`, output `$W/out/` |
| `qemu.sh [entry] [timeout] [std\|bochs]` | ryzen, root | the rehearsal (below) |
| `bisect.sh <entry> <commit>…` | ryzen, root | older kernels against today's root image in the rehearsal, one verdict line each |
| `install.sh` | ryzen, root | kernels + root image to `/boot/efi/EFI/akuma/`, `grub.cfg` embedded into a standalone `grubx64.efi`, a `grubenv`, and the systemd-boot entry `akuma.conf`. **Arms nothing** |
| `arm.sh [entry]` | ryzen, root | boot Akuma **once**: systemd-boot's one-shot picks Akuma, and with an index GRUB's one-shot (`next_entry` in `grubenv`) picks the menu entry. Both are consumed by that boot |
| `format-p3.sh --yes-destroy-p3 [size]` | ryzen, root | **destructive**: the root image onto `nvme0n1p3`, grown with `resize2fs`. Refuses unless start/length/PARTUUID match the measured partition and it is unmounted. Done once, 2026-10-06, at 64 GiB |
| `fetch-firmware.sh` | ryzen, root | `rtw8852c_fw*.bin` from Alpine's `linux-firmware-rtw89` (no dependencies; files are `.zst`, decompressed here) onto p3, with Realtek's licence beside them |
| `wdt-probe.py` | ryzen, root, from Pop | **read-only** dump of the FCH watchdog and PM registers (`/dev/mem`): decoded? disabled? running? fired? |
| `wifi-ssh.py --key KEY [CMD]` | laptop | finds Akuma on the LAN by the station MAC (ping sweep + ARP) and runs CMD over ssh on port 2222; for entry 11 |
| `cycle.py <entry> [--grep RE] [--log dmesg] [--transcript NAME]` | laptop | one loop cycle through `hpbox.py`: ship this tree's `no-tests` kernel and `grub.cfg`, rehearse the entry in QEMU (stops if it fails), install, arm, wait for Pop, print matching `boot-N.early` (or `boot-N.dmesg`) lines from p3 (default `[rtw]`), and the service's `NAME-N.txt` |
| `w0-trace.sh [--check]` | ryzen, root | wifi **W0**: mmiotrace of rtw89 unbind → bind → up → one scan, to `/var/tmp/akuma-w0/<stamp>/`. Detaches into unit `akuma-w0` (drops Pop's wifi ~1–2 min, takes all CPUs but one offline while tracing); NetworkManager is kept off the card so no association or keys enter the trace; every exit path restores the network. `--check` changes nothing |
| `w2-merge.py <run> [--phase P] [--collapse] [--no-fwdl] [--ts]` | laptop | a W2 run's register accesses, H2Cs and C2Hs in one ordered stream (each H2C anchored to its CH12 doorbell) |
| `w2-seqgen.py <merged> <out.seq> [--from-fw-ready] [--until-stop] [--mac M]` | laptop | compiles a merged recording into the op stream `akuma_rtw89::script` replays (`crates/akuma-rtw89/seq/up.seq`) |
| `w0-summary.py <trace> [--dump PHASE]` | laptop | per-phase read/write counts and busiest BAR offsets of a W0 trace; `--dump bind` prints the ordered sequence |
| `grub.cfg` | — | the menu (5 s), below |
| `remove.list` | — | paths deleted from the image: the framebuffer `console` service (not wanted here) |
| `rootfs/` | — | applied to the image: the `autoreboot`, `wifitest` and `wifijoin` herd services (`etc/herd/available/`) and their scripts (`etc/ryzen/`). **Not** applied to p3 by anything: p3 was formatted from an older image once, so a service a menu entry names on p3 is copied there by hand from Pop (`wifijoin` and `/bin/wifi` were, 2026-10-06) |

## Menu

| # | entry | comes back to Pop by itself |
|---|---|---|
| 0 | **unattended**, NVMe root p3: `sshd` + `autoreboot`, `wdt nosmp fbverbose` | yes |
| 1 | NVMe root p3, `wdt nosmp fbverbose`, herd's enabled services | no; `wdt` resets it only if the kernel wedges |
| 2 | unattended, RAM root (no disk) | yes, but its logs are lost |
| 3 | the self-test kernel, RAM root, `nosmp` | no |
| 4 | headless: as 0, plus `nofb` (framebuffer never touched) | yes |
| 5 | NVMe root p3, SMP | no |
| 6 | **watchdog self-test** (`wdttest`): arm, then wedge with interrupts off | yes, through the watchdog's reset, about 60 s in |
| 7 | **wifi tool test** (`wifisim` + the `wifitest` service): the `wifi` tool against the simulated radio, transcript to `/var/log/ryzen/wifitest-N.txt` | yes |
| 8 | **wifi W1** (`rtw89`): as 0, plus the RTL8852CE brought up to running firmware and shut down again before `init`; `[rtw]` lines in `boot-N.early` | yes |
| 9 | **wifi W2** (`rtw89rx`): as 8, then Linux's recorded start replayed and 12 s of receiving on channel 1; a summary per network (OUI, channel, SSID hash, security) in `boot-N.early` | yes |
| 10 | **wifi W3/W4** (`rtw89wifi` + the `wifijoin` service): the card kept up as `/dev/wifi0`'s radio, then `wifi connect` joins the best known network in `/etc/wifi` on p3; `[rtw]` lines in `boot-N.dmesg`, the tool's exit status and `/dev/wifi0`'s non-identifying keys in `wifijoin-N.txt` | yes |
| 11 | **wifi W5** (`rtw89wifi` + `wifistay` + `sshd`): joins as entry 10 does, then **stays up 10 minutes** for ssh over wifi; `wifistay-N.txt` gets `/dev/wifi0`'s keys and the DHCP address every 30 s, `boot-N.dmesg` is saved as it goes. Reach it with `python3 overlays/ryzen/wifi-ssh.py --key <key>` (finds the station MAC `02:41:4b:55:4d:41`, ssh on port **2222**) — verified 2026-10-06, boot 20 | yes, after 10 min |
| 12 | **Akuma on wifi, for use**: quiet boot (splash, no `fbverbose`), the framebuffer console (`console.conf`), `sshd`, and `wifi auto` (`wifiauto.conf`) keeping the best known network joined; **no autoreboot** — it stays up until rebooted. Not rehearsable (`cycle.py` wants a guest that resets itself): ship the kernel, `install.sh`, `arm.sh 12` | no |
| 13 | reboot | — |

`sh arm.sh 6` boots entry 6 once. `autoreboot` is opt-in through
`initargs=daemon,--service,…`, which loads exactly the named herd services.
**`daemon` must come first**: herd takes argv[1] as a command.

## Getting back to Pop

| what ends the Akuma boot | how |
|---|---|
| `autoreboot` (entries 0, 2, 4) | `etc/ryzen/autoreboot.sh`: saves `dmesg` to `boot-N.early` at +10 s and `boot-N.dmesg` just before the reset, then `busybox reboot -f` |
| `reboot -f` (kernel `reboot.rs`) | syncs filesystems, flushes the USB disk if any, NVMe Flush + shutdown + bus-mastering off, **stops the watchdog**, then resets via `0xCF9` (falling back to the i8042 pulse, then a triple fault) |
| a wedged kernel (with `wdt`) | the AMD FCH watchdog resets the machine when the BSP has not taken a timer interrupt for 60 s |
| anything else | the power button |

## The NVMe root

`root=/dev/nvme0n1p3` makes the kernel drive the SSD itself (`amd64/src/nvme.rs`
over `crates/akuma-nvme`). It takes the controller from UEFI (9 ms on this
drive), reads the GPT (both CRCs checked), and from then on touches **only
p3's LBA range**. Every offset is checked against that window before a command
is built. If p3 holds no ext2, the mount refuses before writing anything and the
kernel falls back to the RAM image. A timed-out command disables the controller
rather than risk a late DMA.

**When a boot leaves no log** (the NVMe root did not come up), `autoreboot`'s
delay is the message. Read the time spent outside Linux from
`journalctl --list-boots` (the previous boot's end to this boot's start). That
is about 35–50 s of firmware and boot, plus:

| delay | furthest stage in `dmesg` |
|---|---|
| 140 s | `ext2 mounted on /dev/nvme0n1p3`: the logs are on p3 |
| 100 s | `nvme: p3 = LBA …`: GPT read, mount refused (p3 not ext2) |
| 60 s | `nvme: ns1 …`: Identify worked, GPT did not |
| 20 s | none of those: no controller, or the takeover failed |

## The watchdog

`wdt` (or `wdt=<seconds>`, 10–65535) arms the AMD FCH watchdog, the block Linux
drives with `sp5100_tco`, from `amd64/src/watchdog.rs`. It is gated on the
chipset (`1022:790b` revision ≥ `0x51`) and set to reset on expiry. It is armed
before the root mount, so a hung NVMe or USB bring-up is covered. It is petted
once a second from the BSP's timer interrupt and stopped right before an
orderly reset, so it can never fire during the firmware's POST or Pop's boot.
`wdttest` refuses to wedge unless the watchdog actually armed.

**Verified on the metal, 2026-10-06.**
- A normal boot with `wdt` (entry 0) logged `armed, 60 s, reset on expiry;
  CONTROL=0x11`. It ran the full 140 s `autoreboot` delay, more than twice the
  timeout, and returned exactly as without the watchdog (184 s outside Linux).
  The probe in Pop then showed it stopped.
- The self-test (entry 6) wedged with interrupts off. Pop was back 97 s later
  (~35 s firmware/boot + 60 s), with `CONTROL.FIRED = 1` and `COUNT = 0`: the
  chipset's own record that it reset the machine. `e2fsck` on p3 was clean.

From Pop, `python3 wdt-probe.py` shows the registers (`FIRED` is noted when set). Firmware leaves the
watchdog decoded-off and stopped but not locked; Linux's own driver accepted it
(`heartbeat=60 sec`, MMIO `0xFEB00000`).

## The rehearsal (`qemu.sh`)

OVMF → this `grub.cfg` → the same kernel and root, entry chosen through
`grubenv` exactly as `arm.sh` does it, serial to `$W/qemu/serial-<entry>.log`.
`-no-reboot`, so an autoreboot entry *exits* QEMU. With **`DISK=nvme`**, QEMU's
NVMe device gets a sparse 477 GiB image with **ryzen's exact GPT** (same LBAs,
same p3 window), the ESP on p6 and the root on p3. Afterwards p3 is
`e2fsck -fn`'d and `/var/log/ryzen` shown, from Linux.

| env | |
|---|---|
| `P3=blank` | p3 unformatted: the fallback path; reports whether p3's first 256 MiB are still zeros |
| `P3_SIZE=32G` | smaller ext2 for speed (default: the whole 279 GiB) |
| `KEEP=1` | boot the same disk again (persistence) |
| `KERNEL=`, `MEM=`, `ACCEL=tcg` | another kernel; RAM size; no KVM |

What it cannot rehearse: the FCH watchdog (q35 has Intel's TCO), the
framebuffer's 64-bit address (QEMU's display BARs land below 4 GiB) and the
real controller's timing.

## What the first boots found (2026-10-06)

1. **Black screen.** `MAPPED_LIMIT` was a literal 4 GiB, left over from before
   `boot.s` mapped 64 GiB. ryzen's GOP framebuffer is the Radeon's BAR at
   `0x4b0000000` (18.75 GiB), so it was refused. The kernel then halted on an
   EGA text line, and a UEFI machine has no EGA. Now
   `MAPPED_LIMIT = PHYSMAP_LIMIT`, and a missing or unusable framebuffer means
   a headless boot with the reason in `dmesg` (`nofb` forces it).
2. **The PMM handed out the kernel image.** Under UEFI the image spans two
   firmware memory regions, and `mem::usable_of` only raised the floor of the
   region containing `kernel_end`. So `0x100000 + 7 MiB` went to the PMM, and
   herd's first `fork` overwrote live kernel memory: a silent triple fault, or
   headless, a `#PF` in `talc_alloc`. It reproduced on every kernel back to
   `87f1a7c5`, under KVM and TCG. It is a property of the UEFI memory map, not
   this CPU. Reserved spans (kernel, modules, the multiboot2 info block) are
   now carved out of every region they overlap; spans strictly inside a region
   become PMM holes and the heap is placed around them.
3. **herd's `--service` needs `daemon` first.**
4. **Tooling:** an Alpine `.apk` is three concatenated gzip'd tars (GNU `tar`
   needs `--ignore-zeros`); `git.kernel.org` refuses scripted fetches from ryzen;
   a post-run check under `set -e` once leaked a mount and two loop devices on
   deleted 477 GiB images.

## Background

- [`docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`](../../docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md): the assessment, the wifi plan (§ 5), the NVMe results (§ 9)
- [`docs/runbooks/amd64-bare-metal-loop.md`](../../docs/runbooks/amd64-bare-metal-loop.md): the trashcan's loop, which this one inverts (there Akuma is the default)
- [`scripts/benchmarks/ryzen_fc/`](../../scripts/benchmarks/ryzen_fc/README.md): the Firecracker rig on the same machine
