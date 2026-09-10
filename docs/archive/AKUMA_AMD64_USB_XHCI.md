# Akuma/amd64: a USB (xHCI) disk for persistence — build log

**Grade: B** (the driver works end-to-end **on the metal** and under
`qemu-xhci`; `sda1` is a mountable persistent root. Verify behaviour rather than
trusting it — one boot path's reset is unexplained, see the last section).
Written 2026-09-06 and revised twice the same day — **the two dated sections at
the end supersede everything above them**, in order.

**The crash-loop is gone**, and so is the bring-up failure. A failed bring-up
halts the controller and the boot carries on; a successful one mounts the disk.
Everything below about the box restarting, and about which step fails on real
hardware, describes what was, not what is.

## Why this exists

Akuma/amd64 on the HP 500-502nj boots from a **512 MiB ext2 RAM image** GRUB loads
whole into memory every boot (`module2 /boot/akuma/root.img`, mounted by
`amd64/src/ramdisk.rs`). It does not persist — every `apk add`, every build
artifact, every edit is gone at the next reboot — and it eats half of the ~3 GiB
the kernel can address. The user wants **reliable persistence for a full
edit → build → run dev cycle on the metal.**

The original plan (`proposals/NEXT_AGENT_AMD64_AHCI_PERSISTENCE.md`, now deleted)
was to move a spare drive to a SATA port and write `akuma-ahci`. That is dead: the
drive is screwed into a caddy the user cannot open, so it stays in a USB-to-SATA
enclosure and Akuma has to speak USB.

## Decisions (with the user, 2026-09-06)

1. **Controller: xHCI, not EHCI.** The keyboard (the other USB consumer) is
   deferred, which removes the one thing that forced EHCI. `XUSB2PRM = 0` on this
   box means xHCI only ever sees SuperSpeed devices — but the enclosure *is* one,
   and it is the only device on the xHCI bus. A single SuperSpeed device with no
   hub is a minimal xHCI: one slot, a command ring, an event ring, a transfer
   ring per endpoint, control + two bulk endpoints, **no split transactions**.
   Bonus: no physical change (the disk is already on a rear SuperSpeed port at
   134 MB/s), 4× an EHCI USB-2 connection.
2. **Boot scope: persistent root only.** GRUB keeps loading the kernel from
   Ubuntu's ext4 `/boot/akuma/` (GRUB 2.12 here has no xHCI module and cannot
   read the disk); the kernel mounts `/dev/sda1` as the writable root.
3. **No MBR-parsing crate.** `sda1` starts at LBA 2048 (the `fdisk`/`mke2fs`
   default) — a hardcoded `SDA1_OFFSET = 1 MiB` constant with an inline
   signature + start-dword sanity check.
4. **Closing the dev loop** (a kernel-installer so Akuma deploys its own builds)
   is explicitly **out of scope for now** — "get the boot done first".

## The disk, prepped (all on the Ubuntu side)

`/dev/sda`, MBR: `sda1` (PARTUUID `21dda1ff-01`, LBA 2048, 64 GiB) is **ext2,
label `AKUMA`, UUID `9329e325-ac7a-4c3d-ac7b-630132acdb76`, 4 KiB blocks**
(`mke2fs` defaults matching `scripts/create_disk.sh`), `e2fsck` clean, last
session's `amd64-root.img` rootfs staged onto it. `sda2` (`21dda1ff-02`, 867 GiB)
raw. See `docs/archive/AKUMA_SELF_HEALING_PORT.md` § "A proper disk" for the SMART
health, the hub-vs-rear-port finding, and the UAS quirk pinned on Ubuntu
(`/etc/modprobe.d/akuma-usb-storage-quirk.conf`).

## Hardware facts (read off `00:14.0` while Linux drove it, 2026-09-06)

Intel 8 Series / C220 xHCI, `8086:8c31`, HCIVERSION 1.0.0. BAR0 `0xf7200000`,
64-bit, 64 KiB.

```
CAPLENGTH   0x80        HCSPARAMS1  0x15000820  (32 slots, 8 intrs, 21 ports)
HCSPARAMS2  0x84000054  (ERSTMax 5, MaxScratchpadBufs 16 — MUST be provided)
HCCPARAMS1  0x200077c1  (AC64=1, CSZ=0 → 32-byte contexts, xECP @ 0x8000)
DBOFF 0x3000   RTSOFF 0x2000
ext caps:  @0x8000 SupportedProtocol USB 2.0 ports 1..14
           @0x8020 SupportedProtocol USB 3.0 ports 16..21
           @0x8040 id 0xC1 (Intel)   @0x8070 id 0xC0 (Intel)
           @0x846c USBLEGSUP  dw0=0x00000001 (Linux had released it)
```

The enclosure (ASMedia `174c:55aa`) was on **root-hub port 20**, SuperSpeed, one
BOT interface (class 8 / subclass 6 / protocol `0x50`), bulk EP `0x81` IN /
`0x02` OUT, `wMaxPacketSize` 1024, **`bMaxBurst` 15**. (It also exposes a UAS alt
setting — protocol `0x62`, 4 endpoints — which the driver ignores.)

These are frozen in `crates/akuma-xhci/tests/xhci_hp_500_502nj.rs` and
`crates/akuma-usb-storage/tests/bot_wire.rs`.

## What was built

Committed as `e647f844 "usb checkpoint"` (crates + kernel wiring), with
`amd64/src/xhci.rs` rewritten + `amd64/src/pci.rs` extended uncommitted on top.

### `crates/akuma-xhci` — pure layout + bit math (`#![forbid(unsafe_code)]`, host-tested)

