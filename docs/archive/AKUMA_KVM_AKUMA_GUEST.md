# Phase 9: Akuma boots under its own hypervisor

**Date:** 2026-10-22. **Status:** **PASS on the ryzen metal** (entry 28, 2026-10-22) and under QEMU TCG:
`[KVM] akuma guest: PASS (banner=yes, init=yes)`. Nested SVM was skipped (the box was
offline for it; the metal run went straight after TCG). Third step of the ladder in [`AKUMA_KVM.md`](AKUMA_KVM.md) ("KVM runs
akuma"); continues [`AKUMA_KVM_LINUX_GUEST.md`](AKUMA_KVM_LINUX_GUEST.md). No Linux
or KVM source was read; every register offset and bit is the architectural xAPIC
map (AMD APM Vol 2, ch. 16), the x86 `CMP`/`TEST` definitions, or the guest's own
trace.

## What runs

`kvmtest kvmakuma` (or `kvmtest kvmlinux` with `/boot/kvm-guest` beginning `akuma`):
`kvm::late` runs `kvm/linux.rs` with `Guest::Akuma`. The guest is **this kernel's own
ELF**, read from `/boot/akuma-guest`, entered the PVH way exactly as Firecracker
enters it, with:

* 768 MiB of RAM (the kernel takes a fixed 512 MiB heap, `mem.rs` `HEAP_SIZE`, out
  of one region and refuses to boot when the region is smaller — first attempt, at
  256 MiB: `[FATAL] heap ends 0x21191000 but the region holding it ends 0xfffd000`);
* the command line `nosmp virtio_mmio.device=4K@0xd0000000:5 init=/bin/busybox
  initargs=sh,/etc/guest.sh` — a virtio-mmio window announced the Firecracker way, no
  ACPI, no PCI, no network;
* the same COM1 / PIT / PIC / CPUID / MSR platform as the Linux guest, **plus a local
  APIC**;
* a 16 MiB ext2 disk (`scripts/kvm/akuma_guest_rootfs.sh`): busybox and a script that
  prints `AKUMA-GUEST-UP`, `uname -a`, the md5 of busybox, writes and `sync`s a 256 KiB
  file, prints its md5 and `AKUMA-GUEST-DONE`. The hypervisor takes that marker as the
  pass condition and stops.

## What had to exist

**A local APIC** (`akuma_svm_amd64::lapic`, 23 host tests). Akuma's `lapic::init` maps
`0xFEE0_0000`, software-enables it, calibrates its timer against the PIT, and from then
on its tick, its idle one-shots (`arm_deadline`, `arm_idle_shot`), and its EOIs are
LAPIC registers. Linux was told `nolapic`; Akuma cannot be. The model is a function of
`now` like the PIT: xAPIC register page (ID, version, TPR/PPR, EOI, SVR, ISR/IRR/TMR,
ESR, the LVT entries, ICR with the fixed self-IPI), a periodic/one-shot timer with the
documented divide encoding, request/in-service state with priority classes, and
**coalescing**: periods the caller was too late to see are one request and the next edge
keeps its original phase. Delivery is the same virtual interrupt as the PIC's
(`V_INTR`, §15.21.4); the first exit after the processor clears `V_IRQ` performs the
acknowledge, so the guest's EOI (an MMIO store, an exit of its own) always finds
the in-service bit set. The page is emulated only after the guest sets the enable bit of
`IA32_APIC_BASE` — a Linux guest that never does is unchanged.

**`CMP`/`TEST` in the MMIO decoder.** The second stop: Akuma's virtio driver died at
`44 3b 88 fc 00 00 00` = `cmp r9d, [rax+0xfc]` ("config generation unchanged?").
LLVM folds a volatile load whose only use is a comparison into the comparison, so a
device access need not be a `MOV`. `mmio` now decodes `38/39/3A/3B`, `80/81/83 /7`,
`84/85`, `F6/F7 /0` — which only *read* the window and write only the arithmetic flags
— and computes the flags (`sub_flags`, `test_flags`). Read-modify-write ALU forms stay
refused. Pinned against two independent oracles: the encodings are **assembled by
clang**, and the flag arithmetic is checked against **1 600 operand pairs executed on
real x86 `cmp`/`test`** (`scripts/kvm/flag_oracle.c`, run under Rosetta on the Mac;
vectors in `crates/akuma-svm-amd64/src/mmio_flag_vectors.in`).

Small things: the guest's `#PF` is not intercepted (Akuma demand-faults all user memory;
nested paging delivers it with no exit), `RAM`, the disk path and the kernel path are
per-guest, and `kvm::late` picks the guest.

