# Akuma/amd64 on the HP 500-502nj — what this box can and cannot do

2026-09-05. Host `192.168.1.123` (`vaporwave`), root access, on the LAN.
Kernel under test: tag `v0.0.7-akuma-amd64-apk-checkpoint` (`b5b1da8d`).

Written in answer to "set up firecracker there, deploy akuma on fc, then resize
the machine to boot akuma by itself" — this is the assessment of what of that is
possible on this particular machine, and at what cost.

## Verdict

1. ~~**Firecracker is installed and cannot run.**~~ **RESOLVED the same day.**
   VT-x was disabled and locked by the firmware; the user enabled it in BIOS
   setup and cold-booted, `/dev/kvm` appeared, and **Akuma now boots on this
   machine under Firecracker v1.16.1** — 173/173 self-tests, DHCP, SNTP, sshd,
   and `apk` installing real packages over the network. See §4.
2. **Everything else is already working.** The kernel builds natively on the box
   in 17 s, the 128 MiB ext2 image builds in 30 s, and the guest boots under
   QEMU/TCG with **173/173 self-tests**, DHCP, SNTP and sshd. **`apk` works** —
   `apk update` against the real Alpine CDN and two real installs, verified on
   this hardware today. One small real gap found (`chown`, §4).
3. **Bare metal is a project, not a switch.** This box has **no UART at all**,
   and Akuma/amd64 has no console other than a 16550, no PCI enumeration, no
   AHCI, no driver for its NIC, and no bare-metal boot protocol. §5 is the gap
   list; §6 is the order I would do it in.

## 1. The machine (all measured, not assumed)

| | |
|---|---|
| Model | HP 500-502nj, board `2B2C`, AMI BIOS **80.06 / 2015-04-01** (SMBIOS rev 4.6) |
| CPU | Intel **i5-4460** (Haswell), 4 cores / 4 threads, 3.2–3.4 GHz |
| RAM | **16 GiB** (2 × 8 GiB DDR3-1600, DIMM1 + DIMM2) |
| Firmware | **UEFI**, 151 efivars, **Secure Boot disabled**, legacy "USB Floppy/CD" boot entries present (CSM likely available — verify in setup) |
| Disk | Toshiba DT01ACA100 **1 TB HDD** (`sda`): `sda1` 1 GiB ESP (vfat), `sda2` 930 GiB ext4 — **62 G used, 807 G free** |
| SATA | Intel 8-series 6-port **AHCI** `8086:8c02` |
| NIC | **Realtek RTL8111/8168** rev 0c `10ec:8168`, driver `r8169`, 1000 Mb/s link, MAC `60:02:92:61:4e:73` |
| GPU | NVIDIA GM107 **GTX 745** `10de:1382` in the x16 slot |
| USB | Intel xHCI + 2 × EHCI |
| Free slots | **PCIe x1: Available**. Mini Card: Available. x16: in use (GPU) |
| **Serial** | **none** — see below |
| VT-d | DMAR table present (IOMMU exists) |
| OS | Ubuntu 25.04, kernel 6.14.0-37, GNOME desktop, Docker |

**No serial port.** `/proc/tty/driver/serial` reports `uart:unknown` at every
legacy address (0x3F8/0x2F8/0x3E8/0x2E8) — that is the 8250 driver having
*probed* and found nothing. No `PNP0501` ACPI device, no serial port connector in
SMBIOS type 8. The `ttyS0..ttyS31` nodes are the driver reserving 32 slots, not
32 ports. Treat this box as UART-less; it matters more than anything else in §5.

## 2. What is now set up on the box

| what | where |
|---|---|
| Firecracker + jailer v1.16.1 | `/usr/local/bin/firecracker`, `/usr/local/bin/jailer` |
| Rust nightly 1.100.0 + `x86_64-unknown-none` | `/root/.cargo`, `/root/.rustup` |
| `rust-readobj` shim → `llvm-readobj` | `/usr/local/bin/rust-readobj` (the Linux toolchain ships only the `llvm-` name; `amd64/run.sh`'s PVH-note check wants the `rust-` one) |
| Source at the tag | `/root/akuma` (`git archive` of the tag + the `tinycc` submodule and `userspace/tcc/{vendor,dist}` caches, which `git archive` cannot carry) |
| Guest network | `/usr/local/sbin/akuma-tap-up.sh` — `tap0` at 10.0.2.2/24, dnsmasq DHCP pinned `02:FC:00:00:00:01 → 10.0.2.15`, NAT out via `eno1`, log `/var/log/akuma-dns.log` |
| QEMU (the no-KVM stand-in) | `qemu-system-x86` from apt |

The addresses are deliberately QEMU's SLIRP ones (gateway 10.0.2.2, guest
10.0.2.15), same as `amd64/net-setup.sh` uses on the Ryzen host, so a guest
cannot tell the three hosts apart. Unlike that script this one needs no Docker —
we are root here, so `ip tuntap` and `iptables` are used directly.

Two things that cost time and are worth knowing:

- **`pkill -f <pattern>` over ssh kills your own session** when the pattern
  appears in the remote shell's argv — which it does, because the argv *is* the
  script you sent. It looks exactly like a silent hang: partial output, no error,
  no exit code. Use a bracket (`akuma-[a]md64`) in every remote `pkill`.
- `mkdisk.sh` builds `tcc` best-effort; without the `tinycc` submodule you get an
  image with no `/bin/tcc`, no `/usr/include` and no static `libc.a`, and the
  script says nothing about it. Copy the submodule if you want parity.

## 3. The blocker, exactly — and how it was cleared

```
$ dmesg | grep -i vmx
[    0.135737] x86/cpu: VMX (outside TXT) disabled by BIOS
[  808.026917] kvm_intel: VMX not enabled (by BIOS) in MSR_IA32_FEAT_CTL on CPU 0
$ modprobe kvm_intel
modprobe: ERROR: could not insert 'kvm_intel': Operation not supported
$ firecracker --no-api --config-file /tmp/fc-probe.json
Kvm error: Error creating KVM object: No such file or directory (os error 2)
```

The i5-4460 has VT-x in silicon. The firmware leaves `MSR_IA32_FEAT_CTL` with
VMX off *and the lock bit set*, which no amount of software can undo — the
register is write-once per power-on. Nothing in Linux, and no Firecracker flag,
can work around it.

**This was fixed on 2026-09-05.** The user enabled VT-x in BIOS setup and
power-cycled; `/dev/kvm` appeared, `kvm_intel` loaded, and the `vmx` flag is now
in `/proc/cpuinfo`. Everything below is kept as the record of what the symptom
looked like and what to do if another machine shows it.

**The fix is one BIOS visit**: power on, `Esc`/`F10` into setup, look under
*System Configuration* (some HP builds: *Security* or *Advanced*) for
**Virtualization Technology (VT-x)**, enable, save, and then **fully power the
machine off and on** — a warm reboot does not always re-latch that MSR.

While in there, three other things are worth writing down because they decide §5:

1. Is there a **CSM / Legacy Boot** option? (decides whether VGA text mode at
   0xB8000 is available as a bare-metal console)
2. Is there any **serial / COM port** option, or an internal COM header on the
   board? (SMBIOS says no, but firmware setup is the authority)
3. Is a **newer BIOS than 80.06 (2015-04-01)** offered by HP for this board? On
   some HP consumer models the VT-x toggle only appears in a later BIOS.

**If there is no VT-x option at all** — a real possibility on an HP consumer
board — then this box can never run Firecracker, and the choice is: keep it as a
TCG build/test box (§4 shows that is genuinely useful), keep the Ryzen at
`192.168.1.126` as the Firecracker host, and treat this machine as the *bare
metal* target instead, which is where it is actually interesting anyway.

## 4. What already works here (measured today, TCG, no KVM)

`INIT=/bin/sshd amd64/run.sh` on the box, guest reachable at `127.0.0.1:2222`:

```
Akuma/amd64 self-test: 173 passed, 0 failed
[SmolNet] DHCP configured / IP: 10.0.2.15/24
clock: synced via SNTP (pool.ntp.org)
[SSHD] Listening on 0.0.0.0:2222...
```

Boot to a listening sshd took well under 30 s *under emulation*; QEMU sat at
~91 % of one core (the netpoll daemon polls — there are still no device
interrupts on this target).

Over that ssh, on this machine:

| command | result |
|---|---|
| `apk --version` | `apk-tools 3.0.8-r0, compiled for x86_64` |
| `apk update` | real TLS to `dl-cdn.alpinelinux.org`, both repos, **28641 distinct packages** |
| `apk add busybox-static` | installed, 1014 KiB |
| `/bin/busybox.static uname -a` | `Akuma akuma 0.1.0-amd64 … x86_64 Linux` — the freshly installed binary runs |
| `apk add curl` | all **14 packages**, 12.7 MiB |

So: **apk works on this box.** That is the checkpoint's claim, reproduced on
hardware it had never run on, with a full toolchain built from scratch on the
machine itself.

**One real gap found.** Every installed file produces