| module | contents |
|---|---|
| `regs` | capability / operational / runtime-interrupter register offsets + typed decoders (`CapabilityRegisters`, `HcsParams1/2`, `HccParams1`, `PortSc` with RW1C-safe read-modify-write helpers, `config_max_slots_en`, `erst_entry`, doorbell offsets) |
| `trb` | TRB type + completion-code constants; builders (Enable/Disable Slot, Address Device, Configure/Evaluate/Reset Endpoint, No-Op, Setup/Data/Status Stage, Normal, `data_trbs` which splits a buffer at the 64 KiB boundary, Link); `Event::decode`; **`ProducerRing`** and **`ConsumerRing`** — the cycle-bit bookkeeping for command/transfer rings and the event ring |
| `context` | `dci()` (endpoint address → Doorbell Context Index), `SlotConfig`/`EndpointConfig`/`input_control_context` builders returning the 8 context dwords, 32- or 64-byte stride from `HCCPARAMS1.CSZ` |
| `xcap` | extended-capability walk, `USBLEGSUP` BIOS→OS handoff, `SupportedProtocol` (port → USB2/SuperSpeed) |

21 golden-fixture tests.

### `crates/akuma-usb-storage` — BOT wire format (`#![forbid(unsafe_code)]`, host-tested)

`Cbw` (31-byte Command Block Wrapper) encode, `Csw` (13-byte Command Status
Wrapper) parse + `CswStatus`, and `cdb::` builders for `TEST UNIT READY`,
`REQUEST SENSE`, `INQUIRY`, `READ CAPACITY(10)`, `READ(10)`, `WRITE(10)`, each
returning a `Command { cdb, cdb_len, data_len, direction }`. Response parsers
`InquiryData` / `ReadCapacity10` / `RequestSense`. 8 tests.

Descriptor parsing is **not** duplicated — `akuma_usb::descriptor` is
transport-independent and already host-tested; `xhci.rs` consumes it directly.

### `amd64/src/xhci.rs` — the MMIO/DMA glue

- All controller-visible memory (DCBAA, scratchpad array + 32 pages, command
  ring, event ring + ERST, device + input contexts, 3 transfer rings, control
  buffer, CBW/CSW buffers, a 64 KiB bounce buffer) is `.bss` statics behind
  typed accessors — the same DMA discipline as `crates/akuma-net-nic/src/rtl8169.rs`
  (`virt_to_phys` from the kernel-image window, `compiler_fence` before every
  ownership word). Bounce is 4 KiB-aligned; the data phase splits at the 64 KiB
  boundary with `trb::data_trbs` (`.bss` cannot promise > page alignment).
- Bring-up: find the controller (`pci::find_class(0x0c, 0x03)` + `prog_if 0x30`),
  map BAR0 uncached, `pci::enable_full(.., mask_intx=true)`, read the cap block as
  8 × u32, BIOS handoff, stop + `HCRST`, program DCBAAP / CRCR / ERST /
  scratchpad, `USBCMD.RS`, prove the ring loop with a No-Op command.
- Enumerate: scan ports for a connected one, warm-reset if not auto-enabled,
  Enable Slot → Address Device → `GET_DESCRIPTOR` (device, config) → parse the
  BOT interface's bulk endpoints + SS-companion `bMaxBurst` → `SET_CONFIGURATION`
  → Configure Endpoint.
- BOT: `bot_run` (CBW / optional BOUNCE data phase / CSW), `bot_small` (staged
  through BOUNCE for the ≤ 512-byte SCSI commands), `read_bytes` / `write_bytes`
  (whole-disk LBA, RMW for partial blocks).
- Public surface mirrors `akuma_virtio::block`: `init`, `is_initialized`,
  `capacity_sectors`, `read_bytes`, `write_bytes`, plus `SDA1_OFFSET` /
  `mbr_looks_right` / `smoke_test`.

### Kernel wiring

- `amd64/src/fs.rs`: `RootDevice::Usb(UsbDisk)` variant (holds the partition
  byte offset, forwards to `xhci::{read,write}_bytes`); `no_clock` replaced with
  `wall_clock_secs` (feeds `clock::now_us` — 0 until SNTP lands, then correct).
- `amd64/src/multiboot2.rs`: `try_usb_root()` runs when `root=/dev/sda1` is on
  the command line — `xhci::init` → MBR sanity check → `mount_root_on(Usb…)`,
  falling back to the RAM image on any failure; `xhci::smoke_test` added to the
  boot suite (gated on the controller being present).
- `amd64/src/pci.rs`: `enable_full(addr, bus_master, mask_intx)` — new;
  `enable` delegates with `mask_intx=false` (behaviour unchanged for existing
  callers).

Everything builds clean for `x86_64-unknown-none` and the aarch64 kernel;
`cargo clippy` clean; 29 new host tests pass; full host suite green.

## The incident: a faulted bring-up wedged the box across reboots

First metal boot (`root=/dev/sda1 skiptests`): the box crash-looped and would
not answer ssh, and so did a **known-good pre-change kernel** afterwards. Cause:
the first `xhci::init()` set `USBCMD.RS` with **legacy INTx unmasked** and
`USBCMD.INTE` set. The controller raised an interrupt on the first event
(port-status-change) → unhandled IDT vector (no IOAPIC routing on this target) →
fault. Worse, it stayed **running with bus-master DMA active**, pointing at that
kernel's `.bss`. A warm `reboot` does not reset an xHCI controller, so every
later boot got its RAM scribbled on.

**Recovery: full power cycle** (documented in `docs/runbooks/amd64-bare-metal-loop.md`).