## Results

| where | result |
|---|---|
| QEMU TCG, Mac (`scripts/kvm/akuma_rehearse.sh`) | **PASS**: banner, 34 LAPIC timer interrupts delivered, `blk` found from the command line, ext2 mounted, busybox init ran, 10 516 exits (3 453 MMIO, 48 LAPIC page accesses). Guest `/wtest` md5 `71277886…` = host `debugfs` dump of the image; `e2fsck -fn` clean. Same through the `/boot/kvm-guest` selector |
| Linux guest regression (same kernel, `linux_rehearse.sh`) | PASS, `init ran to completion` — unchanged |
| **metal, ryzen entry 28** (`/boot/kvm-guest` = `akuma`, kernel `b3a2c71` built on the ryzen) | **PASS**: the guest calibrated its LAPIC timer against the virtual PIT (`625062 counts per 10000us`) and measured the real TSC (**3794 MHz**), `blk` from the command line, ext2 mounted, `uname` printed, `md5sum /bin/busybox` = `ebce4301…` (the host's), `/wtest` = `71277886…`, `AKUMA-GUEST-DONE`; **9 853 exits, 24 virtual interrupts, 1 665 MMIO, 38 LAPIC page accesses**. Independent check from Pop: `/boot/akuma-guest-root.img` on p3 was rewritten by the guest (mtime = the boot), `debugfs dump /wtest` → md5 `71277886…`, `e2fsck -fn` clean. Log: [`AMD64_KVMTEST_PHASE9_AKUMA_METAL_DMESG_2026_10_22.txt`](AMD64_KVMTEST_PHASE9_AKUMA_METAL_DMESG_2026_10_22.txt) |

## The smallest root: hello world as init

`kvmtest kvmhello` (or `kvmlinux` with `/boot/kvm-guest` = `akuma-hello`) boots the same
guest kernel on a **2 MiB ext2 whose only file is `/bin/hello`** — a 8.9 KB `no_std`
program (`userspace/amd64/helloworld`, built by `scripts/kvm/akuma_hello_rootfs.sh`)
that `write`s one line and `exit_group`s. No busybox, no shell, no script; the command
line is `nosmp virtio_mmio.device=4K@0xd0000000:5 init=/bin/hello`. The hypervisor stops
the run on the line's last words (`run by Akuma`).

| where | result |
|---|---|
| QEMU TCG | **PASS** (flag and selector), 5 269 exits; the busybox guest unchanged |
| **metal, ryzen entry 28** (`/boot/kvm-guest` = `akuma-hello`, kernel `d79495a` built on the ryzen) | **PASS**: `Hello, world! This is the init of a guest of Akuma, run by Akuma`, 6 531 exits, 4 virtual interrupts, 46 MMIO, 18 LAPIC page accesses; guest TSC 3791 MHz. Log: [`AMD64_KVMTEST_PHASE9_HELLO_METAL_DMESG_2026_10_22.txt`](AMD64_KVMTEST_PHASE9_HELLO_METAL_DMESG_2026_10_22.txt) |

## Two virtual processors (phase 11, 2026-10-22)

`kvmtest kvmsmp` (`/boot/kvm-guest` = `akuma-smp`): the busybox guest with two vCPUs, run in
turn on the host core. Rehearsed under QEMU TCG, then **PASSED ON THE METAL** (below).

What it took, in the order it was found:

* **The guest scans, it is not told.** The first run reported `acpi: none found (no RSDP in
  the EBDA or the BIOS window)` although `rsdp_paddr` pointed at a valid table: the Akuma
  kernel ignores that field (`akuma-ryzen-amd64` `acpi.rs`: it is 0 on QEMU and Firecracker).
  The table page now sits at `0xE0000`, the BIOS window's first address.
* **INIT/STARTUP.** `Lapic` returns the ICR write as an `Ipi`; INIT parks and resets the
  target, STARTUP puts it in real mode at `vector << 12` with the architectural reset state
  (`realmode`); the guest's own trampoline at `0x8000` does the rest. `smp:  2 cpus online`.
* **`SWITCH BADFRAME` (a corrupted saved frame) with 500 µs and 5 ms slices**, and
  `[bkls>] spins=` convoys, while the BSP held the BKL and the AP spun: a descheduled lock holder.
  The same kernel under plain QEMU `-smp 2` ran clean, so it was the multiplexing. Intercepting
  `PAUSE` while two processors are runnable ends both. The cause of the corrupted frame was
  **not** pinned down; the working assumption is spin-count logic in the guest, and an
  uninstrumented guest that never executes `PAUSE` could still meet it.
* **`PAUSE` does not advance RIP.** The first version of the exit arm did nothing, so the
  guest re-executed the same `PAUSE` forever (1.5 M exits, no interrupts, console quiet).
  `ExitCode::{Pause,Hlt}`'s doc comment claimed the hardware advances RIP; the `HLT` arm had
  always stepped over it by hand. Corrected.

| where | result |
|---|---|
| QEMU TCG, `SMP=1 scripts/kvm/akuma_rehearse.sh` x7 | **PASS** every time: `guest cpus: 2`, `/wtest` md5 `71277886…` (host `debugfs`), clean `e2fsck`, 2-3 or ~255 IPIs, 45-305 `HLT`s |
| **metal, ryzen entry 28** (`/boot/kvm-guest` = `akuma-smp`, kernel `89cea0b4`) | **PASS** (`banner=yes, init=yes, smp=yes`): `cpu 1 started at 00008000`, `smp:  2 cpus online`, 15 769 exits, 2 IPIs (INIT, STARTUP), 2 557 `PAUSE` yields, 18 `HLT`s, 50 virtual interrupts. Log: [`AMD64_KVMTEST_PHASE11_SMP_METAL_DMESG_2026_10_22.txt`](AMD64_KVMTEST_PHASE11_SMP_METAL_DMESG_2026_10_22.txt). Distinct ASIDs per vCPU therefore work on real silicon. |
| QEMU TCG, one-vCPU Akuma and hello guests | unchanged **PASS** |
| the Linux guests | not rerun (no `vmlinux` off the metal); their path differs only by one-element arrays |

## A NIC (phase 12, 2026-10-22)

`kvmtest kvmnet` (`/boot/kvm-guest` = `akuma-net`): the busybox guest with a **virtio-net**
device in the slot after the disk's (`virtio_mmio.device=4K@0xd0001000:6`), wired to a
gateway that lives inside the hypervisor. **Rehearsed under QEMU TCG only.**

* `akuma-svm-amd64::virtio_net` is the device: virtio-mmio v2, queue 0 = receive, queue 1 =
  transmit, the 12-byte `virtio_net_hdr`, a MAC in config space; `VERSION_1` and `MAC` offered,
  nothing else. 13 host tests, including a loop-in-the-ring and a runaway-index driver.
* `akuma-svm-amd64::gateway` is the backend, QEMU-slirp's addresses on purpose (a guest that
  falls back to `10.0.2.15` agrees with it): ARP for `10.0.2.2`/`.3`, DHCP (DISCOVER -> OFFER,
  REQUEST -> ACK, `10.0.2.15`, router `.2`, resolver `.3`, a day), ICMP echo and UDP echo (port 7)
  to the gateway; everything else is counted and dropped. Replies are built in place into 8 fixed
  slots; checksums are real and the tests verify them by an independent sum.
