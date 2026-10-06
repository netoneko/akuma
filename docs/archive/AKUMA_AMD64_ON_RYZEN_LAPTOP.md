# Akuma/amd64 on ryzen (bare metal) — what it would take, wifi first

2026-10-06. Began as an assessment made with read-only probes (`lspci`,
`lsusb`, `lsblk`, `parted print`, `efibootmgr -v`, `journalctl -k`, sysfs, and
`ntfsresize --info --no-action`, which refused and modified nothing). It was
then carried out the same day: §8 and §9 record what was built and measured.
The verdict below is the original assessment, with resolved items marked.

Written in answer to: "install akuma on it on a separate partition, get wifi
going or better yet usb networking and then work on wifi via that", then
narrowed to **"can we do wifi with a reboot cycle — temporarily boot Akuma, run
tests, dump dmesg to disk, reboot back to Linux and keep working there?"**

## Status, 2026-10-06 (end of day)

**Akuma runs on ryzen's metal with its root filesystem on the laptop's own
NVMe SSD.** It boots one-shot from Pop, reaches userspace, writes its kernel log
to `/var/log/ryzen` on that disk, resets itself, and Pop reads the log back.
`e2fsck` is clean, even after a hard power-off mid-run. **The whole cycle has
run unattended:** boot 2 took 184 s outside Linux (140 s `autoreboot` delay +
44 s firmware/boot) and left `boot-2.early` + `boot-2.dmesg` on p3.

| | state |
|---|---|
| boot path (systemd-boot one-shot → standalone GRUB → multiboot2) | **done**, `overlays/ryzen/` |
| framebuffer above 4 GiB (`0x4b0000000`) | **fixed**, verified on the panel |
| PMM handing out the kernel image under UEFI | **fixed**, found by the QEMU rehearsal |
| NVMe driver + `root=/dev/nvme0n1p3` | **done**, verified on the metal (§9) |
| log sink | **done**: p3, ext2 (64 GiB filesystem inside the 279 GiB partition) |
| Windows | **gone**: p3 reformatted at the user's request |
| FCH watchdog (`wdt`) | **done**: a deliberate wedge was reset by the chipset in 60 s (`FIRED=1`), `overlays/ryzen/README.md` § The watchdog |
| wifi W0 (§5.1) | **done**: mmiotrace of probe and of interface-up through `fw ready`, `overlays/ryzen/w0-trace.sh` |
| wifi W1 (§5.2) | **done**: `[rtw] fw ready v0.27.122`, 166 packets in 50 ms, card shut down again (`crates/akuma-rtw89`, `amd64/src/rtw89.rs`, menu entry 8) |
| wifi W2 (§5.3) | **done**: the recorded start replayed, 17 networks' beacons on channel 1 (menu entry 9) |
| wifi W3/W4 (§5.4) | **done**: `rtw89wifi` (menu entry 10) scans, authenticates, associates and completes the WPA2 4-way handshake in the kernel; keys installed, link held (boot 16) |
| p3 self-host environment (§5.7) | **staged**: toolchain, clone, rig, goose+Kimi, rio; not yet booted |
| wifi RX replay protection (§5.7) | **built**, host-tested; not yet on metal |
| wifi W5 (§5.6) | **done**: DHCP, SNTP, DNS, ssh in on 2222, outbound HTTP to the LAN and the internet; menu entry 12 boots it for use (quiet boot, framebuffer console, stays up) |

## Verdict

1. **Yes, the reboot loop is the right way to develop wifi, and it does not need
   a new partition or USB networking.** systemd-boot's one-shot entry boots
   Akuma once and comes back to Pop on the next reset with no one at the
   keyboard (§3). The kernel and a RAM root image can live on the second ESP,
   which has 3.7 GiB free.
2. **The box runs systemd-boot, not GRUB.** There is no GRUB line to add next to
   Windows. Pop!_OS 22.04 boots `\EFI\systemd\systemd-bootx64.efi`;
   `grub-efi-amd64-bin` is not installed and `grub-mkstandalone` is absent. The
   equivalent is one standalone `grubx64.efi` (for `multiboot2`) plus one
   systemd-boot entry file that points at it (§3).
3. *(Log sink resolved 2026-10-06 by the NVMe driver, §9; the watchdog is
   still open.)* **The real costs are the log sink and a watchdog, not the boot.** Akuma can
   read the ESP only through GRUB. Once it is running it has no NVMe driver and
   cannot write vfat, so `dmesg` has to land somewhere else: a USB stick through
   the existing xHCI mass-storage driver (cheapest), or a new NVMe driver
   (§4). An unattended loop also needs a hang to end in a reset, which means
   arming the FCH hardware watchdog (§3).
4. **Native wifi on this card is a large project: 11–24 sessions with wide
   error bars** (§5). The card is a Realtek RTL8852CE (`10ec:c852`, `rtw89`,
   WiFi 6E). Akuma has no 802.11 stack and no kernel crypto. **Decision
   (user, 2026-10-06): all of it stays in the kernel, with no userspace
   supplicant.** That reverses the "kernel has no cryptography" line in
   `CLAUDE.md` for the narrow WPA2 set (§5 W4). On
   this exact box, Linux's own driver crashes the card's firmware routinely
   (18,000 `[ERR]fw PC` lines and `SER catches error` this boot).
5. **USB ethernet is cheap (3–6 sessions)** but optional for the wifi work (§6).
   It buys an ssh session into Akuma on this box, not a faster loop: every
   kernel change is still a reboot.