**Fixes applied (uncommitted, kernel `f182222d`):**
1. `pci::enable_full(.., mask_intx=true)` — sets `command::INTERRUPT_DISABLE`.
2. `USBCMD` run = `RS` only (no `INTE`, no `HSEE`); `IMAN.IE` left clear. The
   event ring is maintained regardless; the driver polls.
3. `halt_controller(op)` on **every** `xhci::init` error path — `USBCMD=0`, wait
   HCH, `HCRST` — a failed bring-up now leaves the controller reset, never
   running.

## Status: still crash-looping — root cause OPEN

After the power cycle **and** the fixed kernel `f182222d` (`root=/dev/sda1
skiptests`), the box **still crash-looped** ("same behavior, something isn't
landing compared to previous ones"). So the INTx/halt fix is either incomplete
or the crash has a second cause. **What has NOT been tested and must be step 1:**

> Post-power-cycle, boot the **known-good pre-change kernel**
> (`/boot/akuma/akuma-amd64.bak-20260906-005452`, md5 `837db3a5`) with the plain
> `init=/bin/sshd` command line, to confirm the box/controller is actually
> healthy again. If it boots → the fixed xHCI kernel's code is faulting (a bad
> MMIO offset, a `w64` the controller GPs on, a DMA that triggers `#MC` — none
> of which `halt_controller` can catch). If the known-good kernel **also**
> crashes → the power cycle did not clear it / the controller is confused at a
> deeper level, or the `.bss` growth / module presence breaks the boot
> independent of the xHCI code path.

The box is currently on Ubuntu with the known-good kernel restored and GRUB
**not** armed. The fixed xHCI kernel is at
`/boot/akuma/akuma-amd64.xhci-f182222d`; the crashing first version at
`/root/akuma-amd64.xhci-broken`.

## Method notes

- The 64 KiB `dmesg` ring wraps during the ~200-check self-test suite and eats
  early `[xhci]` lines — boot with `skiptests` so `try_usb_root`'s `xhci::init`
  trace survives, or read the screen.
- The `ovmf5` KVM rig on the box runs the multiboot2 path but has **no xHCI
  controller**, so it cannot test the driver — `xhci::init` returns
  `Err("no xHCI controller")` there. A `qemu-system-x86_64 … -device qemu-xhci
  -device usb-storage,drive=…` run would exercise the TRB/ring/enumeration logic
  under emulation (different register layout, 0 scratchpad buffers, but the
  driver reads all that dynamically) — **not yet tried, worth doing** to iterate
  without the metal. *(2026-09-06: done — `amd64/run-xhci.sh`. See below.)*
- Keep the enclosure **off the USB hub** — behind it, sustained writes drop it
  off the bus; straight into a rear port it does 134 MB/s.

## Background

- `docs/archive/AKUMA_SELF_HEALING_PORT.md` § "A proper disk" — the disk-prep
  story, the hub finding, the controller decision.
- `docs/runbooks/amd64-bare-metal-loop.md` — the trashcan loop + the
  wedged-box entry in "Known-broken".
- `crates/akuma-xhci/tests/`, `crates/akuma-usb-storage/tests/` — the register
  and wire fixtures.

---

# 2026-09-06 — the QEMU rig, and what it did and did not settle

The previous session ended with a driver that had never worked on the metal and
a box that crash-looped when it tried. Both of those changed, and not in the way
the handoff predicted.

## The rig

`amd64/run-xhci.sh` — QEMU `-M q35`, `-device qemu-xhci`, `-device usb-storage`,
the PVH entry with `pci` on the command line. `-M microvm` (what `run.sh` uses)
has no PCI bus, which is the whole reason the driver had never run anywhere but
the metal. The PVH path does not scan PCI unless asked, because Firecracker does
not emulate the config ports and a scan there invents devices out of garbage;
`pci` is the boot-time promise that the ports are real.

The fixture is `amd64/mkusbdisk.py`: MBR, ext2 `sda1` at LBA 2048, scratch
`sda2` at LBA 134217728 — the real drive's layout, sparse, so 64 GiB costs about
256 MiB. Without it the four disk checks skip and the run proves nothing about
`READ(10)`/`WRITE(10)`.

**It reproduced the bug on its first run**, in about ninety seconds, after a
whole previous session of cold reboots had not.

## The bug it found: Configure Endpoint claimed EP0

`enumerate` built the Configure Endpoint input context with
`add_flag(0) | add_flag(1) | add_flag(bulk_in) | add_flag(bulk_out)`. `A1` is
EP0, which belongs to Address Device and Evaluate Context; a Configure Endpoint
that also claims it is rejected. QEMU requires the low two bits of the Add flags
to be exactly `0b01` and answers **`TRB Error` (cc=5)** otherwise.

That completion code names the *TRB*, not the flag. The symptom is a bring-up
that resets the controller, starts it, runs a no-op command, resets the port,
enables a slot, addresses the device, reads its descriptors, finds its bulk
endpoint pair — and then fails on the last command before the disk works, saying
nothing about why.

Fixed by `akuma_xhci::context::configure_endpoint_add_flags`, a builder rather
than a comment, because the call site is a chain of `add_flag(..) | add_flag(..)`
in which `add_flag(1)` looks exactly as reasonable as the others — it is what
Address Device used three lines earlier. Pinned by
`configure_endpoint_add_flags_exclude_ep0`.

After the fix the rig runs the whole path: enumerate → address → configure →
`READ CAPACITY` → MBR → `sda1` superblock → `WRITE(10)` round trip. 11/11.

