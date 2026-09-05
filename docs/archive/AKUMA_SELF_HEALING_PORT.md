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
  physical RAM regardless of how much the machine has. On the HP box that is
  ~2–3 GiB of usable RAM after the PCI hole — plenty for now, but "use the full
  16 GiB" needs more page directories in `boot.s` **and** the physmap has to
  either skip the MMIO hole (framebuffer BAR ~`0xE000_0000`, LAPIC
  `0xFEE0_0000`) or it will lay a cached alias over every device register. Not
  started.

The `dmesg` ring buffer (below) now prints `mem: heap …/… KiB, pmm … MiB free`
every 10 s, so the next leak shows up as a line that only climbs.

### `dmesg` works over ssh now

`serial.rs` keeps the last 64 KiB of console output in a `.bss` ring, and
`syslog(2)` (x86_64 103, `klogctl`) serves it — so `ssh akuma "dmesg"` returns
the kernel's own boot log and diagnostics. On a box whose console is a
write-only framebuffer this is the only way to read them; every "why did it do
that" before this was a photograph or a guess.

### A proper disk

Today the rootfs is a **128 MiB ext2 image GRUB loads into RAM** (`module2 …
root.img`), mounted by `ramdisk.rs`. It does not persist and it competes with
everything else for the sub-4 GiB window. Options for real storage, in rough
order of effort:

1. **A second SATA disk.** The box's Intel C220 has 6 ports; add a small
   SSD/HDD and Akuma's driver owns it whole — no partition table to share, no
   risk to the Ubuntu install. **Needs an AHCI driver** (there is none;
   `pci::scan` *finds* the controller, `blk.rs` only speaks virtio-blk).
2. **A partition on the existing 1 TB Toshiba.** `/dev/sda` is GPT: a 1.1 GB
   FAT32 ESP and a 999 GB ext4 that runs to the end of the disk. Shrink the
   ext4 offline, add a third partition, format ext2, mount it from the AHCI
   driver. Same driver work as (1) plus a risky resize.
3. **The SD-card slot** (`sdb Multi-Card`). Almost certainly USB-attached, so
   it needs the USB mass-storage stack — which needs the EHCI driver that the
   keyboard is also waiting on. Worst effort-to-value.
4. **Stay on the RAM disk, bigger.** Bump `SIZE_MIB` in `mkdisk.sh` and (once
   `PHYSMAP_LIMIT` is raised) give it more room. Not persistent, but enough to
   run `apk` without OOMing in the meantime.

The common blocker for (1) and (2) is **`akuma-ahci`**, which does not exist
yet. That is the next real subsystem.

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
- **USB HID.** The box's keyboard is FS-on-EHCI (not xHCI). No EHCI driver, so
  the framebuffer is output-only and ssh is the only console.

---

## Background

- [`AKUMA_AMD64_ON_HP_500_502NJ.md`](AKUMA_AMD64_ON_HP_500_502NJ.md) — the
  chronological bring-up, including the three wrong diagnoses of the RX stall.
- [`AKUMA_FIRECRACKER_AMD64.md`](AKUMA_FIRECRACKER_AMD64.md) — the earlier
  Firecracker/QEMU port this built on (§3.29–3.30 cover the first SNTP and DNS
  work).
- [`../runbooks/amd64-bare-metal-loop.md`](../runbooks/amd64-bare-metal-loop.md)
  — how to edit and test on the box without touching it.