6. **The persistent partition is Windows' partition, reformatted as ext2**
   (user's decision, 2026-10-06). `nvme0n1p3`, 279 GiB, so no resizing is
   needed. ntfsresize had also found that NTFS inconsistent, which no longer
   matters. GRUB can read the kernel and root image from it on day one. Akuma
   can write it once it has an NVMe driver (§4b, §7).

## 1. The machine (measured)

| | |
|---|---|
| Model | **Lenovo IdeaPad 5 2-in-1 16AHP9** (`83DS`), BIOS `P1CN30WW` 2025-07-10. A **laptop**, Pop!_OS 22.04, kernel 6.17.9 |
| CPU / RAM | Ryzen 7 8845HS, 8C/16T; 13 GiB visible |
| Firmware | UEFI, **Secure Boot off** (`SecureBoot` efivar = 0) |
| Boot | **systemd-boot** on `nvme0n1p6` (2nd ESP, 3.9 GiB, 3.7 GiB free); `loader.conf` = `default Pop_OS-current`, no timeout, so the menu is hidden. Windows Boot Manager is on `nvme0n1p1` |
| Disk | SK hynix 512 GB **NVMe** `1c5c:1d59`, GPT, no free space: ESP 260M · MSR · **Windows 279 GiB NTFS** · Pop ext4 191 GiB (76% used) · ESP 3.9 GiB · WinRE 2 GiB. No BitLocker (`blkid` reads plain NTFS) |
| Wifi | **Realtek RTL8852CE** `10ec:c852` sub `17aa:5852`, BAR 1 MiB @ `0x80b00000`, firmware `rtw89/rtw8852c_fw-1.bin` v0.27.122.0, `rfe_type 1`. Bluetooth is the same chip's USB function (`0bda:5852`) |
| Ethernet | **none**. Wifi is the box's only network today, which is why a Linux-side capture that unloads `rtw89` drops ssh |
| USB | 4 xHCI controllers, all AMD: `04:00.3` (`15b9`, bus 1/2: 5×USB2 + 2×SS), `04:00.4` (`15ba`, bus 3/4: internal camera), `06:00.3`/`06:00.4` (`15c0`/`15c1`, buses 5–8, USB4-side). 64-bit BARs, all below 4 GiB. Two Type-C ports (`/sys/class/typec/port{0,1}`) |
| Keyboard | **real i8042** (`AT Translated Set 2 keyboard` on `isa0060/serio0`). The internal keyboard is PS/2 behind the EC, not USB, so `amd64/src/kbd.rs` should work natively. The trashcan never had that |
| Touch | I2C HID (Goodix touchpad, Wacom pen). Not needed |
| Display | eDP 1920×1200. The GRUB GOP framebuffer is all `akuma-fbcon` needs; the Radeon 780M is never touched |
| Serial | **none** (8250 probed `uart:unknown` at all four legacy ports). Same situation as the trashcan |
| IOMMU | AMD-Vi present (IVRS), Linux uses translated default domain. Whether firmware leaves it on at handoff is **unmeasured**; `amd64/src/xhci.rs` assumes "no IOMMU on this target" |

Also live on this box, and **down for every Akuma cycle**: the Firecracker
guest (`akuma-vm.json`, tap0, kot), `llama-server`, docker, and the user's own
sessions.

## 2. What already works on this machine's hardware class

From the trashcan port (`AKUMA_AMD64_ON_HP_500_502NJ.md`,
`docs/runbooks/amd64-bare-metal-loop.md`): multiboot2 boot from GRUB, GOP
framebuffer console, ACPI/MADT, LAPIC timer, SMP, a ramdisk root from a
multiboot2 module (`amd64/src/ramdisk.rs`), polled xHCI with USB mass storage
and a HID keyboard, `0xCF9`/i8042/triple-fault reboot (`amd64/src/reboot.rs`).
On an AMD FCH, `0xCF9` is the standard reset port, so reboot should work as-is.

Unknowns that only a first boot will settle:

- **SMP at 16 threads.** The trashcan has 4. Boot `nosmp` first.
- **xHCI controller choice.** `xhci.rs:633` takes the **first** class
  `0c03`/prog-if `30` device, which here is `04:00.3`. That controller owns 2 SS
  ports, so a USB stick has to be in one of those ports. Which physical socket
  that is has to be measured: plug the stick in under Linux and read `lsusb -t`.
- **IOMMU at handoff**, as above.
- The first one or two boots need **someone looking at the screen**. There is no
  serial port and no network, so the framebuffer is the only output until the
  log sink works.

## 3. The reboot loop

### Boot path: systemd-boot → standalone GRUB → Akuma

```
/boot/efi/EFI/akuma/grubx64.efi     # grub-mkstandalone -O x86_64-efi, modules: multiboot2 part_gpt fat
/boot/efi/EFI/akuma/akuma-amd64     # the kernel (build with --features no-tests? see note)
/boot/efi/EFI/akuma/root.img        # ext2 RAM root, the test init on it
/boot/efi/loader/entries/akuma.conf
    title Akuma/amd64
    efi   /EFI/akuma/grubx64.efi
```

The embedded `grub.cfg` is
`multiboot2 /EFI/akuma/akuma-amd64 init=/bin/<test> skiptests …` followed by
`module2 /EFI/akuma/root.img`. Cost: `apt install grub-efi-amd64-bin` (binaries
only; it does **not** touch Pop's boot chain) plus three files on `p6`. Removing
it means deleting `EFI/akuma/` and `akuma.conf`. Linux's ESP and Windows are
not touched.

### One cycle

```
Linux:  build kernel + root.img → copy to /boot/efi/EFI/akuma/
        bootctl set-oneshot akuma.conf && systemctl reboot
Akuma:  test init runs the wifi probe, writes dmesg to the log sink, reboot -f
Linux:  (one-shot consumed → default Pop entry) read the log, iterate
```

This is **the opposite of the trashcan's arrangement** (Akuma as GRUB default,
with no remote way back). Here the default stays Pop, and Akuma only runs when
it is armed. A reset from any cause lands back in Linux. Budget about 1–2 min
per cycle (firmware POST plus two OS boots). `bootctl set-oneshot` is supported
by Pop's systemd 249 but **untested on this box**: run it once with the
Windows entry before relying on it.

### What makes it unattended

- **The Akuma side must always end in a reset.** The test init must finish with
  `reboot -f` and also enforce its own deadline, so that a stuck userspace probe
  still reboots. That is a few lines in the probe binary.
- **A kernel wedge must also end in a reset.** Otherwise someone has to hold the
  power button. Arm the **AMD FCH watchdog** (the device Linux binds
  `sp5100_tco` to; `sp5100_tco` is in this box's module list) early in boot,
  and pet it from the timer tick. Akuma has no watchdog today (no hits in
  `amd64/src`). Cost: **~1 session**, mostly the FCH register sequence and
  checking it fires. Until it exists, every cycle needs someone near the
  machine.

## 4. The log sink: where `dmesg` goes

**Resolved 2026-10-06: option (b)**, the NVMe driver and ex-Windows p3 (§9).
The USB stick was never needed.

Akuma cannot write the ESP: it has no vfat (and, until §9, had no NVMe driver).

| option | new code | risk | verdict |
|---|---|---|---|
| **a. USB stick, ext2** | none if the existing xHCI + BOT driver comes up on an AMD controller; the test init writes `/mnt/dmesg.N` | stick must be in an SS port on `04:00.3`; xHCI bring-up has wedged a box before (`AKUMA_AMD64_XHCI_WEDGED_BOX`, faulted DMA across warm resets) | **do this first** |
| b. **NVMe driver + the ext2 partition** (ex-Windows `p3`), mounted through `akuma-ext2` like the USB root. dmesg becomes a normal file Linux reads with `mount` | `akuma-nvme`: polled admin + one I/O queue, PRP, read/write. ~2–3 sessions on the `akuma-xhci` pure-crate pattern | it writes to the disk that holds Pop. The driver must hard-refuse any LBA outside `p3`'s GPT bounds, enforced in the driver and not left to the caller | **the target**: no stick, persistent root, self-install. Use (a) only if NVMe slips |
| c. RAM that survives a warm reset | small | firmware may scrub it; unreliable | no |

## 5. Native wifi on the RTL8852CE: the work, staged for the loop

Every stage produces a dmesg line that says whether it worked, which is exactly
what the reboot loop delivers. Session counts are rough. This card is among the
hardest a hobby kernel could pick, and "golden reference from Linux" is the
only thing that makes it tractable.

| # | stage | done when | sessions |
|---|---|---|---|
| W0 | **Golden reference from Linux.** `mmiotrace` of an `rtw89_8852ce` unbind/bind, PCI config, efuse via debugfs, firmware file. This is the method `akuma-net-rtl8169` used. Unbinding drops the box's only network, so it has to be a **local script that rebinds itself**, not an ssh session | a register-level trace of power-on → firmware download → `fw ready` | 1 |
| W1 | **Power-on + firmware download** in Akuma: PCIe/MAC power sequence, the DMA queue used to push `rtw8852c_fw-1.bin`, wait for the firmware-ready bit | `[rtw] fw ready v0.27.122` | 2–4 |
| W2 | **Receive:** MAC/BB/RF init (large register tables, liftable as data from Linux `rtw8852c_table.c`; check the SPDX line, rtw89 is believed to be dual GPL/BSD), channel set, RX DMA ring, at least the RF calibration that RX needs | SSIDs and BSSIDs from beacons printed to dmesg | 3–6 |
| W3 | **Transmit + 802.11 MLME:** TX ring, probe/auth/assoc frames, a station state machine. Test against an **open** AP (a phone hotspot) | associated, DHCP lease over the open network | 2–4 |
| W4 | **WPA2-PSK, in the kernel** (no userspace supplicant, per the decision above): EAPOL 4-way handshake, HMAC-SHA1 PRF + MIC, AES key unwrap (RFC 3394) for the GTK, key install into the card's CAM so the **hardware** does CCMP. So the kernel needs only SHA-1 and AES-128 block decrypt, in a host-tested `forbid(unsafe_code)` crate with test vectors from the 802.11i annex. The PSK (PBKDF2-SHA1, 4096 iterations) can be computed once on Linux and passed as a 64-hex `psk=` on the cmdline, which keeps PBKDF2 out of the kernel entirely; or compute it at boot, at a cost of about 8k SHA-1 compressions | joins the home network | 2–4 |
| W5 | **Integration + robustness:** `ExternalDevice::Rtw89` beside `Virtio`/`Rtl8169` in `akuma-net-nic`, netpoll, power save off, firmware-error (SER) recovery. Linux needs SER recovery on this very box | sshd reachable over wifi, survives an hour | 1–3 |

**Credentials** (for W3/W4): `~/.akuma/wifi/<network>` on the laptop holds that
network's WPA2 passphrase. It is never copied into
the repo, a doc, a log or a commit. How it reaches Akuma is a W4 decision. The
natural shape is a PSK derived from it (PBKDF2) staged onto p3 from Pop, which
keeps PBKDF2 out of the kernel. That puts a secret on ryzen's disk, so the
decision is the user's when it comes.

**Total: 11–24 sessions and many dozens of reboot cycles.** W0–W2 can run
entirely on the reboot loop. W3 onward needs an AP to test against and
eventually a way to talk to Akuma, which is where §6 helps.

A cheaper wifi path, if the goal is "Akuma on wifi" and not "this card": a USB
wifi dongle with a small, well-understood chip (MT7601U being the classic
choice). It reuses the xHCI work in §6 and skips PCIe DMA and the 802.11ax
tables. W3–W4 are needed either way.

### 5.1 W0 results, 2026-10-06

`overlays/ryzen/w0-trace.sh`, run twice from Pop (kernel 6.17.9, firmware
`rtw8852c_fw-1.bin` 0.27.122.0; Pop has no `-2.bin`, which the driver tries
first). Each run: NetworkManager kept off the card by MAC, mmiotrace on (it
takes every CPU but one offline), unbind, bind, `ip link set up`, down, trace
off, network back. Wifi was gone about 25 s per run; association resumed 4 s
after the trace stopped, so no keys are in it. The traces are on the laptop
in `~/.akuma/w0/<stamp>/`, outside the repo because the efuse holds the MAC,
and on ryzen under `/var/tmp/akuma-w0/`. Summarize with
`overlays/ryzen/w0-summary.py <trace> [--dump PHASE]`.

| phase | reads | writes | what it is |
|---|---|---|---|
| bind (probe) | 58,539 | 2,316 | power-on, efuse (50k polls of `0x30`), power-off. Identical across both runs to within polling counts |
| up | 18,913 | 8,886 | power-on again, MAC init, firmware download, `fw ready` |
| down | 1 | 1 | — |

Two things the traces settle for W1:

- **Firmware is downloaded at interface-up, not at probe.** Probe reads the
  file (`request_firmware`) but only parses it. Run 1 missed the download
  entirely: the rebound card appears as `wlan0` and udev renames it to
  `wlp2s0` a moment later, so `ip link set wlan0 up` found nothing. The script
  now waits for `udevadm settle`.
- **The `fw ready` handshake is visible in `R_AX_WCPU_FW_CTRL` (`0x1e0`)**,
  status field bits 7:5: `0x01` (driver sets FWDL_EN) → `0x23` (H2C path
  ready) → `0x27` (download path ready) → `0xc3` (status 6, image accepted;
  polled ~5.6k times while the firmware boots) → `0xe2` (status 7,
  **init ready**). That last value is W1's "done when".

What the trace cannot show: the firmware bytes and every H2C command travel by
DMA. The busy registers in "up" (`0x1174c` polled 7.2k times, `0x10370`
read-modify-written 3.4k times, `0x1e0c0`/`0x1f0c0`) are the DMA channel's
bookkeeping; decoding them against `rtw89/pci.c` is the first job of W1.

### 5.2 W1 results, 2026-10-06

`crates/akuma-rtw89` holds the sequence — every function the 8852C arm of its
Linux namesake, `forbid(unsafe_code)` — and `amd64/src/rtw89.rs` the PCI,
BAR, DMA memory and firmware file. Before any metal boot the bring-up already
**replayed access-for-access against the W0 trace** (`tests/golden_trace.rs`:
651 register accesses, power-on through `0xe2`, ring base addresses the only
values not compared), and a simulated chip checked the DMA payload. Seven
boots of menu entry 8, each rehearsed in QEMU first (no card there: the
"absent" path), each back in Pop by itself in 175–216 s:

| run | changed | result |
|---|---|---|
| 1 | — | power-on, DMA setup, `H2C_PATH_RDY` (`0x23`) all as Linux; the chip fetched the header packet (CH12 index 1/1) but `FWDL_PATH_RDY` never came |
| 2 | `DevCtl.NoSnoop` cleared; arrival state logged | same. No Snoop was already off; the card arrived exactly as Linux's bind found it |
| 3 | IOMMU, `DevSta`, ring readback logged | same. AMD-Vi off (`IommuEn` 0), no PCIe errors, the ring entry is at the address the card was given. **`DMAC_ERR_ISR` = `HAXIDMA_ERR_FLAG`** |
| 4 | RX rings stocked with buffers, as Linux's are; `HAXI_IDCT` logged | same; `HAXI_IDCT` = **`TXMDA_STUCK`** — the TX DMA engine wedged on the packet |
| 5 | ASPM L1 and CLKREQ# off for the bring-up (`rtw89_pci_link_cfg`'s warning) | same |
| 6 | **Bus Master Enable on the card's root port**, as Linux's `pci_enable_bridge` does | **`fw ready v0.27.122`**: header accepted `0x27`, 166 packets, `0xe2`, 49.6 ms |
| 7 | the experiments of runs 2–5 removed, `pci::enable_bridges_above` | confirms run 6 |

**Root cause.** The root port above the card (bus 2) arrived from firmware
with command `0x0003`: memory and I/O decode on, Bus Master **off**. A bridge
forwards a device's DMA upstream only with its own Bus Master Enable set, so
the card fetched nothing. It still advanced its ring index and reported
"TX DMA stuck" rather than any PCIe error, which is why it took four runs to
see past the content of the packet. Linux sets bus mastering on every bridge
above a device it enables; Akuma's `pci::enable_full` never touched bridges,
and the NVMe and xHCI drivers never needed it because firmware boots through
their ports. `pci::enable_bridges_above` is the fix, and any new bus-master
driver on this target should call it.

What W1 deliberately does not do: keep the firmware running. The card is
shut down (CPU stopped, DMA stopped, MAC powered off, bus mastering off) before
`init`, so a warm reset never finds a live bus master. W2 starts from here:
the rest of `rtw89_mac_init` (`sys_init`, `trx_init` with the full quota mode),
then RX.

### 5.3 W2 results, 2026-10-06: receiving, from a recording

**Done on the first metal boot** (menu entry 9, token `rtw89rx`): in 12 s on
channel 1 the card delivered 1604 802.11 frames (zero CRC errors), 1503 PPDU
status reports and 56 firmware events, and 17 distinct networks' beacons,
the home network's among them (matched by SSID hash; the log carries no
network names and only the OUI of each BSSID).

How, in three steps:

1. **A second trace, with the firmware commands.** `w0-trace.sh` now also
   dumps every H2C the driver sends and every C2H it receives (two fprobe
   events on `rtw89_h2c_tx` / `rtw89_fw_c2h_irqsafe`, 2 KiB / 512 B each, into
   a trace instance of their own — the mmiotrace tracer's pipe silently drops
   foreign events). `w2-merge.py` puts them in order: the instance's clock
   runs ~0.75 s off mmiotrace's, so each H2C is anchored to its own CH12
   doorbell instead (585 H2Cs, 585 doorbells). The run also showed that
   **after `ip link up` the card enters idle power save**: the "up" phase
   ends powered off, and the scan phase begins with the whole start again.
2. **The start is a property of the chip, not of the run.** After `fw ready`
   Linux makes ~17 000 register accesses and 45 H2Cs in 89 ms: the rest of
   `mac_init`, the BB/RF tables (which come from the firmware file's
   elements, not from `rtw8852c_table.c`), BB post-init, coexistence, DM
   init, the start-time RF calibrations (RCK, DACK, RX DCK) and channel 1.
   The two starts in one trace (up, and the scan's) differ in **564** of
   them, all DACK readouts and ~40 writes computed from them. So
   `w2-seqgen.py` compiles the recording into an op stream (writes, checked
   reads, polls on the bits that changed, delays from the timestamps, H2Cs
   with the MAC zeroed) and `akuma_rtw89::script` replays it: 16 815 ops,
   170 KiB, `crates/akuma-rtw89/seq/up.seq`.
3. **Receive.** Every RXQ entry gets its own 12 KiB buffer (W1 shared eight),
   `akuma_rtw89::rx` parses the RX descriptor, the RX filter is opened as
   mac80211 opens it for a scan, and `akuma-ieee80211` takes the beacons apart.

On the metal the replay took 52.6 ms. Of 4976 checked reads, 91 differed:
the coexistence scoreboard (`0xac`: Linux's run had Bluetooth up, Akuma has
no Bluetooth driver) and the DACK readouts, as the two-start comparison
predicted. No poll timed out.

Open: the home network's beacons arrived 17 times in 12 s against ~117 sent
(other networks: up to 43). Linux runs more calibration (RX DCK, IQK, TSSI,
DPK — `rtw8852c_rfk_channel`) when it associates, which the "up" recording
does not contain. That is W3's first recording.

### 5.4 W3/W4 results, 2026-10-06 evening: the station joins

**Akuma joined the home network with WPA2-PSK, the handshake in the kernel
(boot 16).** Authentication, association (AID 3), message 1 → 2, message
3 → 4, both keys installed (`JOIN4`, group key id 2), and the link still
connected 20 s later. The way there is below; the cause that took four boots
to find was the address CAM's **address hashes**. Token
`rtw89wifi` (ryzen menu entry 10, herd service `wifijoin`) keeps the card up
after the firmware download and hands it to `amd64/src/rtw89_sta.rs`, the
`/dev/wifi0` backend: a daemon that replays the recorded join
(`script::JOIN1..4`) around the frames it sends itself — scan, auth, assoc,
`JOIN3`, the 4-way handshake (`akuma_wpa::eapol::Supplicant`), msg 4, `JOIN4`.

Design points, each a decision rather than an accident:

- **Boot replays `JOIN1`, not `up.seq`.** `JOIN1` starts at the last
  `fw ready` of the join recording and so *is* Linux's whole start plus the
  interface setup; replaying it after `up.seq` would run the start twice. A
  failed join leaves a peer in the card, so the next join power-cycles it
  (`Card::restart`: `shutdown` + `bring_up`) and replays `JOIN1` afresh.
- **Every frame of a join uses mac id 0.** A station's peer entry shares its
  interface's mac id (`rtw89_core_sta_add`); all three recorded `txd` records
  say 0. `tx.rs` had claimed 1 for frames to the AP.
- **The daemon parks between polls** (`sched::block_until_deadline`) so a
  `nosmp` box keeps running sshd through a join; the replays are the only busy
  stretches (~37 ms each).
- **The reboot path powers the card off** (`rtw89::shutdown_for_reset`, from
  `reboot.rs`) through its own register view, since the daemon may be parked
  mid-join on the same core.

Fixed on the way, each by comparing `tx.rs` with Linux's
`rtw89_core_fill_txdesc_v1` / `rtw89_pci_txwd_submit` and the recorded `txd`
bytes (host tests now pin all of them to the recording):

| bug | effect |
|---|---|
| the address-info entry carried the **WD page's** bus address, not the frame's | the chip would have transmitted the descriptor as the frame |
| `USE_RATE` set on every frame | EAPOL/data at a forced CCK rate; Linux sets it for management only |
| `TID_INDICATE` never set | EAPOL (tid 7) recorded with it set |
| `wp_offset` 0 on protected data | Linux uses 1 (room for the security header the chip writes) |

Metal runs (all reached Pop again by themselves, ~93 s outside Linux):

| boot | what it showed |
|---|---|
| 12 | `JOIN1` 18 337 ops in 36.7 ms, 0 poll timeouts (85 checked reads differ: the `0xac` coex scoreboard and calibration readouts, as in W2). Scan: 110 beacons, 22 networks in 2.5 s, **the home network among them** (`ssid#2ce1df73`, ch 1, wpa2-psk). `JOIN2` 4072 ops, 0 timeouts. Auth ×3: no answer → `error=timeout` |
| 13 | with TX diagnostics: CH8's read index moves 0→1→2→3 with each auth frame, **one release report per frame, status `TX_DONE`** (qsel 0x12, WD seq 0/1/2), no DMA error. A unicast management frame reports done only once acknowledged, so the AP very likely ACKed it. But in the 1.2 s of waiting **the host received nothing at all**, not even the AP's beacons |

| 14 | RX counters and a 300 ms probe before/after `JOIN2`: RX is alive after `JOIN2` (the PHY keeps reporting PPDUs, 802.11 frames pass with the filter open), but with the station filter **no frame addressed to us** is ever delivered |
| 15 | auth waits with the RX filter opened as for a scan: **the AP's auth reply arrives and is accepted**; the association reply (filter normal again) does not. The card's own address match was dropping everything sent to the station |
| 16 | address-CAM hashes recomputed (below), the filter left open for the whole join with the station matching addresses itself, whole-join retries. Auth try 1 unanswered (a busy channel: 111 frames in 600 ms), try 2 accepted with **`A1_MATCH` set** — the card's match works now; assoc accepted; handshake complete; **connected** |

**The root cause: the address hashes.** An address-CAM entry
(`rtw89_cam_fill_addr_cam_info`) carries, beside the station address (SMA)
and the peer's (TMA), a one-byte hash of each — the XOR of the address's
bytes (`rtw89_cam_addr_hash`), bytes 18 and 19 of the H2C. The hardware
matches received frames through them. The join segments substitute this
station's address and the AP's BSSID into the recorded commands but kept the
recording's hashes, of the card's real MAC and the recording's AP: an entry no
frame could hit. `script::fix_addr_cam` now recomputes both hashes for every
address-CAM command (class 6, func 0) right before it is sent, honouring the
entry's address mask; a host test checks every such command in all four
segments. The same trap waits for any recorded H2C that carries a value
*derived* from a substituted one: a substitution is only complete once every
derived field follows it.

**Resilience**, since this channel loses frames routinely: auth and assoc
are each sent up to 6 times with a 600 ms wait; the handshake gets 10 s;
a join that times out is retried twice more from a power-cycled card
(`JOIN_ATTEMPTS`); a deauthentication mid-join ends the attempt; a
retransmitted message 3 after the join gets message 4 again; and the RX
filter stays open during the join so a card-side match failure cannot cost a
join again (the reply's `A1_MATCH` is logged, so such a failure stays
visible). The `wifi` tool waits 90 s for a join to settle (was 20 s).

**One ordering fix after boot 16:** the AP resent message 3 twice — our
message 4 was still queued when `JOIN4` installed the keys, so the chip sent
it under the new pairwise key or not at all. The station now waits for
message 4's release report (up to 200 ms) before installing the keys.
Boot 17 confirmed it: joined again, no message 3 resent. Its auth needed
three tries (203 frames heard in one 600 ms wait), which is what the retries
are for.

Staged on ryzen for these runs: `/etc/wifi/home` on p3 (PSK derived on the
Mac from `~/.akuma/wifi/<network>`, piped in, mode 0600, never printed) and
`wifijoin` + `/bin/wifi` copied onto p3, which was formatted from an image
older than the tool. `wifijoin` records only the tool's exit status and
`/dev/wifi0`'s non-identifying keys (`wifijoin-N.txt`); the `[rtw]` lines
hash SSIDs and cut BSSIDs to the OUI. Driven with
`cycle.py 10 --log dmesg --transcript wifijoin`.

### 5.5 The join, as built: recording, replay, pieces, rules

Folded in from the W3/W4 handoff (`NEXT_AGENT_RYZEN_WIFI_JOIN.md`, removed
2026-10-06 once its plan was carried out); § 5.4 is what the metal said.

**The recording.** `JOIN=1 sh overlays/ryzen/w0-trace.sh` traces Linux joining
the home network: `~/.akuma/w0/20261006-102808` (the best: it has the `txd`,
`txh`, `txm` and `rxm` probes) and `-102211`, on the Mac. Merge and view with
`w2-merge.py <run> --phase join --collapse --no-fwdl --ts` and
`w3-timeline.py`. What it showed:

- Linux goes into idle power save between scans; the final join starts with a
  full power-on and firmware download, then the start, then the join setup.
- **Only H2C commands carry the BSSID, the station's MAC or the AID**; no
  register write does. The station address lives only in the address CAM, so
  the driver picks its own: `02:41:4b:55:4d:41`. Values *derived* from those
  addresses ride along in the same commands — the address CAM's SMA/TMA hashes
  (§ 5.4) — and must follow any substitution.
- `up.seq` writes no ring bases and un-stops every TX channel at the end, so
  TX needs ring memory of its own for the channels used (`bringup::Dma::tx_phys`,
  ACH0/ACH3/CH8).

**The four segments** (`w2-seqgen.py --join N --mac … --bssid …` →
`crates/akuma-rtw89/seq/join{1..4}.seq`; boundaries found by content):

| N | from | to (exclusive) | replayed | size |
|---|---|---|---|---|
| 1 | after the last `R8 0x1e0 = 0xe2` (fw ready) | the first address-CAM H2C carrying the BSSID | at boot, right after `bring_up` | 183 423 B, 29 H2C |
| 2 | that address-CAM H2C | the authentication frame's TX | before auth | 39 439 B, 9 H2C |
| 3 | after the association response | the first periodic `OFLD_RSSI` H2C | after the association response | 716 B, 15 H2C |
| 4 | the first security-CAM H2C | through the `BCNFLTR` H2C | after EAPOL message 4 is on the air | 537 B, 9 H2C |

The card's MAC and the BSSID are blanked wherever they occur and recorded as
`0x41` substitutions (generation fails if a byte of either survives); the AID
is patched into the address CAM's `AID12` (byte 44) and the PS-Poll template's
duration (byte 14); the group key's id into bits 7:6 of the address CAM's
byte 46 and the DCTL's byte 30 (the recording's AP used id 2). The two
security-CAM bodies, redacted to zeros in the recording, are synthesized from
`cam.c` (`[idx, 0, 20, 0, 6, 0, 0, 0, key[16]]`: pairwise entry 0, group entry
1, CCMP-128). Dropped from every segment: C2Hs, IRQ registers, the RX and TX
ring index registers (`0x1058..=0x107c`), the periodic `OFLD_RSSI` exchange and
the later BA_CAM/ADDBA exchange.

**The pieces.**

| piece | does |
|---|---|
| `crates/akuma-wpa` | SHA-1, HMAC, the 802.11 PRF, PBKDF2, AES-128, RFC 3394 key wrap; `eapol::Supplicant` (message 1 → 2, message 3 → 4 + keys, group rekey). Host-tested against published vectors and an AP built from the same primitives |
| `crates/akuma-ieee80211::sta` | auth/assoc request builders (the assoc elements are Linux's own from this card, RSN capabilities 0), auth/assoc/deauth parsers, QoS data frames with LLC/SNAP and room for the CCMP header, the RX data parser |
| `crates/akuma-rtw89::tx` | the WD page and TX rings, the three frame classes (§ 5.4) |
| `crates/akuma-rtw89::script` | `Vars`, op `0x41`, `JOIN1..4`, `fix_addr_cam` |
| `amd64/src/rtw89.rs` | `Card`: the kept card (`restart`, `replay`, `send`, `poll_rx`, filter open/close), `shutdown_for_reset` |
| `amd64/src/rtw89_sta.rs` | the station daemon, `/dev/wifi0`'s backend: scan, join with retries, rejoin on loss (below) |
| `overlays/ryzen` | menu entry 10 (`rtw89wifi` + `wifijoin`), `cycle.py --log dmesg --transcript wifijoin` |

**Staying joined.** The station keeps the network it was told to join until
`disconnect` (or a wrong key / a refusal, which retrying cannot fix). A
deauthentication or disassociation from the AP, or the firmware's beacon-loss
report (`BCNFLTR_RPT` C2H, type 0), drops the association and rejoins at once
from a restarted card; a join that times out is retried after 1 s, every time.
`wifibackoff` on the command line makes that wait double after each failure,
to 30 s — off by default, since on this channel the link drops often and
waiting longer only means being offline longer. The same report's averaged
RSSI feeds `/dev/wifi0`'s `signal` while joined.

**Rules that hold for all of this work.**

- **Never print, log, commit or write into docs** the home network's name,
  its passphrase, the router's BSSID or the card's real MAC; it is "the home
  network". Logs carry SSIDs as FNV-1a hashes (home = `0x2ce1df73`, channel 1,
  20 MHz, WPA2-PSK CCMP, PMF capable-not-required) and BSSIDs as their OUI.
- The passphrase lives in `~/.akuma/wifi/<network>` on the Mac, read only to
  derive the PSK (`PBKDF2-HMAC-SHA1(passphrase, ssid, 4096, 32)`), which is
  staged on p3 as `/etc/wifi/<name>` (mode 0600) and never echoed. Never
  `cat`/`od` a file you did not create. The kernel never reads that file: the
  `wifi` tool does and writes `connect wlan0 <ssid-hex> <psk-hex>` to
  `/dev/wifi0`.
- Talk to ryzen only through `scripts/utils/hpbox.py`; one metal boot is
  `python3 overlays/ryzen/cycle.py <entry>` after
  `cargo build -p akuma-amd64 --target x86_64-unknown-none --release --features no-tests`.
- The Linux source the driver follows is `~/.akuma/src/rtw89` (v6.17, dual
  GPL-2.0/BSD-3, used under BSD-3; the notice stays in `akuma-rtw89/src/lib.rs`).

**Still to do, in order.**

1. ~~W5, the data path~~ — done, § 5.6 (outbound TCP still refused).
2. Scanning beyond channel 1 (a recorded channel switch), a deauthentication
   on `disconnect`, a group-key-only segment for rekeys (today all of `JOIN4`
   is replayed).

### 5.6 W5 results, 2026-10-06 night: ssh into Akuma over its own wifi

**Akuma on ryzen is reachable over wifi** (boot 20, menu entry 11): it joined
the home network, DHCP gave it an address, the wall clock synced over SNTP,
DNS resolves, and **`ssh -p 2222 root@<address>` works** — from the Mac,
through the RTL8852CE, with the WPA2 keys the kernel negotiated. Find the
address with `overlays/ryzen/wifi-ssh.py --key <key>` (ping-sweeps the /24 and
looks the station MAC `02:41:4b:55:4d:41` up in the Mac's ARP table). Akuma's
sshd is on **2222**; port 22 answers with a reset. The user's key is in p3's
`/etc/sshd/authorized_keys` beside the image's `amd64-ssh-test-key`.

**The data path.**

- `akuma-net-nic::queued`: a NIC that is two fixed frame queues
  (`FrameQueues`, 16 × 1536 B per direction, `try_lock` only — never spins,
  since the stack and the driver may share one core). `ExternalDevice::Queued`
  is the stack's side; the station is the driver's. `net::init_bare_metal`
  builds the stack on it when there is no Ethernet NIC and `rtw89wifi` kept
  the card (DHCP on, no static pre-DHCP address).
- Receive: data frames from the AP that the card decrypted (`hw_dec`, no ICV
  error) go to the stack as Ethernet (`da`, `sa`, ethertype, payload). The
  card keeps the CCMP header and MIC (Linux reports `RX_FLAG_DECRYPTED` only);
  `sta::Data::parse` skips them.
- Transmit: the stack's frames go out as QoS data, tid 0, **Protected bit set
  and no CCMP header** — the 8852C writes the header itself (`hw_sec_hdr`; the
  recording's encrypted frames are `0x4188`, the 62-byte ARP has no header
  space), from the packet number in the descriptor (`tx::Desc::data`,
  `wp_offset` 1, security CAM 0). After the keys, EAPOL goes out protected too.
- **Link changes reach DHCP through an atomic**, not a callback:
  `FrameQueues::link_changed` bumps a generation on join and loss, and
  `smoltcp_net::poll` resets the DHCP client when it moves — discovery at once
  on a join, no stale lease after a rejoin.
- **Boot does not wait for DHCP on a wifi link** (`boot_to_init`): the link
  only exists after userspace asks for a join, after `init`. `clock::sync_tick`
  in the netpoll daemon does SNTP once the interface has an address; on boot
  20 that was seconds after the join.

**Two failures on the way, both worth remembering:**

- **The card refuses a second firmware download in one boot**
  (`FWDL_SECURITY_FAIL`, status 3). The first build's join retries
  power-cycled the card (`Card::restart`) and could never come back. Retries
  and rejoins now reuse the running card (`JOIN2`..`JOIN4` overwrite the old
  peer's CAM entries), as Linux re-authenticates without powering anything
  off. How Linux re-downloads after idle power save without this refusal is
  open.
- **A 24 KiB value on a 32 KiB kernel stack**: `flush_transmit` rebuilt the
  queue by value, right after the keys went in. Boot 19 joined and then could
  start no new process — `sleep`, `dmesg` and `reboot` all failed at once and
  the log saved was empty. It now resets two indices.

**Outbound TCP, fixed the same night.** Inbound TCP (sshd), DNS and SNTP
worked, but every outbound `connect` failed — refused at first, timed out
later. Three things were wrong, found in this order:

1. **The link-down address was QEMU's.** With no static config the stack's
   no-lease fallback was `10.0.2.15` (gateway `10.0.2.2`, DNS `10.0.2.3`). The
   wifi link now carries a link-local `169.254.65.77/16` with DNS `1.1.1.1`
   until DHCP answers (`net::WIFI_NO_LEASE_V4`; `ip=` still overrides).
   Not the cause of the failures, but every log line about it misled.
2. **The stack's transmits waited for the daemon's timed lap.** The station
   only drained the stack's queue when its nap ended, so frames queued up and
   were dropped. `FrameQueues::on_transmit` now registers a doorbell that wakes
   the daemon — rung by `smoltcp_net::poll` *after* it releases `NETWORK`
   (`queued::ring_deferred`): the first version rang it from inside the
   critical section and **wedged the box whole** (sshd and the link stopped
   while the watchdog, seeing ticks, never fired).
3. **The root cause: a blocking socket wait starved the station.**
   `net_blocking_relax`, what `connect`/`recv` call between polls, is a bare
   `allow_tick` (drop the BKL, `hlt`) with no yield — correct for a NIC the
   waiter's own `poll()` reads, wrong for a link whose frames only the station
   daemon moves. Kernel code is not preempted here, so on `nosmp` the
   daemon lapped **twice in 10 s** while `nc` waited (1000 times otherwise):
   our SYNs went out, the SYN-ACKs sat in the card's receive ring, the connect
   timed out. sshd (epoll) and DNS (`poll(2)`) park properly and never saw it.
   On the wifi link the relax now yields first. After that: the router's page,
   `example.com`, `example.org`, Firefox's portal check, each in ≤ 1 s.

Diagnosed with the station's own accounting, which stays in: a `[rtw] link:`
line every minute (data frames by fate, A-MSDUs, TX completion statuses, queue
drops, TCP SYN/RST counts, daemon laps and the longest gap) and the first 40
SYN/RST segments logged by port with their IP header checked. A-MSDU frames
are now taken apart and delivered (`sta::Amsdu`) — none arrived on this AP, but
the station advertises A-MSDU reception, so another AP may send them.

**Also learned:** the Mac's application firewall drops unsolicited SYNs to an
unsigned listener without a trace in `netstat` — a LAN test target on the Mac
proves nothing; the router (TCP 53/80/443) is a reliable one.

**Open:**

- `ping` cannot open a raw socket (`Invalid argument`); not wifi-specific.
- Signal strength stays 0 until the firmware's beacon-filter report arrives.

### 5.7 2026-10-06 night: RX replay protection, and the self-host environment on p3

**RX replay protection (task 1 of the next-session list).** The card decrypts
and checks the MIC but leaves the CCMP header in the frame, and nothing checked
that the packet number was *new*: a recorded protected frame could be played
back. `akuma_ieee80211::ccmp` now reads the PN, TID and key id from the header
(`ccmp::header`, QoS or not, with or without HT control) and `ccmp::Replay`
keeps the highest PN accepted per **key and TID** — pairwise (key id 0) and
group (ids 0..=3, each its own space), non-QoS data counted as TID 0, as in
mac80211. `rtw89_sta.rs` runs it on every frame the card decrypted, A-MSDUs
included, before EAPOL or delivery; a group rekey (`JOIN4` replay) resets the
group counters. The `[rtw] link:` line gained `replayed N`. Host-tested
(`cargo test -p akuma-ieee80211`: equal/lower refused, PN 0 accepted once, TIDs
and keys independent, rekey resets only group, header offsets). **Not yet seen on
metal** — kernel built (`no-tests` and plain), entry 12 not re-armed.
Caveat for task 2: once A-MPDU/BA is on, any reordering must happen *before* this
check, or a legitimately reordered frame is dropped as a replay.

**The self-host environment on p3** (the goal: replicate the trashcan's
self-hosting on ryzen, then use Kimi from inside Akuma). Staged from Pop with
`overlays/ryzen/stage-dev.sh`, a cousin of
`scripts/benchmarks/ryzen_fc/stage{2,3}.sh` that writes onto the real partition
rather than a Firecracker image:

| what | where on p3 |
|---|---|
| nightly musl toolchain (`x86_64-unknown-{linux-musl,none}`, `rust-src`, clippy, rustfmt; 1.101.0-nightly 2026-10-05) | `/usr/local/rust`, plus the `libc.so`/`libgcc_s.so` copies lld needs |
| a shallow clone of `ryzen-wifi` with submodules | `/src/github.com/netoneko/akuma` |
| Alpine `git make patch less libgcc` + a monospace font | via `apk.static --root`, db at `/lib/apk` |
| the rig: `/etc/akuma-dev.env`, `/bin/{kbuild,ubuild,mbuild,kinstall}`, `/root/.cargo/config.toml` | from `scripts/box/` |
| goose 1.52.0 + `goose-kimi` (reads the key from `/root/.akuma/kimi/token`, mode 0600, at run time) + goose config (`openai` provider, `https://api.kimi.com`, `coding/v1/chat/completions`, `kimi-for-coding`) | `/usr/local/bin`, `/root/.config/goose` |
| rio (musl, `wgpu,fb`) + the panel config | `/bin/rio`, `/root/.config/rio/config.toml` |

No cargo registry is copied: the clone is small and the box fetches crates
itself (`kbuild --online` the first time, over wifi). Findings: two submodules
(`rumpkernel/src-netbsd`, `tcc/tinycc`) point at commits their remotes do not
have, so the clone reports `fatal` for them and leaves the rest intact — the
kernel build does not need them. `e2fsck -fn` on p3 shows 15 "incorrect
filetype (was 1, should be 7)" dirents for symlinks written by Akuma's own
`apk` (the directory-entry type of a created symlink is wrong; contents fine,
`e2fsck -p` repairs it) — a kernel bug to chase. Long staging steps on this
laptop must be detached (`setsid nohup … &`) and polled: the ssh session over
the wifi drops them otherwise (two copies of the script collided on apk's lock
before this was learned). The Kimi key was copied file to file on Pop from the
kot cat's token; it appears in no log, config or doc.

**Large uploads corrupted on the way out (found 2026-10-06 by goose → Kimi).**
`curl` to `api.kimi.com` worked for POST bodies up to 7 KB and died at 14 KB
with `SSL_read: alert bad record mac` after 39 s — an alert *from the server*,
i.e. our TLS record arrived damaged. goose showed it as "Network error" on every
chat request (its requests are ~15 KB) and pegged the single core retrying (the
fans). Cause, found by reading `crates/akuma-rtw89/src/tx.rs` rather than the
network: the TX channel had **8** WD pages / frame buffers (`PAGES`) and recycles
one as soon as the chip's ring read index passes it, which can run ahead of the
chip's DMA read of the page and frame. The stack hands the driver up to 16
frames per lap (`queued::SLOTS`), so a request of more than ~8 segments
overwrote buffers the chip had not read yet. Fix: `PAGES = 32` (twice `SLOTS`;
3 × 59 KB of static DMA memory). This is the **DMA contract the virtio path
already states** (`crates/akuma-net-nic/src/nic.rs`, `docs/archive/AKUMA_NET_SPLIT.md`):
a buffer handed to the device is owned by it *until the matching completion* —
the rtw89 ring freed on a read index instead of a completion. (The first archive
search looked for the error string only and found nothing; the precedent was
under "DMA", not under "bad mac" — search for the mechanism, not the symptom.) If it recurs, the better fix is
to free a page on the chip's TX release report (what Linux does) rather than on
the ring index — `rpq_status` already counts them, but not per channel.
Also learned: goose ignores `OPENAI_API_KEY` from the environment here (401
`Invalid Authentication` on the first request) and reads `secrets.yaml` with
`GOOSE_DISABLE_KEYRING=1`, so `goose-kimi` writes that 0600 file from the token
at run time.

### 5.8 2026-10-07: what Linux knows, for rio and for the battery

Dumped from Pop onto p3 by `overlays/ryzen/gfx-dump.sh` and
`overlays/ryzen/acpi-dump.sh` (`/root/gfx/gfx.txt`, `/root/acpi/` — Akuma and
Kimi read them there). Pop got `acpica-tools edid-decode vulkan-tools mesa-utils
libdrm-tests fbset` from apt for it. Serial numbers are dropped.

**Graphics (for making rio faster — it renders in software to `/dev/fb0`):**

| | |
|---|---|
| GPU | AMD Radeon 780M (Phoenix1, PCI `1002:1900`, rev cc), `04:00.0`, PCIe 4.0 x16 link, driver `amdgpu`; Mesa RADV 25.1.5 = Vulkan 1.4 on Linux |
| **the framebuffer is the GPU's VRAM aperture** | BAR0 = `0x4b0000000`, 256 MiB, prefetchable — exactly the UEFI GOP framebuffer Akuma maps write-combined. BAR2 `0x80000000` 2 MiB (doorbells), BAR5 `0x80600000` 512 KiB (registers, MMIO), BAR4 I/O `0x1000` |
| VRAM | 2 GiB carved from system RAM (UMA), **all of it CPU-visible** (`vis_vram` = `vram_total` = 2 GiB), GTT 6.7 GiB; sclk 800 / 1100 / 2700 MHz, mclk 400 / 800 |
| panel | eDP-1, 340×220 mm, **1920×1200 @ 60 Hz** (pixel clock 168.15 MHz, htotal 2260, vtotal 1240), 8 bpc, XRGB8888 `rgba 8/16,8/8,8/0`, stride 7680 B, 9 216 000 B per frame. Linux's own fb is 1920×1200×32 too (`amdgpudrmfb`); before amdgpu loads it is `simpledrm` on the same GOP buffer |
| CPU | Ryzen 7 8845HS (Zen 4), 8 cores/16 threads, L2 8 MiB, L3 16 MiB, 14 GiB RAM. **AVX2 and AVX-512** (`avx512f/dq/cd/bw/vl/ifma/vbmi/vbmi2/vnni/bitalg/vpopcntdq/bf16`), `gfni`, `vaes`, `vpclmulqdq`, `sha_ni`, `fsrm`, `erms`. Linux's own software Vulkan (llvmpipe) uses 256-bit vectors here |

What follows for rio, in order of payoff — the first two are measured facts, the
rest is the reasoning, not yet tried:

1. **Akuma runs `nosmp`; the machine has 16 threads.** A software rasteriser
   scales across cores almost linearly; SMP on this target is the largest single
   lever (and the least safe: see the `-j4` notes in the bare-metal runbook).
2. **Every pixel goes through write-combined VRAM at ~3 GB/s** (`map_wc`/PAT, the
   71→3026 MB/s result): a full 1920×1200 repaint is 9.2 MB ≈ 3 ms. Repaint only
   damaged rectangles, and write whole cache lines (64 B) in order — WC
   buffers flush on a full line, partial lines cost a read-modify-write on the
   bus.
3. **Render into ordinary cached RAM, then copy to the aperture in one
   streaming pass** (non-temporal stores, `movntdq`/AVX-512 `vmovntdq`); reading
   back from WC memory is uncached and slow, so never blend against the
   framebuffer.
4. Use the vector ISA: AVX2 is the safe floor, AVX-512 is present (check `XCR0`
   — the kernel must enable the ZMM state in `XSETBV` and save/restore it on
   context switch, which is a kernel change, not a library one).
5. Real GPU acceleration would need the amdgpu stack (PSP/SMU firmware, GFX
   ring, memory manager) — not a near-term option; the register BAR and the
   2 GiB aperture are the only parts that are simple.

**Battery (for the applet; handoff in `docs/handoff-battery-status.md`):** 45
ACPI tables dumped, raw (`/root/acpi/tables/`) and decompiled (`/root/acpi/asl/`):
`DSDT`, `SSDT1..26`, `FACP`, `APIC`, `IVRS`, `CRAT`, `HPET`, `MCFG`, `TPM2`,
`WSMT`, `BGRT`, **`BATB`** (Windows battery table — not the battery's data) and
no `ECDT`. Linux's reading at the time of the dump (the numbers to match):
`BAT0` Li-poly, charging, 87 %, voltage 12.973 V (design minimum 11.31 V),
`power_now` 20.964 W, energy 47.2 / 54.42 Wh full / 57 Wh design, 34 cycles;
`ACAD` and two UCSI source power-supplies exist. The battery data lives behind
ACPI methods (`_BIX`, `_BST`) reading the embedded controller. **Field map
verified 2026-10-07** (`overlays/ryzen/ec-sample.sh`, from Pop): the EC's
memory mirror is readable through `/dev/mem` at physical `0xFEEC2300 + off`, and
reading it reproduces Linux's `BAT0` exactly — voltage 13028 mV, remaining
5294 ×10 mWh, full 5442, design 5700, RSOC 97 %, and **current × voltage / 1000 =
6983 mW = `POWER_NOW`** in all three samples. Offsets (status byte `0x80`:
ACIN bit 0, BTIN bit 1, BTST bits 2-5; design cap `0x84`, design V `0x86`, full
`0x88`, current `0x8c` s16, remaining `0x8e`, voltage `0x90`, RSOC `0x92`) are in
`/root/acpi/linux/EC-FIELDS.md` on p3. The ASL-derived offsets goose first wrote
down are one byte too high on the multi-byte fields. Verified on battery too (3 samples): status
`0b00000110` = no AC, battery in, BTST 1 = discharging; current is a magnitude
in both states (sign from BTST); power again equals `POWER_NOW` to the mW
(47391, 42106, 26107). Open: the *full* and *absent* status codes, and mapping
that page from Akuma.

### 5.9 2026-10-07: rio on the panel — arrow keys fixed, font, and the slowness

First real rio session on this machine, from the panel keyboard. Three things
came out of it.

**Arrow keys (and Home/End/Delete/PgUp/PgDn) did nothing; fixed.** This box's
internal keyboard is real PS/2 behind the EC (§ "Hardware"), so keys reach the
kernel through `amd64/src/kbd.rs` — not the native-USB path the trashcan uses.
That driver decoded letters, modifiers and Alt fine, but **dropped every
0xE0-extended key on purpose** ("Arrows, Home/End, Delete and the like are
dropped"). Fix (2026-10-07, uncommitted in `amd64/src/kbd.rs`): extended keys
now emit the same terminal escape sequences the USB keymap does (`akuma_usb::
keymap` — arrows `ESC [ A..D`, Home/End `ESC [ H`/`F`, `ESC [ 2~/3~/5~/6~` for
Insert/Delete/PgUp/PgDn; Alt+extended-key keeps the "meta sends escape" rule),
through a small lock-free SPSC byte queue that `getb`/`has_byte` drain before
touching the controller, so a sequence is never interleaved with the next key.
Verified with a host harness that runs the actual `decode()` + queue code
(I/O stubbed): every sequence checked, plus `a`, keypad Enter → CR and
Ctrl-D → 0x04 unchanged. **Verified on the metal** (entry 12, 2026-10-07):
panel typing works, arrows reach rio.

**SMP for rio: tried (menu entry 13), two metal boots, wedged both times —
but for a different reason each time.** Entry 12 without `nosmp` (all 16
MADT cores).

*Boot 1 (kbd without a lock):* the keyboard was dead — scancodes *did*
arrive (`[kbd] polls=6000 … scancodes=267`) but none decoded: the i8042
driver is polled, and with 16 cores several pollers race on ports
`0x60`/`0x64` — two cores both see "output full", one takes the scancode,
the other steals the next byte (or the `E0` second byte of an extended
key's pair). The wifi station joined and carried ssh and HTTPS traffic, but
its `replayed` counter climbed into the hundreds and the link fell apart.

*Boot 2 (kbd behind a spinlock — first a plain `spinning_top`, then the
bounded give-up lock from `serial.rs`):* keyboard fixed on `nosmp` (entry
12, typed on and verified), but entry 13 **froze the whole machine before
klog's first flush**. The panel photos of the frozen boot tell the story:

* `SELF-TESTS FAILED; starting init anyway` — the boot suite has failures
  under SMP (names in `dmesg | grep FAILED`, not yet captured).
* `[bkls>] core=12 ticket=22941287 serving=22941286 owner=15 spins=1048576`
  then `spins=2097152`, twice, minutes apart — the BKL stall detector
  reporting a genuine multi-second hold by core 15. The BKL is a fair FIFO
  ticket lock: per `akuma-bkl`, it *cannot* starve a waiter — a wait this
  long is a holder that never releases.
* The PSTATS printer kept ticking (timers alive, which is also why `wdt`
  never fired — the BSP petted it) but **every PID's syscall counts were
  frozen across prints**: the entire system was stopped behind the BKL.

Leading theory (unproven until `klog-30` is read off p3): the wifi station
daemon (`rtw89_sta::daemon`) runs card register polls **with the BKL held**
and `0 timed out` is its healthy number — if the card stops answering under
SMP, the daemon spins forever inside a BKL-held poll loop and takes every
core down with it ("wifi setup interrupted and never recovered"). The two
freeze symptoms — dead panel keyboard and dead wifi — are then the *same*
bug: the keyboard path is syscalls (`read`/`poll` need the BKL), so a BKL
wedge looks exactly like a dead keyboard.

Also fixed along the way, and worth keeping regardless of SMP: the plain
spinlock was itself a landmine on **any** core count — this kernel preempts
threads that hold spinlocks, so a preempted holder with an unbounded
spinner behind it hangs even a single-core boot. `kbd.rs` now uses the
`serial.rs` bounded lock (give up after `1 << 22` spins, report "no key";
the scancode stays in the controller and the next poll retries).

Next: read `klog-30` (+ `dmesg | grep FAILED`) off p3 from Linux; name the
BKL-held loop; either bound the poll, drop the BKL around card I/O (the
dropped-BKL-window machinery exists for exactly this), or both. Until then
**entry 13 stays unusable; rio runs on entry 12 (`nosmp`)**.

**Bisected by core count (2026-10-07, late): the wedge needs all 16.** The
kernel gained an `smp=N` command-line option (`amd64/src/smp.rs`:
`set_cpu_cap_from_cmdline`; N total CPUs, BSP + N-1 APs), and entry 13 was
booted at each width on the metal:

| cores | result |
|---|---|
| 1 (`nosmp`) | clean — keyboard works (bounded-lock `kbd.rs`), rio usable |
| 2 | up, stable. Wifi `replayed` 1–2. ~19 `[bkls>]` long BKL holds, mutually owned (each core takes turns as owner/waiter), all self-healed |
| 4 | up, stable (same profile: ~22 holds) |
| 8 | up, stable (~20 holds) |
| 16 | **wedge**, twice: services start, wifi joins, then `[bkls>] core=12 ticket=… serving=…-1 owner=15 spins=1048576→2097152` with **every PID's syscall counts frozen** across successive PSTATS prints. Both times klog never completed a flush, so the dmesg ring (with the FAILED test names) was lost to the power-cycle |

The step from 8 to 16 is where the SMT siblings come online, so the trigger
is SMT-width-specific — either sibling contention on a per-physical-core
resource, or waiter pressure finally tripping the BKL lost-ticket path.
Also caught on the smp=2 boot: `laps … max gap 18446744073708564 ms` — a
u64 **underflow** in the station daemon's lap timing (`now - last_lap` went
backward): `now_us()` is not monotonic across cores (unsynchronised TSC).
Two concrete SMP bugs to hunt: the BKL 16-core wedge and the cross-core
`now_us()`. The 2 self-test failures under SMP (`physmap: reaches
PHYSMAP_LIMIT`, `pci: every enumerated function has a real vendor id`) still
need names read from a surviving dmesg.

Meanwhile the knob is useful in its own right: **rio runs on entry 13 with
`smp=8`** — 8 software-rasteriser cores, verified up and responsive on the
metal.

**Font.** The panel config started at `size = 32` (the console-cell match,
~40 px lines — huge for real work). 11 (the dev machine's kitty default) is
too small on a scale-1 1920×1200 panel; **22 is the settled value**
(`misc/akuma/config.toml` in the rio fork and the box's
`/root/.config/rio/config.toml`).

**Slowness: measured shape, work ongoing.** rio renders in software on this
kernel's single CPU and presents through the WC VRAM aperture. Every present
is a full 1920×1200 pass: the CRT filter (`crt.rs`, per-pixel map + 3-tap
blur, `akuma-crt-2-flat` in the config) then a 9.2 MB copy at the measured
~3 GB/s (≈3 ms). rio repaints continuously (cursor blink 600 ms), so this
runs even when idle, and each frame blocks the input loop. Levers, in order
of payoff: turn the CRT filter off (one config line); `nosmp` off — the
machine has 16 threads and a software rasteriser scales nearly linearly
(least safe, see the `-j4` notes); damage-rect present instead of
whole-frame; AVX-512 non-temporal stores for the blit. §5.8 above has the
hardware numbers behind all of these. The SMP lever shipped as menu entry 13
(`overlays/ryzen/grub.cfg`) — it boots but kills the keyboard and wifi under
real concurrency (see above); `nosmp` stays until the i8042 and wifi races
are fixed.

## 6. USB ethernet (optional for wifi, nice for everything else)

| # | stage | sessions |
|---|---|---|
| U0 | Existing xHCI driver up on an AMD controller (shared with §4a) | 1–2 |
| U1 | CDC-ECM class driver: bulk in/out, raw Ethernet per transfer, link via the interrupt EP; `ExternalDevice::UsbEcm`. Pick a **plain single-port RTL8153 dongle** (its second USB configuration is standard CDC-ECM, so no vendor protocol). **Avoid dongles with a built-in hub**: Akuma has no hub driver. Phone USB tethering (NCM) is the alternative, at ~1 more session for NTB framing | 2–4 |

It needs more xHCI slots and the event-ring demux by slot ID (today: one disk
slot plus one keyboard slot, as singular statics). It gives ssh into Akuma on
this box. It does **not** remove the reboot from the kernel-change loop.

## 7. The persistent partition: ex-Windows `p3` as ext2

**Done 2026-10-06** with `overlays/ryzen/format-p3.sh --yes-destroy-p3 64G`.
That is the 512 MiB root image `dd`'d onto p3, `e2fsck`'d, grown with
`resize2fs` to a **64 GiB** filesystem (the user's choice: quicker checks; it
grows in place later), and labelled `AKUMA-RYZEN`. This satisfies the
"mkdisk.sh's format" point below, because it *is* mkdisk.sh's image. The script
refuses unless start, length and PARTUUID all match the partition measured
here. The kernel and root image still load from the ESP (p6); the "GRUB reads
p3" step below was not needed.

- **Decision (user, 2026-10-06):** reformat `nvme0n1p3` (279 GiB, "Windows-SSD")
  as ext2 and give it to Akuma. No resizing; the NTFS inconsistency becomes
  irrelevant. Consequences to accept knowingly: Windows and its recovery
  (`p4` WinRE) stop working, and Lenovo BIOS updates normally ship as Windows
  executables (this BIOS is from 2025-07). The Windows Boot Manager entry on
  `p1` becomes dead and can be removed from systemd-boot.
- Format it as ext2 in the format `amd64/mkdisk.sh` produces (block size, revision
  and features `akuma-ext2` supports), not `mkfs.ext2`'s defaults, or
  check those defaults against the crate first.
- **Before Akuma can write it:** NVMe driver (§4b). **Before that:** GRUB
  (with the `ext2` module) loads `akuma-amd64` + `root.img` straight from `p3`
  and `p6` holds only `grubx64.efi`. Once the driver exists, the root moves
  from RAM to `p3` itself (`root=` on the cmdline), and Akuma installs its own
  kernel, as it does on the trashcan.

## 8. Staged and booted, 2026-10-06 — now `overlays/ryzen/`

The staging moved into an overlay: [`overlays/ryzen/`](../../overlays/ryzen/README.md)
(`send` → `build` → `qemu` rehearsal → `install` → one-shot). Its README has
the loop, the menu and the details. In short:

- **The first metal boot was a black screen.** The kernel refused a framebuffer
  above a stale 4 GiB `MAPPED_LIMIT` (ryzen's is at `0x4b0000000`) and halted
  on EGA text, which UEFI cannot show. Fixed. A framebuffer failure, or `nofb`,
  now boots headless instead of halting.
- **The QEMU rehearsal (OVMF on ryzen) then found a real memory-safety bug.**
  UEFI splits the kernel image's range across firmware regions, and the PMM
  handed out the region holding the kernel's first 6 MiB as free frames. Fixed
  in `mem::usable_of`, by carving every reserved span out of every region it
  overlaps. It reproduced on all kernels back to `87f1a7c5`, under KVM and TCG.
- The framebuffer `console` herd service is removed from this image (user's
  call). Entry 0 boots `sshd` + an opt-in `autoreboot` (90 s), so an unwatched
  boot returns to Pop by itself.

Build changes on the box since §8's first version: `grub-common` +
`grub-efi-amd64-bin` installed, their two services disabled; `sora.service` and
`kot.service` stopped and disabled at the user's request
(`systemctl enable --now sora kot` restores them).

The tinycc and `CC=clang` traps from the first build still apply; both are
handled in `build.sh`.

## 9. The NVMe root on the metal, 2026-10-06

`crates/akuma-nvme` (pure, `forbid(unsafe_code)`, 21 host tests plus an ignored
test that parses a real GPT dump) and `amd64/src/nvme.rs` (MMIO/DMA, polled),
selected with `root=/dev/nvme0n1pN`. What the driver guarantees, and how each
guarantee was checked:

| property | how it was checked |
|---|---|
| touches only the selected partition's LBA range | `Window` checks every offset, and every command's LBA run goes through `Window::absolute`; host tests at the edges and on overflow |
| the window comes only from a valid GPT | header CRC **and** entry-array CRC; host tests corrupt each one; the parser read ryzen's real table (p3 = LBA 567296..=586518527, the same as `parted`) |
| writes nothing to a partition without ext2 | the ext2 mount checks the superblock magic before any write; rehearsal `P3=blank`: p3's first 256 MiB were still zeros afterwards |
| a hung command cannot DMA into reused memory | fail-stop: a timeout disables the controller and turns bus-mastering off; nothing is retried |
| the next kernel is not hit by stale DMA | `reboot` path: Flush, `CC.SHN` normal shutdown, bus-mastering off |

**Rehearsed before every metal boot** on ryzen itself
(`overlays/ryzen/qemu.sh DISK=nvme`). QEMU's NVMe device got a sparse 477 GiB
image with ryzen's exact GPT, so the same LBAs and the same p3 window. OVMF
booted it from NVMe, the UEFI NVMe driver had the controller enabled, and the
kernel took it over. Passed: mount and log write; a second boot finding the
first boot's logs (persistence); p3 as one full-size 279 GiB ext2
(`e2fsck -fn` clean); and the blank-p3 fallback above.

**On the metal**, in three steps, the first one read-only:

1. *p3 still NTFS.* No log could survive, so `autoreboot` encoded the furthest
   NVMe stage in its delay. 144 s outside Linux decoded to "GPT read, mount
   refused": the real controller worked and nothing was written. This side
   channel is still in `autoreboot.sh` for any boot that cannot reach the disk
   (`overlays/ryzen/README.md`).
2. *p3 formatted* (§7).
3. *NVMe root.* The first log Akuma wrote to the laptop's own disk, read from Pop:

```
nvme: 1c5c:1d59
nvme: CAP.TO 20000 ms, MQES 256, DSTRD 0, firmware left CC=0x460001 CSTS=0x1
nvme: disabled in 0 ms
nvme: ready in 3 ms
nvme: SKHynix_HFS512GEJ4X112N fw 51040C31, mdts 256 KiB, vwc yes
nvme: ns1 476 GiB in 512-byte blocks, 256 blocks per command
nvme: init total 9 ms
nvme: p3 = LBA 567296..=586518527 (279 GiB) "Basic data partition"
fs:   ext2 mounted on /dev/nvme0n1p3
```

The same log shows the framebuffer at `0x4b0000000`, 1920×1200, cleared in
1.2 ms (7.4 GB/s with write-combining), and 14096 MiB of RAM, all of it
managed, with a 1 GiB heap above 4 GiB. That boot was power-cycled by hand
mid-run; `e2fsck -fn` afterwards was clean.

**The first fully unattended cycle (boot 2).** One-shot armed at 04:49:34; Pop
was back at 04:52:41 with nobody touching the machine, i.e. 184 s outside
Linux, which matches the 140 s stage. `boot-2.early` and `boot-2.dmesg` were on
p3, and `e2fsck -fn` was clean. That is the loop §3 asked for, closed: arm,
reboot, read the log.

**What the takeover timing settled.** Earlier metal boots ran ~18 s longer
outside Linux than a RAM-only boot, which looked like an NVMe timeout. The log
says the takeover takes 9 ms, so the extra time is elsewhere, most likely
firmware POST re-initialising a controller the OS shut down. That is still
unmeasured.

## Suggested order

1. ~~Boot path, one attended boot~~ (done, §8).
2. ~~Log sink~~ (done: the NVMe root, §9).
3. ~~FCH watchdog~~ (done: `amd64/src/watchdog.rs`, verified by a deliberate wedge).
4. W0 → W2 on the loop.
5. Decide on §6 (USB ethernet) and/or a USB wifi dongle before W3.

## Background

- `docs/archive/AKUMA_AMD64_ON_HP_500_502NJ.md`: the same assessment for the trashcan
- `docs/runbooks/amd64-bare-metal-loop.md`: boot options, xHCI rules, self-install
- `docs/archive/AKUMA_FIRECRACKER_AMD64.md`, `crates/akuma-ryzen-amd64`: this box as a Firecracker host
- `docs/archive/LITTER_TRASHCAN_RYZEN_JOIN.md`: ryzen's network role
