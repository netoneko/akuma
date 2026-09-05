# Akuma/amd64 on raw hardware: the self-healing gap list

**Grade: C** (active work — expect this list to shift). Written 2026-09-06,
during the push from "SSH answers on the metal" to "`apk` and HTTPS work on the
metal".

It took roughly **48 hours** of work to get Akuma/amd64 from nothing to
**sshd + HTTPS on the bare HP 500-502nj** — self-tests passing, its own RTL8169
driver at gigabit, a DHCP lease, `ssh akuma "uname -a"` from a laptop, and
`hget https://example.com` returning the page. Most of that time was not the
port; it was the last mile, where a VMM had been quietly compensating for
something the real machine does not.

This document is the running list of what the real machine exposed that a VMM
hid, what is fixed, and what is still open — kept apart from
[`AKUMA_AMD64_ON_HP_500_502NJ.md`](AKUMA_AMD64_ON_HP_500_502NJ.md) (the
chronological bring-up log) so the *gaps* can be read without the narrative.

The theme: **a VMM's `-netdev user` is a helpful lie.** It answers DNS on a fixed
address, NATs anything outbound, hands out a DHCP lease in milliseconds, and
never drops a packet. Real hardware on a household LAN does none of those
reliably, and every item below is a place the code had baked in one of those
assumptions.

---

## Fixed this round

### 1. `resolve_host` (syscall 300) was never wired on amd64

`hget` and anything else on `libakuma-tls` is `no_std` and resolves through
Akuma's private `resolve_host` syscall, not through musl. The amd64 dispatch
table never had an arm for it, so every `no_std` TLS fetch failed at
`DNS resolution failed` while `busybox wget http://…` (musl, its own UDP
resolver) resolved the same name fine.

**Fixed:** `amd64/src/usermode.rs` syscall 300 → `crate::dns::resolve_a`, the
same resolver `clock.rs` uses. Reads the hostname by `(ptr, len)` — it is not
NUL-terminated.

### 2. The kernel DNS client hardcoded the VMM proxy address

`amd64/src/dns.rs` sent every query to `10.0.2.3` — QEMU usermode's fixed DNS
proxy, the address `mkdisk.sh` also writes into the guest's `/etc/resolv.conf`.
Correct for every VMM. **A black hole on bare metal**, where nothing answers on
`10.0.2.3` — `BARE_METAL_STATIC_V4` seeds the resolver as `1.1.1.1` instead, and
`dns.rs` ignored it.

**Fixed:** `dns.rs` now reads `akuma_net::smoltcp_net::static_ipv4().dns` (the
single source of truth the smoltcp DNS socket is seeded from too).

### 3. A configured resolver that answers some names and NXDOMAINs others

Even with the right resolver address, the HP box's own uplink resolver (reached
through slirp under QEMU, and directly on the metal if you point at the gateway)
returns **NXDOMAIN for `example.com`** while resolving `pool.ntp.org` fine —
measured 2026-09-06 both inside the guest and with `nslookup` on the box's
Ubuntu side. A single-resolver client has no way past that.

**Fixed:** `resolve_a` now walks a list — the configured resolver first, then
`1.1.1.1` and `8.8.8.8` — moving to the next server on a timeout *or* an
NXDOMAIN/SERVFAIL/empty answer (`ParseResult::NoRecord`). The total timeout is
split evenly across the servers. musl does the same thing (it queries every
`resolv.conf` server in parallel); this is the sequential-with-fallback version
for a kernel path.

### 4. The wall clock synced once at boot or never

`clock::sync_via_sntp()` ran exactly once, right after `settle_for_dhcp`. On the
metal the DHCP lease is often not ready in time, or the first UDP datagram is
lost while the gateway ARP is still resolving — and then the machine ran at the
epoch **forever**, which means `date` says 1970 and **every TLS certificate is
not-yet-valid**, which `apk` reports as `server certificate not trusted`. Hours
are available to anyone who inspects the CA bundle instead of the clock.

**Fixed:** `clock::sync_tick()` runs every netpoll-daemon lap. It is a no-op once
the clock is set (by SNTP or by a userspace `date -s`) and self-rate-limits to
one attempt per 15 s otherwise. `sync_status()` records the last outcome; the
`netprobe` line now carries `clk=set` / `clk=dns-fail/tryN`.

### 5. No channel to a person watching the machine's own screen

The HP box has a 4K television and no working keyboard (its USB HID stack is
unwritten). The only way to say something to someone standing in front of it,
without going over the network, was to wait for the boot log to scroll past.

