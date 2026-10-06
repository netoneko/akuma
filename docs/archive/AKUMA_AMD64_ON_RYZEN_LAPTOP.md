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
| FCH watchdog (a wedge still needs a hand on the power button) | **next** |
| wifi W0–W5 (§5) | not started |

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

**Total: 11–24 sessions and many dozens of reboot cycles.** W0–W2 can run
entirely on the reboot loop. W3 onward needs an AP to test against and
eventually a way to talk to Akuma, which is where §6 helps.

A cheaper wifi path, if the goal is "Akuma on wifi" and not "this card": a USB
wifi dongle with a small, well-understood chip (MT7601U being the classic
choice). It reuses the xHCI work in §6 and skips PCIe DMA and the 802.11ax
tables. W3–W4 are needed either way.

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
3. **FCH watchdog.** From here the loop is unattended even when the kernel wedges.
4. W0 → W2 on the loop.
5. Decide on §6 (USB ethernet) and/or a USB wifi dongle before W3.

## Background

- `docs/archive/AKUMA_AMD64_ON_HP_500_502NJ.md`: the same assessment for the trashcan
- `docs/runbooks/amd64-bare-metal-loop.md`: boot options, xHCI rules, self-install
- `docs/archive/AKUMA_FIRECRACKER_AMD64.md`, `crates/akuma-ryzen-amd64`: this box as a Firecracker host
- `docs/archive/LITTER_TRASHCAN_RYZEN_JOIN.md`: ryzen's network role