## What the rig did NOT settle

**The metal still fails.** With the EP0 fix deployed, the box's verdict is:

```
Akuma/amd64 self-test: 201 passed, 1 FAILED
  FAILED: xhci: controller + enumeration + BOT bring-up
```

So the EP0 flag was *a* bug and not *the* bug on real hardware — QEMU's
controller is stricter about `A1` than the Intel one appears to be. Which step
fails on the metal is **open**; the `[xhci] .. <step>` breadcrumbs will say, and
reading them is the next agent's first job.

There is also evidence the metal once got much further than any of this
suggests: the scratch LBA on the real disk already holds the self-test's
`(i ^ 0x5a)` pattern, which only `xhci::smoke_test` writes. A full BOT
`WRITE(10)` completed on that hardware at some point. Either the EP0 bug arrived
with the hardening rewrite, or the real controller tolerates what QEMU refuses.

## What stopped the crash-loop

The box no longer restarts. A failed bring-up now halts the controller and the
boot carries on with networking up — 201 checks pass, DHCP leases, SNTP syncs,
sshd serves. Three changes, in the order they matter:

1. **`xhci::quiesce_all`**, on every boot immediately after the PCI scan: clears
   `BUS_MASTER` on every xHCI controller using config space alone. This defends
   against the *previous* boot — a controller left running keeps writing its
   rings into `.bss` inside a kernel image loaded at 2 MiB, and the next kernel
   is loaded into that same memory with the old DMA still in flight. It is
   scoped to xHCI deliberately; the obvious generalisation would take out the
   GPU, whose framebuffer is this machine's only console.
2. **`xhci::shutdown` in `perform_reset`** — the success path had no halt at all,
   so a working bring-up followed by a reboot recreated the same wedge.
3. **The `usb` / `root=/dev/sda1` gate.** The smoke test used to be gated only on
   the controller being *present*, so a "disarmed" GRUB entry still drove it in
   full. There was no way to boot that kernel without touching the hardware,
   which is why "even the fixed kernel still crash-loops" was not a driver
   symptom at all.

## Three diagnostic bugs, all found by trying to read a failure

Each of these hid the others; they are recorded because the class repeats.

- **The verdict counted failures without naming them.** `200 passed, 2 FAILED`
  on a framebuffer console with no scrollback, on a machine that would not start
  sshd *because* the suite failed. `Suite::report` now repeats the names
  (`akuma-selftest`, 16 slots, `&'static str`, no allocation). It paid for
  itself on its first boot.
- **`dmesg` could only ever return 4 KiB.** `sys_syslog` clamped every read to
  its 4096-byte staging buffer while `SIZE_BUFFER` advertised the real 64 KiB
  ring — fifteen sixteenths unreachable, with nothing reporting a short read.
  The NIC's stall dumps then pushed every boot-time diagnostic out of what could
  be read. Fixed by chunking through `serial::klog_snapshot_from`.
- **A failing USB driver took away the tool for debugging the USB driver.**
  `run_shell = passed && have_fs` treats every check as load-bearing, so the
  xHCI failure withheld sshd — the only way to read the breadcrumbs saying why
  it failed. USB failures are now counted separately and do not condemn the
  boot, because USB here is an opt-in peripheral the kernel already falls back
  from.

Also fixed on the way, and unrelated to USB: `DISK=none` and any PVH boot with
no virtio transports died in `akuma_virtio::probe` walking a window that keeps
AArch64's defaults when nothing announces one. `blk::init` now calls
`akuma_primitives::addr::clear_virtio_window()`. That is the "pre-broken"
`DISK=none` note in `amd64_smp_bkl_first`, closed.

## The rungs above the rig

`qemu-xhci` models a *correct* controller, so it catches every way the driver is
wrong about the spec and none of the ways a particular controller is wrong about
it. Past it, all runnable on the box under KVM without touching its own boot:

| rig | adds | contained by |
|---|---|---|
| `usb-storage` backed by the real `/dev/sdb` | the real partition table and filesystem | the VM |
| `usb-host,vendorid=0x174c,productid=0x55aa` | the real ASMedia enclosure's descriptors, stalls, quirks | the VM |
| `vfio-pci,host=00:14.0` | the **real Intel controller**, including the BIOS/SMM handoff | the IOMMU |

The last is the only one that can reproduce a handoff or controller-quirk bug,
and the only one where a runaway DMA is caught rather than landing in RAM —
strictly more protection than the metal has.

**Careful with device names.** On the box's Ubuntu, `/dev/sda` is the *internal*
Toshiba boot disk and `sda2` is `/`. The USB drive is `/dev/sdb`. Akuma only
sees the USB one and calls it `sda`.

---

# 2026-09-06 (later) — the metal enumerates, and `sda1` is the root

`209 passed, 0 failed` on the HP box, `fs: ext2 mounted on sda1`, a file
written from Akuma and read back from Ubuntu on the physical partition. The
whole `[xhci]` trace, on the real Intel controller:

```
[xhci] v0100 slots=32 ports=21 ctx=32B scratch=16
[xhci] proto USB2 ports 1..14 slot_type 0
[xhci] proto USB3 ports 16..21 slot_type 0
[xhci] BIOS handoff ok
[xhci] reset ok
[xhci] running
[xhci] command ring ok
[xhci] port  8 USB2 connected PORTSC=0x000206e1 PLS=7 not-enabled
[xhci] port 20 USB3 connected PORTSC=0x00201203 PLS=0 enabled
[xhci] port 20 enabled, speed 4
[xhci] slot 1
[xhci] addressed
[xhci] BOT ep IN=0x81 OUT=0x02
[xhci] endpoints configured
[xhci] disk: 1953525168 x 512B = 953869 MiB
fs:   ext2 mounted on sda1
```