* The guest polls its NIC, so the interrupt line is announced and never raised; received frames are
  delivered at the top of every loop turn and after each transmit.

Found on the way: **a stepped-over `HLT` left the interrupt shadow set.** `sti; hlt; cli` intercepts
`HLT` inside the one-instruction shadow `STI` opens; the handler advanced RIP but left the bit in the
VMCB, so the `cli` ran shielded, IF dropped, and the timer interrupt that had woken the guest was not
taken. 127 915 of 139 967 exits were `HLT` with 37 interrupts injected. Every guest has done this since
phase 0; it only showed once something kept the guest idle. `Vmcb::complete_instruction` now ends the
shadow wherever the hypervisor completes an instruction.

| where | result |
|---|---|
| QEMU TCG, `NET=1 scripts/kvm/akuma_rehearse.sh` | **PASS** (`net=yes`): 10 frames, `arp=2 dhcp_offer=1 dhcp_ack=1`, the guest's `eth0` reads `inet addr:10.0.2.15  Bcast:10.0.2.255  Mask:255.255.255.0`; `HLT` exits 503 (was 127 915) |
| QEMU TCG, `NETPROBE=1` (`nc -u -w 2 10.0.2.2 7`) | the echo **worked both ways** (guest strace: `nc` read its 16 bytes back from the socket and wrote them out, the shell read them from the pipe); `nc`'s 2 s poll then timed out and the guest **never finished**: no verdict |
| **metal, ryzen entry 28** (`/boot/kvm-guest` = `akuma-net`, kernel `89cea0b4` built on the ryzen), `GUEST=net` | the device, the gateway and the guest's stack are **proven on hardware**: `net_mmio=69 net_frames=11 arp=2 dhcp_offer=1 dhcp_ack=1 udp_echo=1 net_ignored=6 net_lost=0`, `eth0 52:54:00:12:34:56 inet addr:10.0.2.15 Bcast:10.0.2.255 Mask:255.255.255.0`. Verdict **FAIL (init=no)** for the same reason as under TCG: after `nc`'s timeout the guest sits in an idle loop re-arming its LAPIC timer (377 558 timer interrupts, 377 535 `HLT`s, 2.6 M LAPIC MMIO exits in the 20 s before the quiet-console stop) and the init script never reaches its last line. Log: [`AMD64_KVMTEST_PHASE12_NET_METAL_DMESG_2026_10_22.txt`](AMD64_KVMTEST_PHASE12_NET_METAL_DMESG_2026_10_22.txt) |
| **metal, ryzen entry 28, `GUEST=net` (no probe)** | **PASS** (`banner=yes, init=yes, net=yes`): `net_mmio=67 net_frames=10 arp=2 dhcp_offer=1 dhcp_ack=1`, `eth0 inet addr:10.0.2.15`, `AKUMA-GUEST-DONE`, 13 570 exits, 502 `HLT`s, 524 virtual interrupts (no timer storm). Log: [`AMD64_KVMTEST_PHASE12_NET_NOPROBE_METAL_DMESG_2026_10_22.txt`](AMD64_KVMTEST_PHASE12_NET_NOPROBE_METAL_DMESG_2026_10_22.txt) |
| the same image on plain QEMU with `-netdev user` | finishes (`nc` returns after its timeout, `AKUMA-GUEST-DONE`) |
| the one-vCPU, hello and SMP guests after the shadow fix | unchanged **PASS** |

