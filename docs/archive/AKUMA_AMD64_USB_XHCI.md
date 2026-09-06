# Akuma/amd64: a USB (xHCI) disk for persistence — build log

**Grade: C** (active — the driver is written, host-tested, and **works
end-to-end under `qemu-xhci`**, but still fails bring-up on the metal; which step
is open). Written 2026-09-06, revised the same day — see the dated section at the
end, which supersedes the "Where it stands" notes below.

**The crash-loop is gone.** A failed bring-up now halts the controller and the
boot carries on: 201 checks pass, the network comes up, sshd serves. Everything
below about the box restarting describes what was, not what is.

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