```
WARNING: curl-8.22.0-r0: failed to preserve usr/bin/curl: owner
```

and apk exits with the error count (`15 errors` for curl, `1 error` for
busybox-static) even though every file is installed and executable. Cause:
`chown`/`fchownat` do not exist anywhere in `amd64/` — they take the
unknown-syscall path and return `ENOSYS`. The files land; only the ownership
metadata does not. This is the smallest real bug on the list: ext2 inodes have
`uid`/`gid` fields, so it can be implemented honestly rather than stubbed to
success. Until then `apk`'s exit status is unusable as a success signal on this
target, which is worth knowing before anything scripts it.

## 5. Bare metal — the actual gap

Firecracker hands the kernel a pre-digested machine: PVH entry, an
`hvm_start_info` block, virtio-MMIO devices announced on the command line, no
PCI, no ACPI, no firmware. Bare metal hands it none of that. Per subsystem:

| subsystem | today (FC/QEMU) | this box on metal | size |
|---|---|---|---|
| **Entry** | PVH ELF note; VMM loads the ELF | firmware loads nothing that speaks PVH. Needs **multiboot2** (GRUB) or a **UEFI PE stub** | small–medium |
| **Console** | 16550 at I/O 0x3F8 | **there is no UART on this board** | see below |
| **Memory map** | `hvm_start_info` | multiboot2 mmap tag, or UEFI `GetMemoryMap` | small |
| **Interrupts** | LAPIC timer only; devices are polled | ACPI **MADT** → **IOAPIC** (+ MSI/MSI-X for PCIe) | medium |
| **PCI** | none needed (virtio-MMIO) | full config-space enumeration + BAR programming, ECAM from ACPI **MCFG** | medium |
| **Storage** | virtio-blk | Intel 8-series **AHCI** driver | medium |
| **Network** | virtio-net | **RTL8111/8168** driver — or sidestep it, below | large |
| **SMP** | 1 vCPU | 4 real cores; INIT-SIPI-SIPI bring-up. Not started on this target | medium |
| **Video** | none, deliberately | see console | — |

### The console is the first-order problem

Akuma/amd64 prints to a 16550 and nothing else, by explicit design decision. This
box has no 16550. On bare metal, as things stand today, **the machine would boot
and you would see absolutely nothing** — no output, no way to tell a triple fault
from a working idle loop. Nothing else in the table matters until this is solved.
Four ways out, ranked:

1. **Framebuffer text via GRUB's multiboot2 framebuffer tag.** GRUB sets a linear
   framebuffer (via UEFI GOP or VBE) and hands over its address, pitch and depth;
   the kernel draws glyphs into it with an 8×16 bitmap font. ~200 lines, no PCI,
   no driver for the GTX 745 (you are writing into a framebuffer the firmware
   already set up). **This is the right answer** even though it is a display, and
   it is not the VGA text console that was rejected — it is a pixel buffer, and
   it is the only output this hardware can give a driverless kernel.
2. ~~**VGA text mode at 0xB8000**~~ — **ruled out 2026-09-05.** The box boots
   UEFI, and the user is (reasonably) unwilling to go hunting for a CSM toggle
   on a machine whose current boot works. Without CSM there is no text mode to
   write into, so option 1 is not the best answer, it is the only cheap one.
   Note the tree once had framebuffer code that was deleted — resurrecting it is
   likely cheaper than writing it again.
3. **A PCIe serial card** in the free x1 slot. Real serial, scriptable, ideal —
   but the card's UART is behind a PCI BAR, so it needs PCI enumeration working
   *before* there is any console to debug PCI enumeration with. Chicken-and-egg;
   do it after 1 or 2.
4. **Netconsole** over the NIC — needs a NIC driver first, and gives no output
   before the network is up. Not a bring-up console.

### The NIC has a cheap way out

Writing an RTL8111/8168 driver is the single largest item in the table. The board
has a **free PCIe x1 slot**. An Intel PCIe x1 NIC — an 82574L or 82540/82541-class
card, £10–15 used — is the classic OSDev target: `e1000` is one of the
best-documented drivers in existence, it is a few hundred lines, and it is the
same device QEMU emulates, so it can be developed and tested *under emulation on
the Mac* long before it meets the metal. Buying the card is a straight trade of
money for a large slice of the largest task.

### And a way to skip storage entirely

GRUB can load a **module** alongside the kernel. Point it at the existing 128 MiB
`amd64-root.img` and the whole ext2 root is in RAM at entry, with the multiboot2
info telling you where. `akuma-ext2` reads it through a ramdisk shim of maybe 40
lines. **First bare-metal boot needs no AHCI and no NIC** — just entry, memory
map and a console.

## 5b. Testing the NIC *before* any bare-metal boot

Asked directly: can the networking hardware be exercised before we commit to a
boot? Yes — three levels, and the useful one needs **no BIOS visit at all**,
because VT-d is already enabled (11 IOMMU groups exist with no `intel_iommu=`
on the kernel command line; 6.14 defaults it on) and **the NIC is alone in IOMMU
group 10** — no ACS quirk, no bystander devices, clean isolation.

### Level 0 — what is already known, for free

| fact | value | why it matters |
|---|---|---|
| chip | **RTL8168g/8111g, XID 4c0** | the RTL8168 family has ~40 variants with different init sequences. This names exactly one |
| BAR0 | I/O ports `0xd000`, 256 B | the legacy register window |
| BAR2 | MMIO `0xf7100000`, 4 KiB | the same registers, memory-mapped — use this |
| BAR4 | MMIO `0xf2100000`, 16 KiB, prefetchable | **MSI-X vector table at offset 0, PBA at 0x800** |
| interrupts | MSI-X count **4**; MSI count 1; INTx pin A → **IRQ 18** | three delivery options; INTx via IOAPIC is the simplest first target |
| link | 2.5 GT/s x1, 1 Gbps/full | |
| firmware | Linux loads `rtl_nic/rtl8168g-2.fw` | the chip runs without it (Linux only warns); a first driver can skip it |

All of that came from `lspci -vvv -s 03:00.0`, `dmesg | grep r8169` and
`/sys/bus/pci/devices/0000:03:00.0/{resource,config}` on the running system. The
raw config space is readable as a file right now, so the enumeration code can be
written against a *captured dump* and unit-tested on the Mac before it ever runs.

### Level 1 — a golden register reference, zero risk

`ethtool -d eno1` prints the decoded register block of a **working** chip
(62 lines: MAC, Tx/Rx ring addresses, Command `0x0c` = "Rx on, Tx on", …). Capture
it while Linux drives the card, then compare your own driver's state against it
register by register. Most bring-up bugs on this family are a wrong bit in
`ChipCmd`/`RxConfig`/`TxConfig` or a ring address written before the ring is
aligned, and this dump shows what right looks like on *this* silicon.

### Level 2 — run the real driver against the real chip, from userspace

This is the answer. Bind `03:00.0` to `vfio-pci`, and a normal Linux process can
`mmap` BAR2/BAR4, program the rings, and DMA into buffers the IOMMU maps for it —
**no KVM, no reboot, no kernel**. Crash it and you get a segfault and a core
dump, not a dead machine. This is how userspace NIC drivers (ixy, DPDK) work, and
it needs nothing this box does not already have.

The shape that pays off twice: write the driver as a `no_std` crate over an
MMIO+DMA trait — the same seam `akuma-net-nic` already draws with `FrameArena`
and its "the device owns this buffer until completion" contract — and give it two
backends. A **VFIO backend** for the userspace harness on this box, and a
**kernel backend** (physical addresses, identity map) for Akuma. The same code
gets host tests, a debugger and real packets long before the kernel can boot, and
what the kernel port then adds is only interrupt delivery.

**The catch: `eno1` is this box's only NIC and our only way in.** Binding it to
`vfio-pci` takes the machine off the network mid-session. Do not do it blind.
Options, best first:

1. **A second NIC in the free PCIe x1 slot** — test card and management card are
   then different devices, and nothing about the experiment can lock us out.
2. **A deadman rebind** — `systemd-run --on-active=180` an unbind/rebind script
   before touching anything, so a lockout self-heals in three minutes.
3. **Do it at the physical console** — there is a keyboard and a monitor on it.

### Level 3 — with VT-x on, pass the card into a VM

Once `/dev/kvm` exists, `qemu-system-x86_64 -device vfio-pci,host=03:00.0` hands
the **real** RTL8168 to an Akuma guest: the driver drives actual silicon while
Linux keeps the console and the logs, and a fault kills a VM instead of the
machine. Note this is a **QEMU** capability — Firecracker does not do device
passthrough at all, which is the one thing the metal-bound work cannot borrow
from the Firecracker path.

### One more argument for the Intel card

**Nothing emulates the RTL8168g.** QEMU models `rtl8139` and `e1000`, not this
chip, so a Realtek driver has no dry-run anywhere — every iteration needs this
specific machine. An Intel `e1000`-class card can be developed and tested
**entirely under QEMU on the Mac**, then pointed at the real card through the
same VFIO harness on this box. That turns the largest item in §5 from
"hardware-only, one machine, no emulator" into "emulate first, confirm on metal".