## What was wrong: the port loop stopped one iteration too early

`find_and_reset_port` took the **first connected port** and broke out of the
scan. On this box that is root-hub port **8** — a USB 2.0 port sitting in
`PLS=7` (Polling) with `PED=0`, which is to say connected to something that had
never been enabled. The disk was on port **20**, already connected *and already
enabled*, and the loop never got there.

It then tried to rescue port 8 with `PORTSC.WPR` — a **Warm Port Reset**, which
is a SuperSpeed-only bit and **reserved on a USB 2.0 port**. The write is
ignored, the port stays in Polling, and a second later the bring-up gives up
with `xHCI port reset timeout`. Two independent defects stacked into one
symptom, and the log line that would have separated them — the *other*
connected port — was the one the `break` threw away.

This is also the answer to the loose end the previous section recorded: the
`(i ^ 0x5a)` pattern already on the scratch LBA of the real disk. An earlier
version of the loop did reach port 20 and did complete a `WRITE(10)`. The metal
had been closer than the verdict said for some time.

## The fixes

1. **`akuma_xhci::xcap::ProtocolMap`** (host-tested) — every Supported Protocol
   capability on the controller, answering "what protocol is root-hub port N?".
   It exists because *a physical SuperSpeed socket is two root-hub ports*, a
   USB 2.0 one and a SuperSpeed one, and which half a device appears on depends
   on whether its SuperSpeed link trained. A driver that does not know which it
   picked cannot know which reset that port accepts.
2. **Print every connected port, not the chosen one.** The old loop printed one
   line — `port 8 PORTSC=0x000206e1` — with no protocol on it and no indication
   that ports 16..=21 had never been looked at. The map is the difference
   between "the reset timed out" and "the reset timed out because that port is
   USB 2.0, and by the way the disk is on 20".
3. **Prefer a SuperSpeed port**, falling back to any connected one. A port whose
   protocol is unknown loses to one known to be SuperSpeed and beats nothing
   else, so a controller with an unreadable capability list behaves as before.
4. **`reset_port` picks the reset the port accepts.** Hot reset (`PR`) first —
   valid on both protocols. A warm reset is SuperSpeed link *recovery*, so it is
   worth a second attempt only on a SuperSpeed port and is skipped outright on
   USB 2.0 rather than spent as another second of timeout. `acknowledging_reset`
   gained `WRC` for the warm case.
5. **Enable Slot takes its Slot Type from the capability** rather than the
   hardcoded 0. Every real part answers 0; taking it from the map costs nothing
   and stops the value being a guess.

## Persistence, as it stands

`root=/dev/sda1` mounts the 64 GiB `sda1` (ext2, label `AKUMA`) as the root
filesystem, with the RAM image as the fallback on any probe failure. Verified
both directions across a reboot: a file Ubuntu wrote is read by Akuma, and a
file Akuma wrote (`cp /AKUMA_DISK.txt /var/copy-test.txt`, 119 bytes) is on the
physical partition when Ubuntu mounts it.

Two userspace gaps remain, and **neither is a disk problem** — they fail
identically on the RAM image:

- **`echo x > file` fails with ENOSYS**, leaving a zero-length file. The shell
  redirect needs `dup2(fd, 1)`, and fds 0/1/2 are handled by number below
  `fd.rs`'s table (`FIRST_FILE_FD = 3`), so `dup2` onto them has nowhere to
  land. This is the `cmd | cmd` entry in the runbook's known-broken table, met
  from a different direction. `cp` writes fine, which is what proves the disk
  path works.
- **`mkdir` is ENOSYS** — the syscall is not implemented.

Fixing `dup2` onto 0/1/2 is now the highest-value userspace change on this
target: it is what stands between a working persistent root and a usable one.

## A note on `root=/dev/sda1 skiptests`

While the bring-up was still failing, that combination **reset the box** where
plain `usb` failed gracefully. It has not recurred since the port fix, and the
two were never A/B'd against the same binary, so what it was is unresolved. If
it comes back, isolate it: `root=/dev/sda1` alone and `usb skiptests` alone,
same kernel. Note that `usb` + `skiptests` does **not** exercise the driver at
all — the smoke test lives inside the suite `skiptests` bypasses, and only
`root=/dev/sda1` runs `xhci::init` on the `skiptests` path.

## 2026-09-10 — it was the socket: High Speed over xHCI has never worked

The disk stopped mounting on the metal. Six boots in one session, every one
falling back to the RAM image with the same three `xhci:` self-test failures
(`read the MBR at LBA 0`, `read the sda1 ext2 superblock`, `WRITE(10) to a
scratch LBA in sda2`).

**Nothing regressed. The drive had been moved to a USB 2.0 socket.** Compare the
2026-09-06 trace above, which mounted `sda1` and round-tripped a file:

| | 2026-09-06 (worked) | 2026-09-10 (failed) |
|---|---|---|
| port | **20** — `proto USB3 ports 16..21` | **3** — `proto USB2 ports 1..14` |
| speed | **4** (SuperSpeed) | **3** (High Speed) |
| `READ CAPACITY` | `1953525168 x 512B` | `1953525168 x 512B` — identical |
| result | `fs: ext2 mounted on sda1` | `transfer timeout: data`, RAM image |

So the SuperSpeed path is the one that has ever worked, and **BOT over xHCI at
High Speed is an untested path in this driver, not a broken one**. The fix for
"the disk does not mount" is to plug it into a blue socket.