**Fixed:** `console_notify` (syscall 322, behind the `console-notify` cargo
feature, default-on for amd64) prints a framed line straight to the
framebuffer/serial console. `/bin/wall` is the userspace front end
(`userspace/wall`). Not busybox `wall` — that writes to logged-in ttys via
utmp, and this target has neither.

### 6. Framebuffer tearing on every scrolled line

`akuma-fbcon` cannot scroll by copying pixels (video memory is write-combining —
fast to write, ruinously slow to read back), so it shifts the character grid and
redraws changed cells directly on the visible framebuffer. Once output reaches
the bottom that is a near-full-screen redraw *per printed line*, which on a
television reads as a tear sweeping down the picture continuously.

**Mitigated, not fixed:** `Console::SCROLL_ROWS = 8` — a scroll now advances 8
rows at once, so the redraw happens once per 8 lines instead of every line, with
a few blank rows at the bottom that fill in before the next scroll. The real fix
(a RAM shadow buffer, or a WC-aware block copy) is still open — see below.

### 7. No way to skip the self-test suite

The ~200-check suite runs on every boot. It is the right default — it is how a
regression in a shared crate is caught before the metal — but re-proving demand
paging and the ELF loader on every reboot of a *trusted* build is time spent
watching a television scroll.

**Fixed:** `skiptests` on the kernel command line (multiboot2 path) brings the
machine straight to `init`, still doing the handful of `init_*` calls the suite
happens to also perform (LAPIC, console fd, syscall MSRs, secondary cores).

### 8. Sign-on banner

`run_init` now prints Akuma's cat and the version line
(`amd64/src/banner.rs`, shared with `usermode::UTSNAME` so `uname` cannot
disagree) as the last thing before the init program starts — so on the
television, that is what is on screen when sshd comes up.

---

## Does it happen on QEMU / Firecracker too?

| gap | QEMU `microvm` (virtio-net) | QEMU q35 (multiboot2) | Firecracker | bare metal |
|---|---|---|---|---|
| syscall 300 unwired | **yes** (was ENOSYS everywhere) | yes | yes | yes |
| DNS hardcoded `10.0.2.3` | no — `10.0.2.3` is real there | no | no — dnsmasq answers there | **yes** |
| resolver NXDOMAINs a name | only if the host resolver does | same | same | **yes, observed** |
| SNTP one-shot never retries | rarely bites (DHCP is instant) | rarely | rarely | **yes, every slow lease** |
| framebuffer tearing | n/a (serial console) | **yes** (has a framebuffer) | n/a | **yes** |
| `apk update` | **works** (verified 2026-09-06) | not tested | not tested | pending metal re-verify |
| kernel SNTP landing | **works** (verified) | not tested | not tested | **works** (verified 2026-09-06) |