## 6. The order I would do it in

| # | milestone | done when | needs |
|---|---|---|---|
| ~~**B0**~~ | ~~VT-x enabled~~ | **DONE 2026-09-05** — `/dev/kvm` exists, Akuma boots under Firecracker here | one BIOS visit |
| **B1** | multiboot2 entry | GRUB menuentry boots the kernel; it reaches long mode and *halts silently* | multiboot2 header + a second entry path beside the PVH one |
| **B2** | a console | text on the monitor from `kmain` | framebuffer tag — **not** VGA text; the firmware is UEFI |
| **B3** | RAM root | the self-test suite runs on the metal, ext2 from a GRUB module | mmap tag + ramdisk shim |
| **B4** | interrupts | ACPI MADT parsed, IOAPIC routing a timer IRQ | ACPI table walk |
| **B5** | a real device | PCI enumeration + AHCI (disk) or e1000 (net) | B4 |

The NIC half of B5 does not have to wait for B1–B4 at all — §5b runs it in
userspace on Linux, today, in parallel.

B1–B3 are the interesting fraction: **a real, unassisted boot on real hardware**,
with the self-test suite as the proof, and no drivers at all. B4–B5 is where it
stops being a demo. `apk` on the metal needs B5 with a NIC.

## 7. "Resize the machine"

`sda2` is 930 GiB with 807 GiB free, so there is room for anything. But:

**Do not wipe Ubuntu.** It is the build host (17 s kernel builds, natively), it is
the only way into the box, and — with no serial port — it is the *only* recovery
path if Akuma fails to boot. The shape that works is:

- shrink `sda2`, add a small partition (8–16 GiB is generous) for Akuma's root;
- keep the existing GRUB and add a menuentry for Akuma — Secure Boot is already
  disabled, the ESP is already there, and GRUB is already the boot manager;
- pick Akuma from the GRUB menu when you want it, Ubuntu the rest of the time.

That gives a dual-boot bare-metal target with a one-keystroke way back to a
working machine, and it costs nothing but a partition.

## 8. Recommendation

- **Now:** flip VT-x on the next physical visit and answer the three questions in
  §3. That single action unlocks Firecracker at full speed on this box and tells
  us which console path §5 takes.
- **Meanwhile:** the box is already a useful Akuma test host under TCG — it built
  its own kernel and ran a real `apk add` today. `chown`/`fchownat` (§4) is a
  small, well-defined fix with a clear test (`apk add curl` exits 0).
- **Then:** B1–B3. Bare metal on this machine is genuinely reachable, and it does
  not need a single device driver to be worth doing.
- **Budget item:** an Intel PCIe x1 NIC for the free slot, whenever bare-metal
  networking becomes the goal. It is the cheapest large win available.

---

**Background.** The port itself: `docs/archive/AKUMA_FIRECRACKER_AMD64.md`
(stage-by-stage) and `amd64/README.md`. The aarch64 Firecracker port, a different
machine and device model: `docs/archive/AKUMA_FIRECRACKER_KVM.md`. Platform
dependency and what the arch split needs:
`proposals/REDUCING_PLATFORM_DEPENDENCY.md` §0.

---

# Update — 2026-09-05, later the same day

## VT-x on, and Akuma runs here under Firecracker

The BIOS toggle was made and the machine cold-booted. `/dev/kvm` exists,
`kvm_intel` is loaded, and with the staging from §2 already in place the VM came
up on the first try:

```
Akuma/amd64 self-test: 173 passed, 0 failed
[SmolNet] DHCP configured / IP: 10.0.2.15/24
clock: synced via SNTP (pool.ntp.org)
[SSHD] Listening on 0.0.0.0:2222...
```

Config at `/root/akuma/fc-akuma.json`, log at `/root/akuma/fc-boot.log`, one
vCPU, 2048 MiB, virtio-blk on the 128 MiB ext2 image and virtio-net on `tap0`.

## apk, on this hardware, under Firecracker

| command | result |
|---|---|
| `apk update` | exit 0 — real TLS to the Alpine CDN, **28641 distinct packages** |
| `apk add busybox-static` | installed; `/bin/busybox.static uname -a` runs |
| `apk add curl` | 14 packages, 12.7 MiB |
| `apk add tzdata` | installed; 16 packages now on the image |

**Two real kernel gaps, both small and both in `amd64/`** (left for whoever owns
that tree — it is under active work):

1. **`chown`/`fchownat` do not exist**, so every installed file produces
   `failed to preserve <path>: owner` and `apk` exits with the error count. The
   files are installed and executable; only the ownership metadata is missing.
   ext2 inodes have `uid`/`gid` fields, so this can be implemented honestly
   rather than stubbed to success.
2. **`symlink`/`symlinkat` do not exist** — found by `apk add tzdata`:
   `failed to extract usr/share/zoneinfo/posixrules: Function not implemented`.
   That file is a symlink, and it is the first package tried that contains one.

Until those land, `apk`'s **exit status is not a success signal** on this target.
Anything scripting it must look at the output.

## Two host-setup facts worth keeping

* **The guest's DNS resolver address is hardcoded to `10.0.2.3`**
  (`amd64/src/dns.rs`), QEMU SLIRP's proxy address. A dnsmasq answering only on
  the gateway `10.0.2.2` gives a guest that gets a DHCP lease, reaches the
  network, and cannot resolve anything — which shows up as
  `clock: could not resolve pool.ntp.org` and then, much less obviously, as
  `apk` failing TLS on certificates that are not yet valid because there is no
  wall clock. `akuma-tap-up.sh` now carries **both** `10.0.2.2` and `10.0.2.3`
  on `tap0` and answers on both.
* **dnsmasq must not inherit `/etc/resolv.conf`** on this host: it points at
  systemd-resolved's stub, and forwarding there answered nothing. It now runs
  `--no-resolv --server=192.168.1.1 --server=1.1.1.1`.

## Can the RTL8168g be passed to Firecracker?

**No, and not as a version gap.** Firecracker's PCI support (`--enable-pci`,
v1.13.0) is a *transport for its own virtio devices*; the changelog contains no
`vfio` and no `passthrough`. Fly.io hit exactly this and brought in Cloud
Hypervisor for GPU passthrough machines.

| VMM | passthrough | boots Akuma's PVH ELF | needs KVM |
|---|---|---|---|
| Firecracker | **no** | yes (in use here) | yes |
| Cloud Hypervisor | **yes**, VFIO | yes — same PVH entry | yes |
| QEMU + `vfio-pci` | yes | yes | yes |
| **Userspace VFIO, no VM** | n/a — the driver *is* the process | n/a | **no** |

Cloud Hypervisor is the "Firecracker but with the real chip" answer: same
rust-vmm lineage, same PVH boot path Akuma already produces.

## The NIC driver exists now: `crates/akuma-net-rtl8169`

Written against the live chip rather than from anyone's driver source. The
register map was read back off real silicon through a read-only BAR2 mapping
while Linux had the device up, and `tests/golden_registers.rs` keeps that
256-byte dump as a fixture the map is asserted against.

**64 tests, 0 clippy warnings, builds for `x86_64-unknown-none`.** The driver is
pure logic over two consumer-implemented traits (`Regs`, `Rings`), so
`#![forbid(unsafe_code)]` holds and the whole bring-up runs against
`model::FakeChip` — a simulated chip that implements the reset delay, the
asymmetric MDIO busy protocol and the transmit doorbell, records write order so
ordering constraints can be asserted, and **panics when the driver touches a
descriptor or buffer the chip owns**.

The fixture has already paid for itself: the crate first used the all-ones "no
threshold" encoding for the receive FIFO threshold, and the live chip runs
`0b110`. The measured value won.

**Next, and it needs no BIOS visit and no VM**: a `vfio-pci` harness in userspace
implementing `Regs` over an mmap'd BAR and `Rings` over IOMMU-mapped buffers,
running this crate's own `init`/`transmit`/`receive` against the real chip with a
debugger attached. VT-d is on and the NIC is alone in IOMMU group 10, so nothing
stands in the way except the one caution in §5b: **that NIC is the host's only
link and its only way in.** Second NIC, timed rebind, or a hand on the keyboard.

## What changed in the plan

B0 is done. The bare-metal console question is settled by the firmware being
UEFI: it is the GOP/multiboot2 framebuffer, and the tree apparently had
framebuffer code once that was deleted, which is the cheapest place to start.
The NIC — the largest item in §5 — now has a driver with a test suite, ahead of
the boot path that would need it.

---

# Update — 2026-09-05, bare metal

**It boots.** Milestones B1, B2 and most of what B3 was for, in one evening.
UEFI firmware -> GRUB 2.12 -> multiboot2 -> Akuma, drawing to the television.
The same binary still boots under Firecracker via PVH at 196/196 self-tests, so
this cost the VMM path nothing.

What the machine reported about itself, on its own screen:

| | |
|---|---|
| loader | GRUB 2.12-5ubuntu11, loaded at `0x200018`, ACPI RSDP present |
| framebuffer | **1920x1080 @ 32bpp, pitch 8192, at `0xe0000000`** |
| channels | r 8@16, g 8@8, b 8@0 |
| console | 110x61 characters at scale 2 |
| memory | **24 regions, 16321 MiB usable** |

`0xe0000000` is the GTX 745's BAR, 3.5 GiB up — which is exactly why the
identity map had to grow from 1 GiB to 4, and the one change without which
nothing above could have been printed. And **pitch 8192 against width 1920**:
the rows are 2048 pixels apart, so any code computing the stride as
`width * bpp/8` would have drawn a sheared screen.

## The two bugs, and how they were actually found

Both produced a black screen with no reboot, which is the least informative
failure a machine can have.

1. **`rdmsr` clobbers `%edx`.** The trampoline carried "which loader booted us"
   in `%edx`; enabling long mode reads EFER with `rdmsr`, which returns its
   result in `EDX:EAX`. Every boot therefore fell through to the PVH `kmain`,
   which read GRUB's information block as an `hvm_start_info` and reported the
   confusion over a serial port this board does not have. The marker now lives
   in memory (`__boot_protocol`).
2. **The framebuffer tag's `reserved` field is a `u16`**, so the colour fields
   begin at tag offset **32**, not 31. One byte early gives `blue_size == 0`,
   a format that fails validation, and a kernel whose only console is that
   framebuffer with no way to say so.

Three wrong diagnoses preceded the right one: the parser was blamed for the
first black screen (it was bug 1), then a boot-stack overflow (it was not,
though the stack really was 64 KiB growing into the page tables with no guard
page, and is now 256 KiB). What settled it was **recording `%eax` and `%ebx`
into memory at entry** and reading them back out of the guest: `0x36d76289` and
`0x21000` proved GRUB had entered correctly and the loss happened later.
`__entry_eax` / `__entry_ebx` are kept for exactly that reason — on a board with
no UART, "what did the loader actually hand me" is otherwise unanswerable.

## The rig that ended the reboot cycle

Built on the box and reusable for every step from here:

```bash
grub-mkrescue -o /root/akuma.iso /root/iso        # /boot/akuma/akuma-amd64 + grub.cfg
qemu-system-x86_64 -enable-kvm -m 512 \
  -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,file=/root/vars.fd \
  -cdrom /root/akuma.iso -boot d -display none -vga std -no-reboot \
  -monitor unix:/tmp/mon.sock,server,nowait
```

Then over that monitor socket: `screendump /root/shot.ppm` for a picture, and
`xp/3wx <addr of __entry_eax>` and `xp 0xb8000` (the EGA buffer, which
`kmain_mb2` writes unconditionally) as debug channels. Two minutes per
iteration instead of a walk to another room.

Three traps in the tooling, all self-inflicted and all worth remembering:

- `pkill -x qemu-system-x86_64` **matches nothing**: `comm` truncates to 15
  characters, so the name is `qemu-system-x86`. Five stale VMs accumulated and
  one round of register readings came from the wrong one.
- `pkill -f <pattern>` over ssh kills the shell running your own script when the
  pattern appears in it — which it does, because the script *is* the argv.
- `set -e` plus `pkill` aborts the script whenever nothing matched.

## New crates

| crate | why |
|---|---|
| `akuma-multiboot2` | the information-block parser, 11 tests. The offset bug above is now `the_colour_fields_start_at_tag_offset_32`, which runs in 0.2 s |
| `akuma-fbcon` | the console: an 8x8 font drawn for this, integer scaling chosen from the resolution, overscan margin for televisions, and a character grid in RAM so **video memory is never read** |

## What is next, in the order the machine now argues for

1. **Memory above 4 GiB.** The map shows **12784 MiB at `0x1_0000_0000`** and
   `boot.s` maps only the low 4 GiB. Most of this machine's RAM is currently
   invisible to it.
2. **B3, the RAM root**: the ext2 image as a GRUB module, so the 196 self-tests
   run on the metal with no storage driver at all.
3. **ACPI MADT + IOAPIC**, then PCI enumeration.
4. **The NIC**: `akuma-net-rtl8169` against the real chip — either through the
   kernel once PCI works, or through the userspace VFIO harness (§5b) long
   before that.

Also worth raising: the console grid is 128x64 and 1080p at scale 2 needs 61
rows. A mode with more rows will clip rather than crash, but the margin is
thinner than it looks.

---

# Update — 2026-09-05, the root filesystem and a shell

**Akuma mounts a real ext2 filesystem on bare metal, runs its own test suite
there — 151 passed, 0 failed — loads an ELF off that filesystem, and drops into
`/bin/sh`.** No storage driver is involved: GRUB reads the image off the disk it
already knows how to read and leaves it in RAM (`module2`), and the kernel mounts
it through a block device that is simply memory (`amd64/src/ramdisk.rs`).

The PVH path is unregressed at 196/196, and now gets 2047 MiB of RAM rather than
1 GiB — see bug 5 below.

## Five bugs, each of which only exists on real firmware

None of these can happen under a VMM, which is why none of them had been seen
before. All were found in the QEMU+OVMF+GRUB rig rather than by rebooting.

### 1. `sysretq` is `#UD` without `EFER.SCE`

`usermode::init_syscall()` writes `IA32_STAR`/`LSTAR`/`SFMASK` **and sets
`EFER.SCE`**. The bare-metal entry omitted it, so the first ring-3 entry raised
an invalid-opcode exception one instruction in — at `enter_user_mode+0x2c`,
after every other test had passed. The lesson is not about that MSR: it is that
a second boot path which *replicates* an init sequence will drift from it. The
sequence in `kmain` is the source of truth and `kmain_mb2` says so in a comment,
but the real fix is to share it.

### 2. UEFI's memory map is fragmented, and "the region containing the kernel"
is the wrong question

`mem::init` chose the RAM region containing the kernel image, on the reasoning
that picking any other would hand the PMM frames while the kernel sat somewhere
it had never heard of. That is right on a VMM, which reports two or three big
regions.

**UEFI reports dozens**, carved up by how the firmware itself used memory. On
this machine the region containing the kernel is `0x100000..0x800000` — **seven
megabytes, on a box with sixteen gigabytes** — and a 64 MiB heap does not fit in
it. The boot failed with "heap does not fit in the region holding the kernel".

Two changes followed. `akuma-multiboot2::usable_coalesced` sorts and merges
abutting usable regions before anything looks at them (UEFI splits contiguous
RAM into runs of separate entries), and `mem::init` now takes **whichever usable
region has the most room after everything already in it**. Containment stops
being necessary once the PMM is given a single region: anything outside that
region is never handed out, which is exactly what protects a kernel image or a
loader-placed module living elsewhere.

### 3. A boot loader's modules are not marked used in the memory map

GRUB reports the frames holding the module as ordinary available memory. A
kernel that seeds its allocator from the map alone therefore hands out the pages
containing **its own root filesystem**, and the corruption appears later and
somewhere else, looking like a filesystem bug.

Worse, the heap started at `_kernel_end`, and "just after the kernel" is a
favourite place for a loader to drop a module — so the allocator would have been
placed directly on top of it. `mem::init_reserving` now raises the heap above
anything the caller placed in RAM and reserves it from the PMM.

### 4. Scrolling a 4K framebuffer redraws seven million pixels