**Necessary, and NOT sufficient — read § "It stalls again under use" below
before acting on this.** The socket change is real and it fixes the *boot*;
it does not fix the device. Moved to a USB 3.0 socket (Linux then shows it on
bus 003, the xHCI SuperSpeed root hub, at `5000M`; it had been on `ehci-pci`
bus 002 at `480M`), and the metal came up:

```
[xhci] port 21 USB3 connected PORTSC=0x00201203 PLS=0 enabled
[xhci] port 21 enabled, speed 4
[xhci] disk: 1953525168 x 512B = 953869 MiB
fs:   ext2 mounted on /dev/sda1
```

**`641 passed, 0 failed`** — the first clean bare-metal boot in the session, with
all five `xhci:` disk checks green (`read the MBR at LBA 0`, `MBR signature +
sda1 @ LBA 2048`, `read the sda1 ext2 superblock`, `sda1 superblock magic
0xEF53`, `WRITE(10) to a scratch LBA in sda2`). `df` reports 64 GB with ~63 GB
free, and a write-and-read-back from ring 3 round-trips.

**One trap on the way back in.** With `sda1` really mounted, `sshd` reads
`etc/sshd/authorized_keys` **from the partition**, not from the RAM image — and
a stale copy there locks you out of a box that is otherwise perfectly healthy
(port 2222 open, `Akuma_0.1` in the banner, publickey refused). That is what
`hpbox.restage_disk(keep_keys=True)` is for; run it from Ubuntu before the first
boot onto a persistent root that has been sitting unused. Diagnosing it is
easy once you know: if the key that worked on every RAM-image boot stops
working, the mount *succeeded*.

### The failure shape on the High-Speed path, for whoever fixes it

```
[xhci] BOT ep IN=0x81 OUT=0x02 / endpoints configured
[xhci] .. READ CAPACITY
[xhci] disk: 1953525168 x 512B = 953869 MiB     ← an 8-byte data-in: fine
[xhci] transfer timeout: data                   ← the first 512-byte READ(10)
```
and on the self-test's second bring-up in the same boot:
```
[xhci] bulk cc=0x00000006      ← cc::STALL_ERROR (trb.rs:48)
[xhci] phase CBW
[xhci] transfer timeout: CBW   ← and every transfer after it
```

An 8-byte data-in succeeds and a 512-byte one does not, with `max_packet`
correctly parsed from the descriptor — which points at the data TRB
construction / TD size / bounce-buffer address rather than the endpoint
context. `trb::data_trbs` splits at the 64 KiB boundary and 512 bytes needs no
split, so the single-TRB path is what to read first.

**Two recovery steps are also missing**, and they are why one failure wedges
the device for the rest of the boot rather than costing one retry:

1. **xHCI.** `recover()` issues Reset Endpoint and stops. `trb.rs:138` on
   `reset_endpoint` itself: *"clears a halted (STALL) endpoint's state so the
   transfer ring can be restarted with a **Set TR Dequeue Pointer**"*, and
   `trb.rs:391` repeats it. There is no `set_tr_dequeue_pointer` builder in the
   crate, so the ring's dequeue pointer stays parked on the stalled TRB — which
   is exactly why every later transfer times out in the *first* phase.
2. **BOT.** Class-standard recovery is Bulk-Only Mass Storage Reset (class
   request `0xFF`) then CLEAR_FEATURE(ENDPOINT_HALT) on both bulk endpoints.
   `recover()` does neither, so the *device* stays halted across a controller
   reset — why the second bring-up fails earlier (CBW) than the first (data).

Also: `Xhci::transfer`'s wait loop **silently discards** any event that does not
match `(slot, dci, trb_pointer)`, so "no event arrived" and "an event arrived
and we threw it away" are indistinguishable in the log. Printing the unmatched
ones (bounded) is the first diagnostic to add.

### Ruled out along the way, with the evidence

| not this | how |
|---|---|
| **the media** | `e2fsck -f -n /dev/sdb1` from Ubuntu: five passes, no errors, 156 files, ~63 GB free |
| **the partition table** | `fdisk -l`: valid DOS label `0x21dda1ff`, sdb1 64 G + sdb2 867.5 G, **1953525168 sectors** — the number Akuma's own `READ CAPACITY` returns |
| **the cable** | the disk was physically reconnected mid-session; the next boot's trace was identical line for line |
| **the other USB device** | the keyboard (ROCCAT, `speed 1` on port 8) was removed. Port 8 disappeared and the failure was unchanged |
| **two host controllers contending** | `quiesce_all` was widened from `prog_if == 0x30` (xHCI only) to the whole USB class — `quiesced 1` → `quiesced 3`. Failures unchanged; **reverted**, since it fixed nothing and its crash-loop rationale is unmeasured for EHCI |
| **a recent regression** | `git log -S` puts `read_bytes` and all three checks in the original `caf4076b xhci checkpoint`; the driver has three commits total and none touches the transfer path |

### What Linux does with the same device

`174c:55aa` — ASMedia ASM1051E/ASM1053E SATA bridge. One interface, two alt
settings: **alt 0 = BOT** (`0x81` IN / `0x02` OUT, `wMaxPacketSize` 512) and
alt 1 = UAS. Akuma picks the right pair.

```
usb 2-1.3: new high-speed USB device number 3 using ehci-pci
usb 2-1.3: UAS is ignored for this device, using usb-storage instead
usb-storage: Quirks match for vid 174c pid 55aa: 800000    ← US_FL_IGNORE_UAS
```

