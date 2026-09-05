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