**Item 4 (2026-10-22): FIXED — see UDP_REPLY_WAIT_TIMER_STORM.md. (History of the narrowing follows.)**
* It still reproduces with the current gateway and kernel, and TCP probes do not trigger it
  (`NET=1 TCPPROBE=1`: `tcp echo: akuma-tcp-probe`, finishes).
* **A reply datagram is required.** `NETPROBE_PORT=9` (a UDP port the gateway does not answer, so
  `nc -u -w 2` just times out) completes and passes; port 7 (the gateway echoes) stalls.
* **Not the hypervisor's timers or a lost wakeup.** The same guest on plain QEMU (no SVM) receiving a
  real UDP reply (slirp's DNS at 10.0.2.3:53) finishes normally. And a background watcher in the
  guest (`GUEST_PRE`) shows the system is alive while "stalled": the `nc` process is in state `R`
  and its **`utime` climbs (1002 -> 3004 ticks between samples, `stime` 0)**: `nc` is **spinning in
  user mode with no syscalls** after its poll timeout (a stuck `exit_group` would not show, but
  stime would). The shell, its parent, waits for it.
* **It is a general guest-kernel bug, not a hypervisor one — reproduced on plain QEMU with no SVM.**
  `scripts/kvm/udp_stall_repro.sh LOG` runs a UDP echo on the host (127.0.0.1:7777, which slirp
  shows the guest as 10.0.2.2:7777) and the guest's `nc -u -w 2` against it: **STALL**. A guest
  `strace` of `nc` there reads `poll->1, read(0)->12 (send), poll->2, read(3)->12, read(0)->0
  (stdin EOF), shutdown(3, SHUT_WR)->0, write(1)->12, poll(2000 ms)->0 (the timeout)` and then
  **no further syscall**; QEMU's own monitor goes unresponsive while it spins (the guest is
  hammering something that holds QEMU's lock). slirp's DNS reply (`10.0.2.3:53`, same flow, same
  `shutdown`) does *not* stall, so the trigger is subtler than "a reply arrived": the next place to
  look is `nc`'s exit after `shutdown(SHUT_WR)` on a UDP socket (`close` of a shut-down UDP socket
  with the peer's address `10.0.2.2`), and the guest idle loop's timer re-arming
  (`lapic::arm_deadline` with an already-due deadline: the stalled guest's RIP is in it every time).
* Next (any of): sample the guest RIP while it spins (the hypervisor's stop line prints `rip`) and look at
  what busybox's `nc` does after a UDP reply plus timeout; compare against the byte-exact content
  of the gateway's reply (a malformed datagram that the guest stack accepts but `nc` mishandles is
  the cheapest explanation, since slirp's reply works).

**Open (the guest, not the device):** the stall reproduces on the metal and under TCG with the gateway
answering, and not on QEMU with slirp (which does not answer port 7, so `nc` there never receives a
datagram). `strace` prints a syscall on *return*, so `nc`'s `exit_group` leaves no line; the likeliest
shape is that `nc` exits and the parent's `wait4` is never woken, with the netpoll daemon spinning (the
class of `NETWORK`/`SOCKET_TABLE` ordering bug in
[`BKL_TICKET_LIVELOCK_FIRECRACKER`](BUG_FIX_LIST.md)'s neighbourhood). To find it: `strace_pid=` on the
shell, the guest's own dmesg, and a `NETPROBE=1` run with the gateway's UDP echo disabled (the A/B that
separates "a datagram arrived" from "a timeout elapsed"). Without `NETPROBE` the NIC guest passes
(`GUEST=netprobe` stages it with the probe; `GUEST=net` without, and that one passes on the metal).

`busybox ping` cannot be used as a probe: Akuma has no raw sockets (`can't create raw socket`).

## ssh into the guest (phase 13, 2026-10-22)

`kvmserve` (`/boot/kvm-guest` = `akuma-serve`): the guest runs `userspace/sshd` on its port 22
and its init script never ends; a host port (2223) is forwarded to it. **Rehearsed under TCG
end to end with a real `ssh`** (`scripts/kvm/serve_rehearse.py`): QEMU's `hostfwd` ->
the host Akuma's relay -> the gateway's TCP -> the guest's sshd, running `echo`, `uname -a` and
`cat /proc/cpuinfo` in the guest.

What it took: **TCP in the gateway** (`akuma-svm-amd64::tcp`, ~400 lines, verified against
smoltcp in both roles with 300 KB echoed back byte-exact and 200 KB / 150 KB streamed each way
— `tests/gateway_tcp.rs`); gateway **forward slots** (the gateway opens the connection *to* the
guest); a **relay** on the host's socket API; and the hypervisor as a **background host task**
(the host's network does not exist when `kvm::late` runs, and on the ryzen wifi only joins after
init), which meant pinning a daemon to core 0, re-`VMSAVE`ing the host state each entry,
yielding the BKL, and sleeping with `hlt` when the guest is idle. Found on the way: a host with
no `init` ends in `halt()`, so the task is never scheduled.

**On the metal (ryzen entry 28, `GUEST=serve`, kernel `6a290876`; staged by
`overlays/ryzen/kvm-serve-stage.py`):** `ssh -p 2223 root@<ryzen wifi ip>` from the Mac ran
commands in the guest (`uname -a`, `/proc/cpuinfo`, `eth0 10.0.2.15`), and the ryzen's own Akuma
`ssh -p 2223 root@127.0.0.1` reached it too, with the ryzen's machine key
(`/etc/sshd/id_ed25519`, the identity its client falls back to) added to the guest's
`authorized_keys`. A 1 MiB upload arrived byte-exact (md5) but at ~60 KB/s; a 1 MiB **download**
(guest to Mac) never finished in 10 minutes and held a relay slot. The control that localizes it:
**the host Akuma's own sshd, with no guest in the path, also moves ~55 KB/s outbound over the
ryzen's wifi** (4 MiB in 77 s), while the identical bulk test on the TCG rig (virtio host NIC) does
2 MiB up and down intact in ~1 s. So the throughput ceiling is the ryzen's wifi path, not the
gateway or relay; the hung download is not explained by it and is open. **Closed 2026-10-22 (later): [`KVM_RELAY_SYN_STALL.md`](KVM_RELAY_SYN_STALL.md): the same 1 MiB download completes in 46 s, and the relay itself moves 4-8 MB/s from the host.** Two suspects, both
addressed but unverified on metal: the service task held the BKL through every guest entry
(starving the wifi/network tasks on a 16-core host; now a dropped-BKL window), and the relay had no
visibility (it now logs per-slot ring and window state every 5 s). Re-run with
`--bulk` equivalents once the box is free, and compare a boot with no service guest.

TCP made the guest's `nc` TCP probe pass (`NET=1 TCPPROBE=1`: `tcp echo: akuma-tcp-probe`),
which the UDP probe never did; see the open item below.

## Gentoo as a guest (phase 14, 2026-10-22)

`kvmgentoo` / `/boot/kvm-guest` = `akuma-gentoo`: the service guest with **Gentoo from the ryzen's p7
as its root**. `init=/usr/bin/bash initargs=/akuma-kvm-init.sh` (`overlays/ryzen/gentoo/`): set the
hostname, `ifconfig`, start OpenSSH (p7's own configuration, port 4444); the relay forwards host port
2223 to the guest's 4444 (`Guest::guest_port`). **On the metal:** `ssh -p 2223 root@<ryzen wifi ip>`
lands in `akuma-gentoo` — Gentoo Linux, `/dev/vda` 3.0 G, 766 MiB RAM.

Choices worth keeping:

- **The disk is a RAM copy of an image file** (`HostDisk::Mem`): `/boot/gentoo-guest.img` is read
  once into contiguous frames when the guest starts; the guest cannot touch p7 or p3, and its
  writes are gone when it stops. No second NVMe window was added (the NVMe driver has exactly one
  GPT window, p3's, by design).
- **p7 is never written.** It is `clean with errors` (invalid symlink inodes, mostly under
  `/var/lib/box/layers`, from the old ext2 bitmap bug), so `mke2fs -d` dies with `EUCLEAN`;
  `overlays/ryzen/kvm-gentoo-image.sh` reads it with `tar` (3 complaints) into a staging copy and
  builds the image from that. An `e2fsck -fy -z` repair was deliberately not run.
- The image is 3 GiB for 1.8 GiB of files; the hypervisor copies it in 1 MiB reads (about a minute
  at boot, before the guest is entered).

Also this day: the relay's SYN stall ([`KVM_RELAY_SYN_STALL.md`](KVM_RELAY_SYN_STALL.md)), the
hostname split (`akuma-ryzen` host / `akuma-kvm`, `akuma-gentoo` guests: the host's Gentoo box
re-set the name to `localhost` from its OpenRC `conf.d/hostname`, so the herd oneshot alone was not
enough).

## Still open

* (Fixed 2026-10-22, same day: a guest spinning with `IF` set used to see timer
  interrupts at the 10 ms host-tick rate; the host's one-shot is now armed at the guest's
  deadline — [`AKUMA_KVM_LINUX_GUEST.md`](AKUMA_KVM_LINUX_GUEST.md) § Phase 10.)
* No IOAPIC, no PCI, no route from the gateway to anywhere (no TCP, no DNS answers). SMP (above) is cooperative multiplexing on one host
  core, not parallel: two vCPUs run at most one core's worth. Not yet run on the metal.
* The SMP run on the metal is one data point (a short busybox script); a long multi-process load on both vCPUs has not been run.
* The guest's `HLT` with `IF` clear stops the run (same as the Linux guest).

## Background

[`AKUMA_KVM.md`](AKUMA_KVM.md) · [`AKUMA_KVM_LINUX_GUEST.md`](AKUMA_KVM_LINUX_GUEST.md) ·
`crates/akuma-svm-amd64/src/{lapic,mmio}.rs` · `amd64/src/kvm/linux.rs` ·
[`../runbooks/kvm-linux-guest.md`](../runbooks/kvm-linux-guest.md)