Note `ehci-pci`. On this box `XUSB2PR` (`00:14.0` config `0xd0`) reads
`0x00000000` under Linux — every USB2 port routed to EHCI — and `lsusb -t`
shows both xHCI root hubs empty. The machine has three USB controllers (xHCI
`00:14.0`, EHCI `00:1a.0`, `00:1d.0`). So when the drive is in a USB 2.0
socket, **Akuma is the only thing on this box that drives it over xHCI**, and
that path has no Linux cross-check here. In a USB 3.0 socket both stacks use
xHCI and the path is the one that works.

### It stalls again under use — the socket fixes the boot, not the device

Within minutes of that clean boot, with the root mounted and a handful of
successful reads behind it (`df`, `ls /`, a write-and-read-back from ring 3),
the framebuffer console showed:

```
[SSH Keys] WARNING: cannot read /etc/sshd/authorized_keys -- every publickey auth will be refused
[SSH Auth] Publickey auth failed
  [xhci] transfer timeout: CBW
[BKL] stuck: owner=2 waiter=4 tag=511 (aff0+1)
[BKL] stuck: owner=2 waiter=1 tag=511 (aff0+1)
  [xhci] transfer timeout: CBW
```

and the box refused the key it had just accepted — this time not because
`authorized_keys` was stale, but because **the read of it timed out**. So:

| | High Speed (USB 2.0 socket) | SuperSpeed (USB 3.0 socket) |
|---|---|---|
| first 512-byte `READ(10)` | fails immediately | **succeeds** |
| boot self-test's five disk checks | 3 FAIL | **all [OK]**, 641/0 |
| mount `sda1` | no | **yes** |
| sustained use | — | **stalls after a while, and never recovers** |

**This is why the RAM image path exists.** It was adopted as a workaround for
exactly this failure, not as a design choice — so "the disk stopped working" has
a history longer than one session.

### What the recurrence proves about the recovery gap

The two missing recovery steps above stop being a tidy-up and become the
defect. Once *any* bulk transfer stalls:

- Reset Endpoint is issued and **Set TR Dequeue Pointer is not**, so the ring's
  dequeue pointer stays parked on the stalled TRB;
- the **BOT mass-storage reset** and `CLEAR_FEATURE(ENDPOINT_HALT)` are never
  sent, so the *device* stays halted too;

and every subsequent transfer therefore times out in its **first** phase —
which is precisely the repeated `transfer timeout: CBW` on that screen. One
transient stall wedges the root filesystem for the rest of the boot. A device
that stalls occasionally is ordinary; a driver that cannot recover from one
turns it into "the disk is dead".

**Second-order, and worth its own look:** `Xhci::transfer`'s wait loop spins to
`BUDGET` **while holding the BKL**, so every stalled transfer freezes the other
cores for the whole timeout. That is the `[BKL] stuck: … tag=511` storm
interleaved with the timeouts in the photo — a disk fault presenting as a
scheduler fault.

### The order to fix it in

1. **Recovery first**, because it converts a fatal wedge into a retry and is
   independently correct: Set TR Dequeue Pointer after Reset Endpoint, then the
   BOT reset + `CLEAR_FEATURE(ENDPOINT_HALT)` pair, then re-issue the CBW.
   Until this exists, every experiment about *why* it stalls gets one sample per
   boot.
2. **Then the diagnostic**: `Xhci::transfer` silently discards any event that
   does not match `(slot, dci, trb_pointer)`, so "no event arrived" and "an
   event arrived and we threw it away" are indistinguishable. Print the
   unmatched ones, bounded.
3. **Then the stall itself**, with retries making each boot worth many samples
   instead of one.
4. Separately: get the BKL out of the timeout path, or shorten it.

## 2026-09-11 — the recovery gap is closed (in the worktree)

Implemented on branch `amd64-xhci-recovery` (worktree
`../akuma-xhci-recovery`); slices 1–3 and 5 of
`proposals/NEXT_AGENT_AMD64_XHCI_RECOVERY.md`, in that order.
**Slice 4 (why it stalls) is still open — it needs the metal.**

### What landed

1. **`trb::set_tr_dequeue_pointer(slot, dci, dequeue_phys, dequeue_cycle)`**
   in `akuma-xhci`, plus `ProducerRing::cycle()` (the DCS a resume at
   `enqueue_index()` must program). Host-tested alongside the other builders:
   DCS in parameter bit 0, low nibble masked, type 15, slot/dci in control.
2. **`recover()` is now full class-standard recovery**, in spec/Linux order:
   controller Reset Endpoint on the endpoint that stalled → BOT Mass Storage
   Reset (class request `0x21/0xFF`, `wIndex` = the BOT interface number,
   now parsed by `parse_bot_endpoints` and carried in `Xhci::bot_if`) →
   `CLEAR_FEATURE(ENDPOINT_HALT)` on both bulk endpoints → Set TR Dequeue
   Pointer for **both** bulk rings, resuming each at its enqueue position with
   the cycle the TRB there will carry. Non-STALL completion codes still get
   diagnostics only and no retry.
3. **One retry.** `bot_run` splits into `bot_run_once` + a wrapper: on a
   stall it recovers and re-issues the whole command exactly once. A device
   that stalls twice on the same command reports
   `bulk transfer stalled again after recovery`.
4. **The diagnostic.** `Xhci::transfer`'s wait loop prints (bounded, 4) every
   event it would have discarded — unmatched transfer events with cc/slot/dci/
   TRB pointer, and command completions or port events arriving mid-transfer.

### What was verified