Video memory is uncached, so the console keeps its text in RAM and re-draws
rather than reading back (see `akuma-fbcon`'s header). The naive version redrew
every cell on every scroll: at 3840x2160 that is over 13000 cells of 512 pixels
each, nearly seven million uncached writes **per line of output**, and the suite
prints around two hundred lines. The machine would have looked hung for minutes.

`Console::scroll` now compares each cell against what will replace it and draws
only the difference. Console output is short lines on a wide grid, so most cells
are blank both before and after and cost nothing. `tests/render.rs` pins it:
a scroll must cost under an eighth of a full redraw.

### 5. `PHYSMAP_LIMIT` was lying

The constant said 1 GiB, describing a `boot.s` that built one page directory.
`boot.s` had grown to four (bug: the framebuffer is a PCI BAR at `0xE000_0000`),
so the physmap covered 4 GiB and the constant did not. It capped the PMM at 1 GiB
on every target, VMM included. Raised to 4 GiB, which is also what makes the
LAPIC at `0xFEE0_0000` reachable. **The constant and the page tables describe the
same thing and must move together.**

## And one that is not a kernel bug at all

The shell came up and then the screen filled with replacement glyphs. `sh` was
reading a stdin that never stops producing bytes: **an absent x86 I/O port reads
`0xFF`**, so polling a 16550 that is not there yields an endless stream of
`0xFF` rather than "no data". Not a missing keyboard — a *phantom* one. The fix
is to probe for the UART at init (the scratch-register test) and report no data
when it is absent.

**Fixed 2026-09-05:** `serial::init` writes two patterns to the 16550's scratch
register and reads them back; a bus with no chip returns `0xFF` for both, and
from then on `getb`/`has_byte` report no data and `putb` skips the port (the
framebuffer mirror is unaffected). The multiboot2 entry now calls `init` —
it never had, which was harmless only while writes to an absent port were the
only thing at stake. Both paths print `uart: present` or `uart: absent`.

**And the keyboard itself, 2026-09-05, later.** The obvious cheap route — the
i8042 PS/2 controller, on which most PC firmware emulates a USB keyboard for
as long as no OS claims the USB controllers — is closed on this board:
Ubuntu reports `i8042: PNP: No PS/2 controller found`, and the FADT's
`IAPC_BOOT_ARCH` has the "8042 present" bit clear (`0x10`). The keyboard
(a ROCCAT Vulcan, USB) lights up because the *firmware* enumerated it, not
because anything presents it to the kernel. `amd64/src/kbd.rs` is that i8042
driver anyway — polled, set-1 scancodes, shift/ctrl/caps — because QEMU's `pc`
machine and most other firmware do have one, and `amd64/src/input.rs` reads
from whichever of UART and keyboard has a byte. Verified in the OVMF rig by
typing through QEMU's monitor (`sendkey`). On this box the boot prints
`kbd: no i8042` and the answer stays: networking, or a machine with a serial
header, is the way to type at it. For a hands-off run there is now a second
GRUB entry, `init=/bin/busybox initargs=uname,-a` (`/etc/grub.d/46_akuma_uname`).

The real gap remains input. This board's keyboard is USB, there is no HID stack,
and the machine has no serial port — so an interactive shell on the framebuffer
cannot be driven at all. **Networking is the path to a usable shell here**, not a
keyboard driver, which puts PCI enumeration and `akuma-net-rtl8169` on the
critical path for something other than throughput.

---

# Update — 2026-09-05, PCI, reboot, and the USB keyboard parsed

Four pieces, all with host tests, none needing a reboot to verify.

## PCI enumeration exists

`crates/akuma-pci` (pure: the `0xCF8` address word, the type-0 header, BAR
decode + size math, the capability list) + `amd64/src/pci.rs` (the `unsafe`
port I/O, a full 256-bus scan into a fixed registry, BAR mapping into the
device window). Runs on both entries right after the machine description; on the
VMM path it finds nothing, correctly. `pci::report()` prints an `lspci`-shaped
dump every boot, and `pci::smoke_test` is in the suite.

The crate's fixtures are this box's real config space (`00:14.0` xHCI, `00:1a.0`
EHCI, `03:00.0` `10ec:8168`), so the header/BAR/capability parsing was decided
on a laptop. This unblocks both the NIC driver and the USB stack — the shared
prerequisite the earlier entries kept naming.

## The keyboard, parsed

Measured off the running Linux: the ROCCAT Vulcan (`1e7d:3098`) is a
**full-speed HID boot keyboard on EHCI `00:1a.0`**, behind the Intel Integrated
Rate Matching Hub (single TT, hub address 2, port 6). The xHCI's `XUSB2PRM`
(USB-2.0 port routing mask, PCI config `0xD4`) reads **`0x00000000`** — no
USB-2.0 port on this board can be routed to xHCI. So the keyboard cannot be
moved off EHCI, and the driver is an EHCI driver doing transaction-translator
split transactions.

`crates/akuma-usb` is the host-tested parsing half of that driver:

* `descriptor` — USB standard descriptors + `find_boot_keyboard`
* `hid` — the HID report-descriptor item parser, `is_boot_keyboard_report_descriptor`,
  and `BootKeyboardDecoder` (8-byte boot report → ASCII on the key-down edge)
* `keymap` — HID usage → ASCII, matching `amd64/src/kbd.rs`'s scancode-table
  choices (Ctrl-C → `0x03`, Caps Lock after Shift)
* `ehci` — the capability/operational register layout, the `USBLEGSUP` BIOS→OS
  handoff, the `PORTSC` decode, and the split-transaction queue-head + qTD
  dword builders — with the ROCCAT's queue head hand-computed in a test

25 tests against fixtures from the box (`lsusb -v`, `usbhid-dump`, a read-only
mmap of the EHCI BAR). What is *not* done: the EHCI controller bring-up itself —
MMIO, DMA rings, frame-list threading, port reset, `SET_PROTOCOL(Boot)`, event
polling. That is the next step and it needs the box to iterate on.

## `busybox ifconfig` works

`amd64/src/fd.rs` now answers the read-only `SIOCGIF*` ioctls and serves a
generated `/proc/net/dev`, through the new `crates/akuma-syscalls-net` — the
`struct ifreq` marshalling, the 40-byte `SIOCGIFCONF` stride, the
netmask/broadcast math and the `/proc/net/dev` column format, extracted out of
the aarch64 kernel's `akuma-syscalls-glue` (which does not build for
`x86_64-unknown-none`) so both kernels share one layout. Read-only — no
`SIOCSIF*`, no netlink.

## `reboot(2)`