Verified on the QEMU `microvm` rig (`/root/qrun2.sh`, virtio-net, `-netdev
user`): with fixes 1–3 in, `hget http://example.com` fetches (via the `1.1.1.1`
fallback, because the box's slirp DNS NXDOMAINs `example.com`), and `apk update`
reports `OK: 28641 distinct packages available`. Kernel SNTP sets a correct
clock at boot. So the core code is sound; the bare-metal failures were the
network-path assumptions above, not the syscall surface.

Firecracker: **not exercised this round.** The DNS and SNTP fixes should be
inert there (dnsmasq answers on `10.0.2.3`, so the configured resolver is tried
first and succeeds), but this is unverified.

---

## Still open

### Memory: the box has 16 GiB, the kernel uses a sliver of it

`apk add tar && apk add tcc` **succeed** on the metal (packages install; the only
errors are `failed to preserve … owner`, from an unimplemented `chown` on the
extract path — non-fatal). Then `ls` reports `Out of memory` and the box has to
be power-cycled.

Two hard limits, neither a tuning knob today:

- **`HEAP_SIZE` was 64 MiB** (`amd64/src/mem.rs`). The kernel heap is a single
  fixed slab, and `sys_openat` caches whole files in `Vec`s that are never
  evicted (`busybox` 1.1 MiB, `apk` 5.4 MiB, one per package payload). A couple
  of installs exhaust it. **Raised to 512 MiB 2026-09-06** — the sub-4 GiB
  region it is carved from has gigabytes free. A file-cache eviction policy
  (the AArch64 kernel's `akuma-fpcache`) is the real fix; the bump buys time.
- **`PHYSMAP_LIMIT` is 4 GiB** (`amd64/src/phys.rs`). `boot.s` builds four
  2-MiB-page directories, so the kernel can address only the first 4 GiB of
  physical RAM. On the HP box the usable RAM below 4 GiB is ~3.2 GiB
  (`MemTotal: 3354104 kB`), of which ~2.6 GiB is free — the other ~13 GiB is
  physically present but remapped above the 4 GiB line and unmapped. `free`
  reports 2.6 GiB on a 16 GiB machine.

  **Attempted 2026-09-06, reverted:** extending `boot.s` to 16 page directories
  (32 GiB) and `PHYSMAP_LIMIT` to match. It **boots and works under QEMU** — the
  `ovmf5` rig with `-m 10240` picked a RAM region at `0x1_0000_0000`, put the
  heap and an 8 GiB PMM there, and ran the self-tests green. It **triple-faults
  on the metal** ("simply reboots instead of bringing up ssh"). The likely
  cause: a flat cached 0–32 GiB identity/physmap now lays a writeback mapping
  over the 64-bit device BARs and reserved ranges above 4 GiB that this
  platform's memory controller will `#MC` on — the 4 GiB map got away with a
  cached alias of the framebuffer BAR at 3.5 GiB, but not everything up high.

  **The right approach:** keep `boot.s` at 4 GiB (the minimum the framebuffer
  needs), and after `machine::describe` has parsed the UEFI memory map, build
  the physmap's slots 4+ **in Rust from the RAM regions only** — never mapping
  the holes. That is a real piece of work, not a constant bump.

The `dmesg` ring buffer (below) now prints `mem: heap …/… KiB, pmm … MiB free`
every 10 s, so the next leak shows up as a line that only climbs.

### `dmesg` works over ssh now

`serial.rs` keeps the last 64 KiB of console output in a `.bss` ring, and
`syslog(2)` (x86_64 103, `klogctl`) serves it — so `ssh akuma "dmesg"` returns
the kernel's own boot log and diagnostics. On a box whose console is a
write-only framebuffer this is the only way to read them; every "why did it do
that" before this was a photograph or a guess.

### A proper disk

Today the rootfs is a **512 MiB ext2 image GRUB loads into RAM** (`module2 …
root.img`, `mkdisk.sh` `SIZE_MIB`), mounted by `ramdisk.rs`. It does not persist
and it competes with everything else for the sub-4 GiB window.

**Decided 2026-09-06: a USB disk over xHCI.** The earlier options below assumed
SATA + `akuma-ahci`; that is off the table because the spare drive is screwed
into a caddy the user cannot open, so it stays in its USB-to-SATA enclosure and
Akuma has to speak USB to reach it. `akuma-ahci` is shelved — not wrong, just
not the path while the only spare disk is trapped on USB.

#### What was learned about the enclosure (all on the Ubuntu side, 2026-09-06)

- **The drive is fine.** Seagate ST1000LM035, SMART `PASSED`, 8 reallocated
  sectors, **0 pending / 0 uncorrectable**, 0 UDMA CRC errors, ~1900 power-on
  hours. Old, lightly worn, not failing.
- **The USB *hub* was the flakiness.** Behind the hub, on USB 2.0, sustained
  writes dropped the device off the bus entirely mid-transfer (`usb …: USB
  disconnect`, `DID_NO_CONNECT`) — this was last session's `mkfs.ext2` stall.
  Plugged **straight into a rear port** the enclosure enumerates on `xhci_hcd`
  at SuperSpeed and does **134 MB/s writes with zero errors** (1 GB test).
- **The bridge is UAS-buggy — force Bulk-Only Transport.** ASMedia `174c:55aa`.
  Under the `uas` driver even a rear port wedges on I/O. Ubuntu now has
  `/etc/modprobe.d/akuma-usb-storage-quirk.conf` pinning
  `usb-storage quirks=174c:55aa:u` (disable UAS → BOT), initramfs rebuilt.
  Fortunate, because **BOT is exactly what Akuma implements** — so Linux-under-BOT
  is the representative oracle.
- **xHCI on this box only ever sees SuperSpeed devices.** `setpci -s 00:14.0
  0xD0.l 0xD4.l` reads `XUSB2PR = 0` and **`XUSB2PRM = 0`** — firmware hardwires
  it (Lynx Point `8086:8c31`). Every full/low/high-speed device — the keyboard,
  a USB-2 disk — routes to **EHCI**, no BIOS option. `USB3PRM = 0x3f`: 6
  SuperSpeed ports, all enabled.
- **GRUB 2.12 here has `ehci/ohci/uhci/usb/usbms` modules but no `xhci`.**
  Mainline GRUB still cannot read an xHCI-attached disk. This is why the kernel
  image stays on Ubuntu's ext4 `/boot/akuma/` (GRUB loads it from there) and only
  the *root filesystem* moves to USB.

#### The disk, prepped

`/dev/sda`, MBR: `sda1` (PARTUUID `21dda1ff-01`, LBA 2048 = 1 MiB, 64 GiB) is
**ext2, label `AKUMA`, UUID `9329e325-…`, 4 KiB blocks**, `mke2fs` defaults
matching `scripts/create_disk.sh`, `e2fsck` clean, last session's `amd64-root.img`
rootfs staged onto it. `sda2` (`21dda1ff-02`, 867 GiB) raw.

#### Why xHCI and not EHCI

The keyboard is deferred, which removes the one thing that forced EHCI (a
full-speed HID device behind the Intel rate-matching hub — the split-transaction
problem `crates/akuma-usb/src/ehci.rs` is built around). With the keyboard out:
the disk is already stable on the SuperSpeed port at 134 MB/s (4× a USB-2 EHCI
connection), needs no physical change, and a single SuperSpeed device with no hub
is a *minimal* xHCI — one slot, a command ring, an event ring, control + two bulk
endpoints, **no split transactions**. `crates/akuma-usb`'s `descriptor.rs` (USB
descriptor parsing) carries over; the 631-line `ehci.rs` stays for when the
keyboard comes back.

Plan: a `akuma-xhci` crate (register/TRB/context layout, host-tested against
register values captured off `00:14.0`), an `akuma-usb-storage` crate (CBW/CSW +
minimal SCSI: `INQUIRY`, `TEST UNIT READY`, `READ CAPACITY(10)`, `READ(10)`,
`WRITE(10)`, `REQUEST SENSE`), the MMIO/DMA half in `amd64/src/xhci.rs` (`.bss`
rings, `virt_to_phys`, `compiler_fence` before ownership words — the
`akuma-net-nic/src/rtl8169.rs` pattern), a `RootDevice::Usb` arm in
`amd64/src/fs.rs`, and a `root=/dev/sda1` cmdline token on the `multiboot2.rs`
path that falls back to the RAM image on any probe failure. `sda1`'s 1 MiB offset
is a hardcoded constant with a `0x55AA` + start-dword sanity check, not an
MBR-parsing crate. Full plan in `proposals/` and the plan file for the session
that builds it.

#### Closing the dev loop — open design question

The loop today (`docs/runbooks/amd64-bare-metal-loop.md`) is: edit on the laptop
→ rsync to Ubuntu → build on Ubuntu → `cp` kernel to `/boot/akuma/` → `grub-reboot`
→ boot Akuma. The Ubuntu steps are the open part.

A persistent USB root closes the loop for **rootfs contents** (packages, built
binaries) — writes just survive. It does **not** by itself close the loop for the
**kernel image**, because GRUB loads that from Ubuntu's disk and cannot read the
xHCI USB disk. Ideas on the table (2026-09-06, not yet decided):

- **A small userspace kernel-installer that writes to a FAT filesystem.** GRUB
  always has FAT support and the box already has a FAT32 ESP. A portable tool
  that lays a kernel image into a FAT partition — **testable against a raw block
  device or a loopback image so it never touches the real disk** — would let
  Akuma install its own freshly-built kernels. Catch: the ESP is on the SATA
  disk (no `akuma-ahci`), and a FAT partition on the *USB* disk is not readable
  by GRUB (no xHCI). So this wants either EHCI (GRUB can then read a USB-2 FAT
  partition) or `akuma-ahci` after all — it does not compose with the xHCI-only
  decision above without one of them.
- **Run `debugfs` (e2fsprogs) on Akuma.** `mkdisk.sh` already uses `debugfs -w -R
  write` to populate an ext2 image on macOS with no mount and no kernel FS write
  path. A **static** `debugfs` on Akuma, pointed at `/dev/sda1`, would manage the
  root filesystem's contents the same way — sidestepping `akuma-ext2`'s write
  path entirely. Needs: a static musl build of e2fsprogs (Alpine's is in
  `e2fsprogs-extra`, dynamically linked — a static build is the work), and raw
  block-device `open()`/`pread()`/`pwrite()` on `/dev/sda1`, which the USB block
  driver has to expose anyway. Still does not place a kernel where GRUB reads it.

The honest summary: **xHCI gets a persistent, writable root and a closed loop for
everything except the kernel image.** Fully closing the kernel half needs EHCI
(so GRUB can read the USB disk) or `akuma-ahci` (so Akuma can write Ubuntu's
disk) — a decision to make once the xHCI root is real and the remaining friction
is measured rather than guessed.

#### Superseded options (needed `akuma-ahci`, which is shelved)

1. A second SATA disk, Akuma owns it whole. Cleanest, but the spare disk is on
   USB and `akuma-ahci` does not exist.
2. A third partition on the Toshiba (shrink the ext4). Driver work plus a risky
   resize.
3. The SD-card slot — USB-attached, so it needs the same USB stack anyway.
4. **Stay on the RAM disk, bigger.** Bump `mkdisk.sh` `SIZE_MIB` (done — 512 MiB
   2026-09-06). Not persistent, but keeps `apk` from OOMing in the meantime.

### High value

- **Pipes and redirects.** `cmd | cmd` → `can't create pipe`. fds 0/1/2 are
  handled by number *below* `fd.rs`'s table (`FIRST_FILE_FD = 3`), so `dup2`
  onto them has nowhere to land. The table needs real entries for 0/1/2. This is
  the single highest-value fix for shell usability on the box.
- **Session-state degradation under repeated ssh.** After many back-to-back ssh
  sessions the kernel starts returning ENOSYS ("Function not implemented") for
  `execve` of disk binaries and closing connections mid-command ("Connection
  closed by remote host"), while pure builtins (`echo`, `uname`) still work. A
  fresh boot clears it. Looks like a spawn-slot / fd / smoltcp-socket leak on
  the session teardown path — not yet root-caused. This is why `apk` "worked on
  QEMU but not the metal" in one session: the metal had been hammered first.
- **`apk` past the certificate error: `Read-only file system` (EROFS).** Noted in
  the handoff prompt, not reproduced this round (blocked behind the degradation
  above on the metal, and `apk update` alone succeeds on QEMU). `EROFS` is **not
  in `amd64/src/fd.rs`'s errno table at all**, so something else is producing the
  string — worth finding where. The next step after this is mounting a real
  writable disk on the box.

### Framebuffer

- **Real tear-free scroll.** The `SCROLL_ROWS = 8` chunking is a mitigation. A
  proper fix keeps a shadow of the framebuffer (or at least the text region) in
  cached RAM, scrolls that with `copy_within`, and blits only the changed
  span to video memory — turning a per-line near-full redraw into one bounded
  memcpy. ~33 MB of shadow at 4K is the cost to weigh; the allocator is up by
  the time fbcon scrolls.

### Networking

- **The RX stall.** `rx` stops at exactly 16 (`RING_LEN`) and is revived by
  re-arming `RDSAR`/`RCR`/`CR_RE` in `kick_receiver`. Uncapped, keeps the box on
  the network, `kicks=` in the probe counts it — but it is a workaround, not a
  diagnosis. Why the chip and driver desynchronise, and why completions once
  landed 182 KiB *below* the ring, are both unknown.
- **`nslookup`** fails (`write()` on a connected UDP socket) while DNS itself
  works.
- **`resolve_a` from a syscall context** blocks the netpoll daemon for up to its
  timeout (the daemon and the syscall both call `smoltcp_net::poll()`, and the
  syscall's hand-rolled loop does not yield the daemon's work). With the
  per-server timeout split this is bounded at a few seconds worst case, but a
  `wait_until`-style parked wait would be cleaner.

### Missing subsystems

- **procfs.** `/proc` is an empty directory. `ps`, `top`, `free` need a real one.
- **USB HID.** The box's keyboard is FS-on-EHCI (not xHCI — `XUSB2PRM = 0`), so
  it stays an EHCI + split-transaction problem regardless of which port it is in.
  **Deferred 2026-09-06** in favour of the USB disk (which is xHCI-only). The
  framebuffer is output-only and ssh is the only console until then.
- **USB mass storage.** In progress 2026-09-06 — see "A proper disk" above.
  `akuma-xhci` + `akuma-usb-storage` + BOT, single SuperSpeed device.

---

## Background

- [`AKUMA_AMD64_ON_HP_500_502NJ.md`](AKUMA_AMD64_ON_HP_500_502NJ.md) — the
  chronological bring-up, including the three wrong diagnoses of the RX stall.
- [`AKUMA_FIRECRACKER_AMD64.md`](AKUMA_FIRECRACKER_AMD64.md) — the earlier
  Firecracker/QEMU port this built on (§3.29–3.30 cover the first SNTP and DNS
  work).
- [`../runbooks/amd64-bare-metal-loop.md`](../runbooks/amd64-bare-metal-loop.md)
  — how to edit and test on the box without touching it.