- `cargo test -p akuma-xhci` on the host: 26 pass (two new tests).
- The QEMU rig (`amd64/run-xhci.sh`, q35 + qemu-xhci + usb-storage): all 11
  `xhci:` disk checks `[OK]`, WRITE(10) round-trip included. The rig's 4
  `fs:/elf:/spawn:/mmap:` self-test failures are **pre-existing** — baseline
  HEAD shows the identical 359 passed / 4 failed in the same rig, so they are
  not this change.
- Not yet confirmed on the metal; the rig models a *correct* controller and
  cannot reproduce the stall, so slice 4 needs the metal (§ "Iterating the USB
  driver" in the runbook).

### Reading the next stall correctly

With recovery in place the log language changes: a single
`stall recovered — retrying the command once` line followed by success is the
**good** outcome — a transient device halt, absorbed. The bad outcome to
investigate is a stall that recurs on the same phase, and the new
`discarded … event` lines are what say whether the controller ever answered at
all. With slice 5 below, the `[BKL] stuck` storm is no longer the expected
accompaniment to a stall — its presence after this change is new information,
not background noise.

### Slice 5 — the BKL is out of the timeout path (2026-09-11, same branch)

Finding it took one grep past the obvious answer. The proposal assumed
`Xhci::transfer` *chose* to spin under the BKL; in fact every VFS syscall on
this target ran BKL-held, because **amd64's `smp-shared` feature never
forwarded the `no-bkl-*` carve-outs**. The AArch64 root kernel's
`smp-shared` includes `no-bkl-network`/`no-bkl-vfs`/`no-bkl-process`/
`no-bkl-mm`/`no-bkl-drivers`/`no-bkl-irq`; amd64's forwarded only
`akuma-exec/smp-shared`. Glue's `VfsBklGuard` is
`cfg!(all(kernel_smp_shared, kernel_no_bkl_vfs))` — both cfgs off in an
amd64 build — so every `read(2)`/`openat` on this kernel held the BKL straight
through ext2 into the xHCI transfer loop. That is the whole mechanism of the
photo: sshd's `read` of `authorized_keys` parked on the stalled CBW for its
full one-second `BUDGET`, BKL held, three cores queuing, once per timeout.

Two changes, both on the branch:

1. **amd64/Cargo.toml**: `smp-shared` now forwards
   `akuma-syscalls-glue/{smp-shared,no-bkl-vfs}`, `akuma-exec/no-bkl-vfs` and
   `akuma-ext2/no-bkl-vfs` — the VFS phase only. The other phases are
   deliberately not forwarded: this target's net syscalls are its own
   `net.rs` (glue's `no-bkl-network` would not cover them), and each further
   phase gets its own audit + A/B before landing here. The runtime kill
   switch `akuma_bkl::policy::set_vfs_bkl_drop_enabled(false)` still works
   unchanged.
2. **`execve`** (`amd64/src/usermode.rs::sys_execve`) reads the whole image —
   and via the `akuma_elf` VFS hooks, the interpreter — off the same disk, on
   a path no carve-out covered. New `ExecIoBkl` `ToggledGuard` in
   `exec_runtime.rs` over the existing `EXEC_BKL_DROP_ENABLED` policy toggle
   (default on, `set_exec_bkl_drop_enabled(false)` to A/B), scoped to exactly
   the image/interpreter reads; the image switch after them keeps the BKL,
   as the shootdown outermost-lock argument requires.

Rig verification: boot + all 11 `xhci:` checks identical to before the
change, self-test tally unchanged (359/4, the 4 pre-existing).

### Metal verification, same branch (2026-09-11, later the same day)

Deployed via `hpbox.deploy()` (box at `2d9fc4bc` + no-op patch — the work had
already been committed), `stage("root=/dev/sda1")`, `restage_disk` first.

**The recovery works on real silicon, live.** On a SuperSpeed boot with the
persistent root mounted, the enclosure stalled **repeatedly** — 57
`transfer timeout` lines in the first ~17 minutes of uptime — and **recovered
from every one**: the `stall recovered — retrying the command once` count
tracks the timeout count exactly, 57/57, and every retried command succeeded.
The disk never went away: `df` kept answering, ssh stayed up, the box passed
the 17-minute mark while previously the **first** stall had killed the root
filesystem for the rest of the boot (and, the run before this one, ended in a
self-reset). A boot that used to yield one fatal sample now yields a stall
per ~15 s.

What the metal run also showed, honestly:

- **Slice 4 stands.** The device still stalls, roughly every 15 s of use, at
  SuperSpeed, with recovery absorbing each one. Why an 8-byte transfer always
  works and larger ones intermittently stall is still the open question —
  the diagnosis machinery (bounded discarded-event prints) is now in place
  for it.
- **`[BKL] stuck` lines still accumulate during recovery** (~6 per stall,
  `tag=511` in every one, on every boot — the tag is evidently not the BOT
  tag). The VFS and exec carve-outs are in, so these are a path this
  investigation has not named — prime suspect is the recovery window itself
  or the RTL8169 stall-kick path (`[rtl] stall` lines interleave). The
  timeouts no longer freeze the box — it survived, answered ssh throughout,
  and never reset — but the lines are real and someone still holds the BKL
  for >1 s per stall.
- A controlled second metal run ended with the box **resetting itself** into
  Ubuntu after ~10 minutes idle-with-polling — no console witness, dmesg ring
  lost. On the final run (with a `dmesg` snapshot loop writing to the
  persistent root as insurance) it did not recur in 17+ minutes of active
  use. Whether the reset was this code or the box's known-bad NIC is
  undetermined; treat a self-reset on this box as new information, not
  background noise.