x86_64 169, wired through `akuma-boot::decode` (the ABI decode shared with
aarch64's `sc-reboot`, host-tested against the values musl sends) +
`amd64/src/reboot.rs` for the x86 reset: `0xCF9` reset-control register, then an
i8042 pulse, then a triple fault. `busybox reboot`/`halt`/`poweroff` all land
here; `poweroff` halts (no ACPI PM block). `akuma-boot` grew a `psci` feature
(default on) so the amd64 port can reach `decode` without the AArch64
`smc`/`hvc` asm. Decode + the `EINVAL` path are self-tested; the reset itself is
a by-hand check on the box.

## Verified on the box, and staged

2026-09-05, on `vaporwave`, all three environments:

| environment | result |
|---|---|
| QEMU `microvm` (PVH) | **212/212**; `busybox ifconfig` prints `eth0` (DHCP addr) + `lo` |
| Firecracker (PVH) | **212/212**; no config-port noise |
| OVMF + KVM + GRUB (bare-metal path, q35) | **178/178**; `pci: 6 function(s)` with BARs (host bridge, VGA, e1000, ISA bridge, AHCI, SMBus); framebuffer console clean; `busybox uname -a` prints |

Two bugs fixed in the process: `pci::scan()` must run on the multiboot2 path
**only** (Firecracker returns garbage for `0xCF8`/`0xCFC`, inventing 48 fake
devices and flooding its log — it does not emulate those ports); and a
self-test's `/proc/net/dev` read buffer was smaller than the file's header.

**Staged**: `/boot/akuma/akuma-amd64` + `root.img` replaced (old kept as
`.bak-20260905-182952`), `grub-reboot` armed for `Akuma/amd64 (busybox uname
-a)` — next boot runs Akuma once (self-tests + PCI dump + `uname -a`), the boot
after returns to Ubuntu.

**`busybox ifconfig` does not work on the bare-metal path yet** — it dies at
`socket(AF_INET)` because `kmain_mb2` runs no `net::init` (no NIC, and
`smoltcp_net::init` currently requires a virtio-net device). So the hands-off
GRUB entry stays `uname -a`. The earlier "`uname` showed nothing" was a stale
kernel/disk on the box, not a code bug — it prints correctly now.

---

# Update — 2026-09-05, loopback + the RTL8169 wired

The socket layer now comes up on the bare-metal path, and the Realtek NIC has a
driver behind it.

## The seam: `ExternalDevice`

`akuma-net-nic`'s `LoopbackAwareDevice` hard-wired a `VirtioSmoltcpDevice`. It
now wraps an **`ExternalDevice` enum** — `Virtio` / `Rtl8169` / `Absent`. The
virtio path is byte-identical (one added `match` arm, no allocation); the new
variants are additive. `smoltcp_net` grew `init_loopback_only()` and
`init_with_external()` beside the virtio-probing `init()`.

`interface_snapshot()` was also corrected: with only a loopback address
configured it now reports `0.0.0.0` for `eth0` rather than the `10.0.2.15`
static-fallback constant, which was a placeholder guess on a machine that has
no such address.

## Loopback only — `busybox ifconfig` works on bare metal

`net::init_bare_metal()` (multiboot2 path) walks PCI for an Ethernet controller.
No NIC, or an unsupported one (QEMU's e1000 in the OVMF rig) → `Absent`:
`socket(AF_INET)` works, `127.0.0.1` is reachable, `ifconfig` shows `lo` + an
unaddressed `eth0`. Verified in the OVMF/q35/GRUB rig: **186/186** self-tests
(the extra 8 are `sock::smoke_test`, now run on this path), and `busybox
ifconfig` prints both interfaces.

## The RTL8169 driver

`akuma-net-rtl8169` (the pure, host-tested driver) + `akuma-net-nic`'s new
`rtl8169` module (the `unsafe` half: `MmioReg` over the mapped BAR, two
descriptor rings + frame buffers in `.bss` translated through
`virt_to_phys`, `OWN` written last behind a compiler fence). `net::init_bare_metal`
maps the Realtek's BAR2 and calls it; on failure it falls back to loopback-only.
DHCP on.

**Not yet verified on the real chip** — nothing emulates the RTL8168g, so this
needs the box: a reboot, and `ethtool -d eno1` on the Linux side as the golden
register reference to compare against (§5b). The wiring builds, both kernels are
clippy-clean, and the loopback half is confirmed in the rig.

---

# Update — 2026-09-05, a real LAN address and a way in

**The RTL8169 driver came up on the real chip.** The first bare-metal boot with
it wired read its MAC out of the part (`60:02:92:61:4e:73`), brought `eth0`
`UP BROADCAST RUNNING MULTICAST`, and `busybox ifconfig` printed it beside `lo`.
That is the driver working on silicon nothing emulates.

It also printed **`inet addr:10.0.2.15`** — a QEMU user-mode networking address,
on a `192.168.1.0/24` household LAN. DHCP had not answered (or not yet), and the
static fallback the stack reverts to was a literal spelled out in three places
inside `akuma-net`, put there when every target was a VMM guest. On this machine
it is unroutable, and the machine has **no keyboard** (USB HID, no stack) to fix
it from. Everything below follows from that.

## `StaticIpv4` — the fallback is now a decision, not a literal

`10.0.2.15/24`, `10.0.2.2` and `10.0.2.3` appeared in `smoltcp_net::init`'s
bring-up, `poll`'s DHCP-deconfigure path and `iface`'s couldn't-take-the-lock
answer. They are one `StaticIpv4 { addr, prefix_len, gateway, dns }` now, chosen
by the kernel at bring-up and stored in four atomics — **not** passed around and
**not** behind a lock, because the deconfigure path runs inside the `NETWORK`
critical section and `interface_snapshot`'s fallback is the answer for having
*failed* to take that very lock. `StaticIpv4::QEMU_USER` is the old triple and
stays the default; `init_with_external` takes an `Option<StaticIpv4>`.

amd64 bare metal picks `192.168.1.220/24`, gateway `192.168.1.1`, resolver
**`1.1.1.1`** (`BARE_METAL_STATIC_V4`). The resolver is deliberately not the
gateway: a household router may or may not run one, and this is the box with no
keyboard. `ip=<addr>[/<prefix>][,<gateway>[,<dns>]]` on the kernel command line
moves it for one boot. DHCP still runs and still wins when it answers.

`/etc/resolv.conf` on the image lists `10.0.2.3`, `1.1.1.1`, `8.8.8.8` —
**musl queries every nameserver in parallel** (`__res_msend`), so one image
serves both worlds at no cost to either.

## `init=/bin/herd` — sshd, supervised

`herd` and `akuma-cli` join `sshd`/`paws`/`httpd`/`tcc`/`apk`/`busybox` on the
image; `/etc/herd/enabled/sshd.conf` is enabled and `httpd.conf` is available.
GRUB entry **"Akuma/amd64 (herd + sshd)"** (`/etc/grub.d/47_akuma_herd`) boots
it. ssh **is** the console on this machine, which is exactly why the supervisor
earns its process: sshd exiting would otherwise end the only way in.

`akuma-cli` is the sibling repo's katakana screensaver — a **std** binary
(clap/crossterm/rand) linked static against musl for
`x86_64-unknown-linux-musl`, not built by `mkdisk.sh` (macOS cannot cross-link
musl) but staged from
`target/x86_64-unknown-none/release/akuma-cli-x86_64`. It runs: `akuma --help`
parsed and printed over an ssh session into the kernel.

## The bug that stood between herd and all of that

`herd` died on its first `fstat` with `#PF ... cr2=0x0` at a
`movb (%r14), %bl` — a null dereference through a register the compiler had every
right to assume survived a call. It was not a scheduler race (deterministic at
`SMP=1`) and not a kernel register clobber. **`libakuma::Stat` was
`asm-generic`'s `struct stat` — AArch64's — on every target.** On x86_64 that is
wrong twice:

- **128 bytes vs 144.** The caller reserved 128 on its stack; `sys_fstat`
  wrote all 144, over the two saved callee-saved registers above the frame.
  Bytes 128..144 are x86_64's `__unused[3]`, always zero — so `%rbx` and `%r14`
  came back as **zero**. Nothing about the crash pointed at `fstat`.
- **`st_mode` at 16 vs 24.** `S_ISDIR` read the wrong word.

`Stat` is `#[cfg]`'d per architecture now, with a `size_of` assertion on each,
and the field *names* are unchanged so every consumer compiles as before.
`userspace/tcc/src/amd64_shim.rs` had already hit this and carried a private
copy; its comment is the warning that was there all along.

Also fixed: the amd64 kernel had no `uptime` syscall (Akuma-private 319), which
herd's entire supervision loop is keyed on, and `paws`'s `uname -a` reported
`aarch64` from a hardcoded string — the first thing anyone types after logging in.

## Verified

| env | result |
|---|---|
| QEMU microvm (PVH, virtio) | **219/219** (212 + 7 new `ip=` parser checks) |
| OVMF+KVM+GRUB q35 (bare-metal path) | **197/197**; `ip=` checks green; e1000 → loopback |
| ssh into the guest | `uname -a`, `ls /bin`, `akuma --help` over `herd`-supervised sshd |

The remaining unknown is the one nothing can emulate: whether **DHCP or a real
TCP session works over the RTL8169 on the actual chip.** Staged and
`grub-reboot`-armed for that.

`ssh -i <tree>/target/x86_64-unknown-none/release/amd64-ssh-test-key -p 2222
root@192.168.1.220` — note **2222**, sshd's default port on every Akuma target.

## The session shell, and the one thing it cannot do

`sshd`'s session shell is **busybox `sh`**, not `paws`, since 2026-09-05. The
caveat that made `paws` the default — "an interactive busybox needs `fork`" —
stopped being true when this target grew a real `fork`/`execve`. Verified over
ssh: `uname -a`, `id`, `df`, `grep`, `wc`, `ifconfig`, `ls`, `apk --version`.
The applet set is ~70 names now, including `reboot`/`halt`/`poweroff` — on a box
with no keyboard, `reboot(2)` over ssh is the only clean way to put it down.

**Pipelines and redirects do not work.** `cmd | cmd` fails with `can't create
pipe: Bad file descriptor`. Adding `pipe2` would not fix it: fds 0/1/2 are not
entries in `amd64/src/fd.rs`'s table (`FIRST_FILE_FD = 3`; the console is
handled by number, below the table), so `dup2(pipefd, 1)` has nowhere to land.
Making a pipeline work means giving the table real entries for 0/1/2 and
routing the console through one — a restructure of that file, not a syscall to
add. It is the next thing worth doing for this machine.

`apk` and its whole support tree ship on the image (`/etc/apk/{repositories,
arch,world,keys}` with all five signing keys, `/lib/apk/db`, `/var/cache/apk`),
as do the HTTPS roots: `/etc/ssl/cert.pem` and
`/etc/ssl/certs/ca-certificates.crt`, 121 certificates.


---

# Update — 2026-09-05 (later), the diagnostic that could not fail

The first `grub-reboot` to the herd entry booted **the wrong entry** — the older
`busybox ifconfig` one — and the machine answered nothing on the network. Two
separate causes, both worth writing down, and neither of them the NIC.

## `busybox ifconfig` was never a network diagnostic

`akuma_syscalls_net::write_proc_net_dev` writes **literal zeros** for every
counter, and busybox `ifconfig` reads them from `/proc/net/dev`. So
`RX packets:0 TX packets:0` on the screen was consistent with a NIC moving
nothing *and* with one moving thousands of frames a second. Verified rather than
assumed: an ssh session that was demonstrably passing traffic printed the same
zeros. A diagnostic that reads the same whatever happens is worse than none, and
this one had been the only network output the bare-metal boot produced.

`netprobe` replaces it — a kernel-side daemon (`amd64/src/net.rs`) printing every
two seconds:

```
[probe] t=12s link=up/1000M/full ip=192.168.1.220/24 dhcp=pending | rx=418 posted=419 rxfail=0 | tx=96 drop=0 | irq=0 polls=111
```

Every number is real and reads through no lock:

- `link` — the PHY, sampled by the Realtek glue every 1024 receive laps
  (`akuma_net_nic::link_state`). This is the field that separates "the cable is
  not carrying" from "the driver is not receiving", which are indistinguishable
  from `ifconfig` and have completely different fixes.
- `rx` — frames actually taken off the ring. **The Realtek path bumped no
  counter at all before this**; `rx_counters()` read a flat zero on the only
  target that has the chip. On any real LAN this climbs within seconds from
  broadcast traffic alone, so a zero beside a live link is a receive-path bug
  and nothing else.
- `tx` / `drop` — a new `TX_FRAMES_SENT` beside the existing drop count, on both
  the Realtek and virtio paths. A drop count alone cannot tell "nothing was
  sent" from "nothing was asked to be sent".
- `dhcp` — `off` / `pending` / `leased`. `is_dhcp_configured()` returns `true`
  when DHCP is *disabled*, which is right for its callers and a lie in a
  diagnostic, so `is_dhcp_enabled()` now exists beside it.
- `polls` — `smoltcp_net::poll()` laps: proof the stack is still being driven.

Enable with `netprobe` on the kernel command line. Wired on **both** entry paths
(PVH and multiboot2) deliberately: a diagnostic whose first run is on the metal
is a diagnostic nobody has tested.

## `cycle_forever` took the machine off the network

`init=/bin/busybox initargs=ifconfig` prints and exits in milliseconds. Then
`run_init` returns and `kmain_mb2` called `cycle_forever()`, which abandons the
BKL and loops drawing a colour band **without ever yielding**. The netpoll
daemon therefore stopped the instant `init` exited: no ARP replies, no ICMP, no
listening socket — while the band cheerfully kept cycling to say the CPU was
fine. That is the entire reason `192.168.1.220` was unreachable and its ARP
entry read `(incomplete)`.

`cycle_forever(keep_scheduling)` now yields when there is a stack behind it, and
only abandons the BKL when there is not (abandoning it and then yielding into
tasks that take it is a contradiction).

**That Akuma answers ARP and ICMP at all was verified, not assumed** — QEMU's
`-netdev user` cannot be pinged from the host at all, so `/root/taprun.sh` puts
the guest on a real tap on a real L2 segment. `4 packets transmitted, 4
received, 0% packet loss`, neighbour `REACHABLE`, `PORT_2222_OPEN`.

## One GRUB entry, and `init=/bin/sshd`

There were three Akuma entries. `grub-reboot` set `next_entry`, the boot
consumed it, and the wrong one came up anyway — an hour of reasoning about GRUB
instead of about the machine. `/etc/grub.d/45_akuma` is now the only one, so a
one-shot can resolve to it or to Ubuntu and the screen says which.

Its command line is `init=/bin/sshd netprobe`, **not** `init=/bin/herd`: herd
drains a service's stdout into `/var/log/herd/<svc>.log`, so on a machine whose
only console is a framebuffer a supervised sshd fails *invisibly*. Running it
directly puts its own startup output on the screen. Switch to herd once sshd is
known good on this hardware and restart-on-death is worth more than visibility.

## The console says what font it is in

`Console::choose_font` decides at runtime from the framebuffer size — IBM Plex
Mono whenever it reaches 80x24, Spleen when it cannot — so "what am I looking
at" was answerable only by re-deriving the arithmetic from whatever mode GRUB
picked. The boot now prints it:

```
  font: Spleen 8x16 scale 1 -> 91x34 cells        (the 800x600 OVMF rig)
```

On the HP box, both 3840x2160 and 1920x1080 give IBM Plex Mono at 146x41 cells.
(The default font was JetBrains Mono until 2026-09-06; see
[`AKUMA_SELF_HEALING_PORT.md`](AKUMA_SELF_HEALING_PORT.md).)

## Still open

The RTL8168g **has not moved a frame yet as far as anyone has measured** — the
two boots that reached the metal used the entry that exits immediately. The
probe is what answers it: link state and a climbing (or flat) `rx=` on the next
boot decide between a carrier problem, a receive-path bug, and a working NIC.

Also noted, not fixed: `US_PER_TICK` assumes 10 ms per LAPIC tick, and under KVM
the probe's `t=` runs roughly 6x fast. It is monotonic, and everything that uses
it for timeouts works, but it is not seconds.


---

# Update — 2026-09-05 (later still), the clock

**The RTL8169 works on real silicon.** The probe's first line off the metal:

```
[probe] t=0s link=up/1000M/full ip=192.168.1.220/24 dhcp=pending | rx=16 posted=0 rxfail=0 | tx=1 drop=0 | irq=0 polls=1
```

`link=up/1000M/full` is the PHY on the actual RTL8168g, negotiated gigabit full
duplex, and `rx=16` are frames off a real LAN. ARP resolved from another machine
(`192.168.1.220 at 60:2:92:61:4e:73`), ICMP answered, and an ssh session
completed a TCP handshake. Receive and transmit both work on hardware nothing
emulates. That closes the question the reboot cycle existed for.

What did not work: the session **connected and then stalled**, and the machine
went unreachable afterwards. That turned out to be the clock, and finding it
took two instruments that did not exist yet.

## The clock was never calibrated, and then was not running

`start_timer` loaded a flat `100_000` at divide-16, with a doc saying the value
was "deliberately uncalibrated ... nothing here needs wall time yet". That
stopped being true the moment `net::uptime_us` began returning
`ticks() * 10_000` and handing it to smoltcp, which measures **every** DHCP
retransmit, TCP retransmit and connect timeout against it. The LAPIC counts at
the core crystal, so the same constant meant ~1.6 ms on a KVM guest (1 GHz APIC)
and ~16 ms on this machine's 100 MHz bus: one clock ran 6x fast, the other 1.6x
slow, and both called it 10 ms.

Worse, on the bare-metal path the timer was **delivering nothing at all**. Every
self-test that enables interrupts also disables them on the way out —
`lapic::smoke_test`, `sched::smoke_test` and `usermode::preempt_test` all end in
`cli`, which is right for a test and left the kernel handing the machine to
`init` with `IF` clear. `TICKS` froze, `uptime_us` froze, and with it every
smoltcp timer: DHCP sent one DISCOVER and never retried (`tx=1`), a TCP
connection finished its handshake and never retransmitted again. From the far
end that is exactly "connects once, then dies".

The PVH path had been getting away with it **by accident**: `clock::sync_via_
sntp` does its own `sti` and never puts it back. The bare-metal path calls no
SNTP, so nothing ever re-enabled them.

Fixes, in `amd64/src/lapic.rs`:

- `calibrate()` measures the LAPIC against PIT channel 2 (the only channel with
  a software gate and a pollable output, so no interrupt and no IDT entry) and
  sets the initial count so one tick really is `US_PER_TICK_TARGET`. On the OVMF
  rig: **626 088 counts per 10 ms, a 1001 MHz APIC** — against a hardcoded
  100 000, i.e. the old clock was 6.26x fast, as predicted.
- `clock_rate_check()` runs **inside the self-test suite, before `run_init`**,
  enables interrupts for the rest of the boot, and counts real timer interrupts
  across a PIT-measured 50 ms. It asserts the rate, not merely that ticks
  arrive.

## `inb` on an absent port returns `0xFF`, not zero

The rate check earned itself immediately. On QEMU `microvm` — which has no PIT —
port `0x61` floats high, so the channel-2 output bit read as *already expired*,
the wait loop fell through on its first iteration, and "calibration" measured the
handful of cycles between two instructions: **1199 counts where the real answer
was 626 088**. It passed a plausibility band, set `CALIBRATED`, and left
`uptime_us` running about fifty times fast with nothing downstream doubting it.

`pit_present()` now probes the port instead of trusting it: clear the gate bit,
read it back, and conclude "no PIT" if it comes back set. `calibrate` also
rejects a channel whose output is high immediately after gating, since a freshly
gated mode-0 count must hold its output low. A machine with no PIT keeps the old
uncalibrated count and **says so** on the console and in the suite.

## Two instruments, and what they cost to get wrong

`netprobe` (`amd64/src/net.rs`) prints a live line from **inside the netpoll
daemon**, not beside it. A probe in its own task can go quiet for two unrelated
reasons — its own starvation or netpoll's — and those are identical in a
photograph. Printing from netpoll collapses that: a line appearing proves the
network is being driven, and no line at all is itself the diagnosis.

Its first version gated only on `uptime_us()` and printed exactly one line on
the metal, which was consistent with a stalled clock *and* a starved daemon and
distinguished neither — the same failure as `busybox ifconfig`'s hardcoded
zeros, committed one function after complaining about it. It now prints on a lap
counter or elapsed time, whichever comes first, and carries `ticks=` and `laps=`
so the three ways it can stop (dead clock, starved scheduler, dead kernel) are
distinguishable. `ticks=15` pinned while `laps=` ran to 78 million is what
identified the masked interrupts.

`cycle_forever(keep_scheduling)` yields when there is a stack behind it. It
used to abandon the BKL and spin, so a boot whose `init` exited took the machine
off the network entirely while the colour band kept cycling to say the CPU was
fine.

## Verified

| env | result |
|---|---|
| QEMU microvm (PVH, no PIT) | calibration correctly **declines**; uncalibrated clock announced |
| OVMF+KVM+GRUB q35 (real PIT) | calibrated 626 088 counts/10 ms; `t=` tracks wall time |
| tap rig, `init=/bin/sshd netprobe` | three back-to-back ssh sessions, still pingable after |
| HP box, bare metal | `link=up/1000M/full`, ARP + ICMP + TCP over the Realtek |

The clock fix has **not** been seen on the metal yet — that is the next boot.


---

# Update — 2026-09-05 (later again), rx stops at exactly 16

The clock fix worked on the metal. From the box's own screen:

```
lapic: the clock advances once interrupts are enabled   [OK]
lapic: ticks per 50ms (expect 5) 5
lapic: the tick rate matches US_PER_TICK_TARGET within 2x   [OK]
Akuma/amd64 self-test: 200 passed, 0 failed
```

and the probe, now that time exists:

```
[probe] t=27s ticks=2799(cal) laps=20086470 link=up/1000M/full ip=192.168.1.220/24 dhcp=pending | rx=16 ... | tx=838975553 ...
[probe] t=61s ticks=6199(cal) laps=40995562 link=up/1000M/full ip=192.168.1.220/24 dhcp=pending | rx=16 ... | tx=838975556 ...
```

`ticks` advances 200 per 2 s — exactly 10 ms a tick, calibrated against the PIT
on real hardware. `tx` climbs by one every couple of seconds: **DHCP is
retrying**, which it could not do while the clock was frozen. The link is up at
gigabit full duplex.

And `rx` is **stuck at 16**, from t=3 s to the end of the boot.

## Sixteen is `RING_LEN`

A receive path that dies on a round number died at a ring boundary. The chip
filled all sixteen receive descriptors, and never took another frame.

It is not a missing re-post: `Nic::receive` already hands each descriptor back
(`rx.post(...)` then `set_rx_desc`) as it consumes it. It is `ISR`.

**`ISR` latches independently of `IMR`.** The mask decides whether a bit raises
an interrupt, not whether the bit is recorded — and `regs.rs` deliberately keeps
`INT_RDU` out of the default mask on the reasoning that a dry receive ring is
not worth an interrupt. That is true of the *interrupt* and false of the *bit*:
`RDU` is a stall, not a status. The chip raises it the moment the ring runs dry
and does not resume receiving until it is written back, and on a purely polled
path nothing had written it back since `init`'s single clear. One ring's worth
of frames, then silence, with transmit entirely unaffected — which is exactly
the shape observed.

`Rtl8169Device::take_rx_frame` now calls `take_interrupts()` every lap (`ISR` is
write-1-to-clear, so reading and writing back is the acknowledgement), and the
probe carries `isr=` (every bit ever seen, OR-ed — the interesting ones are
transient) and `dry=` (how many times `RDU` fired).

## `nosmp`

Added to the multiboot2 command line. The `[BKL] stuck: cpu N waiting on owner
4294967295` chatter from four cores interleaves into every other line on a
console that has to be photographed, and taking the other cores out is the
cheapest way to decide whether a fault is a cross-core one. A bring-up lever,
not a policy — drop it once the network is trusted.

## Unexplained, and worth watching

`posted` and `tx` both read `838975552` on the metal, identically, having been
`0` and `1` at t=3 s. `posted` (`RX_BUFFERS_POSTED`) is bumped **only on the
virtio path** and should never move on this machine at all. Two independent
`.bss` atomics taking the same large value between t=3 s and t=27 s is not a
counter overflowing; it is something writing over them. The Realtek's descriptor
rings and frame buffers are `.bss` statics translated by `virt_to_phys`, so a
mis-programmed ring base or a buffer overrun is the obvious suspect and
`0x3201C040` looks far more like a DMA address than a count. **Not yet
investigated.** If `rx` starts moving after the `ISR` fix and these two still
climb into the hundreds of millions, that is the next thread to pull.


---

# Update — 2026-09-06, ssh works, and the clock was the whole story

**Akuma answers SSH on bare metal.**

```
$ ssh -i target/x86_64-unknown-none/release/amd64-ssh-test-key -p 2222 root@192.168.1.123 "uname -a"
Akuma akuma 0.1.0-amd64 Akuma/amd64 (x86_64 bring-up) x86_64 GNU/Linux
```

DHCP leased `192.168.1.123` — **not** the built-in `192.168.1.220`, which is
only the pre-DHCP fallback and is overridden the moment a lease arrives. Pings
to `.220` therefore kept timing out long after the machine was reachable, which
is worth knowing before spending an evening on it.

## What actually revived the receiver

The stall dump said everything was correct: `rdsar` matched the ring base, all
sixteen descriptors were `OWN`-ed by the chip with `EOR` on the last one alone,
`CR_RE` set, `ISR` clear, and **`MPC` at zero** — not even dropping frames for
want of a descriptor. The chip was not overrun and not confused about its ring;
it was not being handed frames.

`MISC_RXDV_GATED` was **clear**, so the PHY-gate theory was wrong too — the
third wrong answer in a row, after `RDU` and a mis-translated address. What
worked was the rest of `kick_receiver`: drop `CR_RE`, re-state `RDSAR` and
`RCR`, bring `RE` back, reset the driver's cursor. `rx` moved off 16
immediately and DHCP completed seconds later.

That is a **workaround, not a diagnosis.** Why the chip and the driver
desynchronise is still unknown, and so is why completions landed 182 KiB
*below* the ring — walking forward off the end does not reach there. The
recovery is uncapped now (it is what keeps the machine on the network) but only
the first five stalls print, and the running total shows in the probe as
`kicks=`. A `kicks=` that keeps climbing means the box is reachable *despite* a
bug.

Three wrong guesses in a row all came from reasoning about the code. What
settled it was reading the chip's own registers and then changing one thing and
watching.

## The clock, again — and what it was really breaking

`date` said `Thu Jan  1 00:00:00 UTC 1970` on the metal even after
`sync_via_sntp` was wired into the multiboot2 path. The consequence was not
cosmetic:

```
ERROR: Unable to open root: No file descriptors available     <- after the clock was set
WARNING: ... TLS: server certificate not trusted              <- before
```

**At the epoch every TLS certificate on earth is not-yet-valid**, and `apk`
reports that as `server certificate not trusted` — which sends you to inspect
the CA bundle. The bundle was correct and present the whole time (188 900
bytes, 121 certificates). Setting the clock made that error disappear.

The kernel's own SNTP still does not land on this machine (why is open). What
does work is userspace: `clock_settime` (227), `settimeofday` (164) and a
minimal `adjtimex` (159) are wired now, and

```
/ # date -s "2026-09-06 00:30:00"
Sun Sep  6 00:30:00 UTC 2026
```

`busybox ntpd -q -n -d -p <ip>` gets as far as a real reply —
`offset:+1788638218` seconds, exactly 1970→2026 — and then **hangs after
computing it**, without stepping the clock. Since `date -s` proves
`settimeofday` works, the hang is elsewhere; `poll()` with a large timeout is
the first place to look, because ntpd's main loop lives in one and only ever
sent a single query.

## Other things this run established

- **`MAX_OPEN` was 16** and `apk update` exhausted it before reaching the
  network (`Unable to open root: No file descriptors available`). Now 64.
- **`/proc` now exists as an empty directory.** Nothing reads it, but busybox
  `reboot` *opens* it to find init and refuses without it — on a machine whose
  only other reboot is the power button. `reboot -f` skips the check and works,
  which is what makes the edit/reboot/test loop autonomous from a laptop.
  `ps`, `top` and `free` are still broken and need a real procfs.
- **Pipes and redirects still do not work** — `cmd | cmd` gives `can't create
  pipe: Bad file descriptor`, and `pipe2` alone would not fix it: fds 0/1/2 are
  handled by number *below* `fd.rs`'s table (`FIRST_FILE_FD = 3`), so `dup2`
  onto them has nowhere to land. That table needs real entries for 0/1/2.
- **`nslookup` fails** (`write to '10.0.2.3': Bad file descriptor`) while
  `wget http://example.com` resolves and fetches fine — so DNS works and it is
  `nslookup`'s particular socket usage (`write()` on a connected UDP socket)
  that is unimplemented.
- **`hget`** (`userspace/hget`) is the answer to HTTPS rather than a staged
  `curl`: `bootstrap/bin/curl` is aarch64, Alpine's `curl` is dynamically
  linked and this kernel has no `PT_INTERP` support, and busybox `wget https`
  needs a `socketpair` plus an `ssl_client` binary. `libakuma-tls` does TLS
  in-process, is already used by `box pull` and `meow`, and builds for
  `x86_64-unknown-none` unchanged — thirty lines instead of a foreign binary.

## The loop that made this fast

The box reboots itself now. `ssh akuma "reboot -f"` resets it into Ubuntu (the
GRUB one-shot having been consumed), Ubuntu builds and stages and arms, and
`reboot` there brings Akuma back — no hands, no photographs. `~/.ssh/config`
carries an `akuma` alias with `StrictHostKeyChecking no` and
`UserKnownHostsFile /dev/null`, which is not laziness: **sshd generates a new
host key every boot and nothing persists it**, so the fingerprint changes by
construction and a `known_hosts` entry would be wrong rather than reassuring.

One trap that entry creates: plain `ssh root@192.168.1.123` now reaches **Akuma
on 2222**, not Ubuntu on 22. Anything targeting the Ubuntu side has to pass
`-F /dev/null`, or it silently talks to the wrong operating system — which is
how a build once ran `cd /root/akuma` inside a kernel that has no such
directory.
