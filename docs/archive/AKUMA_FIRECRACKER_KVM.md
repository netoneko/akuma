# Akuma on Firecracker: first boot, and the five reset-value assumptions it exposed

**Date:** 2026-08-21
**Result:** Akuma boots on Firecracker v1.16.1 under KVM on an Apple M4 Pro, mounts
its ext2 root (both `disk.img` and the 6 GB devbox image), runs the boot suite at
**290 PASSED / 0 FAILED / 0 POISON**, executes userspace processes, and starts
`/bin/sshd` under herd. Outbound networking works and is well-formed on the wire;
**inbound (RX) does not**, so SSH is not yet reachable (§5.1).

> **Update 2026-08-21 — §5.1 is resolved in code.** The RX blocker was
> root-caused after this document was written: Firecracker withholds every
> inbound frame until the posted receive-descriptor capacity reaches 65562 bytes.
> See §5.1's *Resolution* at the end of that section. The investigation below is
> left as written; it is the record of how the symptom presented.

Nine bugs fixed getting there. **Eight of the nine were the same mistake** —
trusting a value that only QEMU happened to provide (§3).
**Reproduce:** `docs/runbooks/run-on-firecracker.md`
**Design:** `proposals/FIRECRACKER_PORT.md`

---

## 1. What was achieved

Two things that were open questions the day before:

- **A local aarch64 KVM host on a Mac.** macOS 15.7.3 / M4 Pro, via a Lima VM
  with nested virtualization. Verified from inside the guest:
  ```
  crw-rw---- 1 root kvm 10, 232 /dev/kvm
  [    0.056826] CPU: All CPU(s) started at EL2
  [    0.174297] kvm [1]: nv: 570 coarse grained trap handlers
  CONFIG_VIRTUALIZATION=y
  CONFIG_KVM=y
  ```
  `started at EL2` is the line that matters — that is nested virt actually
  engaging, not merely being supported by the silicon.

- **Akuma running as a Firecracker guest.** Console, FDT-derived RAM, PMM, MMU,
  exec, GIC, timer, the memory test suite, and virtio-blk all work.

Boot log highlights from the run with a disk attached:

```
DTB ptr from boot (x0 arg): 0x9fe00000
Akuma Kernel starting...
[Platform] firecracker device map installed
Kernel binary: 3948 KB (0x80300000 - 0x806db070)
[Memory] Detected from DTB: base=0x80200000, size=510 MB
GIC initialized
Timer frequency: 24000000 Hz
Memory Tests: ALL PASSED
[Block] Found virtio-blk at slot 0
[Block] Capacity: 2048 MB (4194304 sectors)
[FS] Ext2 filesystem mounted at /
[FS] Procfs mounted at /proc
[FS] Files in root: 15
Memory Tests: ALL PASSED
```

Three predictions from `proposals/FIRECRACKER_PORT.md` confirmed empirically:

- The FDT lands at the **top of DRAM**: `x0 = 0x9fe00000`, which is exactly
  `0xA000_0000 - 0x20_0000` (`FDT_MAX_SIZE`) for a 512 MiB guest. §4.4 predicted
  this, and predicted correctly that it is harmless because all FDT reads finish
  before the allocator exists.
- The kernel loads at **`0x8030_0000`** = `get_kernel_start()` + `text_offset`.
  §3 Q1's arithmetic held; the Image header was accepted with no changes.
- **The FDT `memory` node starts at `0x8020_0000`, not `0x8000_0000`** — the
  first 2 MiB (`SYSTEM_MEM_SIZE`) is reserved, so guest-visible RAM begins where
  the kernel loads. Akuma's existing `detect_memory()` handled this with no
  changes at all.

## 2. Docker Desktop cannot be the KVM host

Recorded so it is not retried. Two independent blockers, both outside our control:

```
$ docker run --rm --privileged alpine zcat /proc/config.gz | grep -i virtualiz
# CONFIG_VIRTUALIZATION is not set        <- KVM compiled out of the kernel
$ ... dmesg | grep -i EL
[0.005578] CPU: All CPU(s) started at EL1  <- no EL2 to run a hypervisor in
```

The kernel is a 40 MB blob in `Docker.app/Contents/Resources/linuxkit/kernel`
with no supported replacement, and Docker VMM (`libkrun.dylib`) never asks
Virtualization.framework for nested virt. Fixing either alone changes nothing.

Contrast with the Lima guest above, which is the same hardware doing it right.

## 3. The theme: six places Akuma trusted a value only QEMU provided

Every failure on the way to a working userspace was the same mistake in a
different place — **depending on a value that happens to hold because QEMU
happens to provide it.** KVM is deliberately hostile to this (its
`reset_unknown()` actively poisons registers the architecture leaves UNKNOWN),
which is what made Firecracker such an effective test of the assumption.

Worth stating because it predicts where the *next* bugs are: not in logic, but in
constants that were only ever validated against one machine.

### 3.1 `GICD_IROUTER` writes landed on the redistributor

Found by inspection while scoping the port, before any boot. The distributor was
mapped as a single 4 KiB page while `GICD_IROUTER` lives at offset 0x6000, and
`DEV_GIC_DIST_VA + 0x6000` was exactly `DEV_GICR_SGI_VA` — so step 3 of
`enable_irq`'s four-step SPI sequence had never once reached the distributor.

It worked anyway because QEMU's `GICD_IROUTER` resets to 0, which targets core 0,
which is what the code wanted. Full analysis, including the INTID→register
aliasing table and why INTID ≥ 128 would corrupt redistributor state for real:
**`docs/archive/GICD_IROUTER_ALIASING.md`**.

Fixed by giving each device a *span* rather than a page in
`akuma_primitives::addr`, with a `const` no-overlap assertion
(`DEV_WINDOW_NO_OVERLAP`) and two host tests. The predecessor test compared base
addresses only, which is why a 64 KiB device declared as one page passed it for
years.

### 3.2 `TPIDRRO_EL0` — KVM's poison reached `current_tid()`

First actual failure, right after device probing:

```
[FATAL] TPIDRRO_EL0 CORRUPT: tid=0x1de7ec7edbadc0de >= MAX_THREADS (256)
System halted - cannot determine current thread
```

`0x1de7ec7edbadc0de` ("I detected bad code") is the poison arm64 KVM's
`reset_unknown()` stamps into system registers whose reset value the
architecture leaves **UNKNOWN**. `TPIDRRO_EL0` is one of those.

`akuma_primitives::preempt::current_tid` reads it and halts the core if it is out
of range — correctly, since every per-slot static is indexed by it. Until
`threading` installs a real tid, that read has to see 0, and on QEMU it did,
because QEMU zeroes the register. Akuma had never said so out loud.

Fixed with `msr tpidrro_el0, xzr` at both entry points — `src/boot.rs` for the
BSP and `secondary_entry_shared` in `src/smp_shared.rs` for PSCI-woken
secondaries, which each get their own freshly-reset register.

### 3.3 fw_cfg — reading a device that isn't there

```
[Exception] Sync from EL1: EC=0x25, ISS=0x47
  ELR=0x8041502c, FAR=0x8000012008
```

`EC=0x25` is a data abort at the same EL; `ISS=0x47` decodes to a level-3
translation fault. `FAR = 0x80_0001_2008` is `DEV_FW_CFG_VA + 0x08` — the fw_cfg
selector register, reached from `ramfb::init`.

Firecracker has no `fw_cfg` device, so `platform::machine::FW_CFG_PA` is `None`
and nothing is mapped at that VA. On QEMU an absent file yields a clean "not
found"; here *touching the register at all* faults.

Fixed with a compile-time `AVAILABLE` gate on both public entry points in
`src/fw_cfg.rs`, so callers get the same "not found" answer they would get from a
machine whose fw_cfg simply lacks the file. `ramfb` now declines gracefully:
`[ramfb] Not available: ramfb fw_cfg entry not found`.

### 3.4 An inverted kernel-text range flooded `[IRQ POISON]`

Every timer tick printed:

```
[IRQ POISON] eret elr=0x8046cbc4 spsr=0x20000345 switched=0 tid=0 core=0
```

The tripwire:

```rust
let kernel_text = (crate::config::KERNEL_PHYS_BASE as u64..0x6000_0000).contains(&elr);
```

`0x6000_0000` is ~511 MB above QEMU's `0x4010_0000`. With
`KERNEL_PHYS_BASE = 0x8030_0000` the range is `0x8030_0000..0x6000_0000` —
**start greater than end, so permanently empty.** `kernel_text` was always
false, and for an EL1-target frame the predicate is `!kernel_text`, so every
legitimate frame was reported as corrupt.

Five sites in `src/exceptions.rs` shared the literal.

### 3.5 The same range again, plus a fourth copy of the kernel base

With 3.4 fixed, the sibling tripwire took over — `[SGI-S POISON]` from
`akuma-exec`'s scheduler, on every context switch (`old_tid=0 new_tid=1` and
back, so the scheduler was working; only the check was wrong):

```rust
let kernel_text = (0x4010_0000..0x6000_0000).contains(&elr);
```

Alongside it, `crates/akuma-exec/src/threading/mod.rs:8`:

```rust
// Must match KERNEL_PHYS_BASE in src/config.rs and KERNEL_PHYS_BASE in linker.ld.
const KERNEL_PHYS_BASE: usize = 0x4010_0000;
```

A **fourth** mirrored copy of the kernel load address — and dead, never
referenced, hidden by the module's `#![allow(dead_code)]`. Deleted.

Both 3.4 and 3.5 are now one runtime window, `akuma_exec::mmu::is_kernel_text`,
installed once from `main.rs` via `set_kernel_text_window`. It is two relaxed
atomic loads on the IRQ path, which sits right next to two `read_volatile`s of
the trap frame, so the cost is in the noise — and it cannot invert.

### 3.6 Firecracker validates the virtio handshake; QEMU does not

The longest-lived symptom. Block init *looked* healthy —

```
[Block] Found virtio-blk at slot 0
[Block] Capacity: 2048 MB (4194304 sectors)
[Block] Block device initialized successfully
[FS] Initializing filesystem...     <- and then nothing, forever
```

— so the obvious read was "ext2 mount hangs". It was not: the device had never
been turned on.

Firecracker validates every write to the virtio MMIO status register against an
exact-match transition table
(`src/vmm/src/devices/virtio/transport/mmio.rs`, `set_device_status`):

```
INIT                           -> ACKNOWLEDGE
ACKNOWLEDGE                    -> ACKNOWLEDGE|DRIVER
ACKNOWLEDGE|DRIVER             -> ACKNOWLEDGE|DRIVER|FEATURES_OK
ACKNOWLEDGE|DRIVER|FEATURES_OK -> ACKNOWLEDGE|DRIVER|FEATURES_OK|DRIVER_OK
```

A write that is not exactly one of those pairs is discarded with a warning.
`virtio-drivers` 0.7.5 (`src/transport/mod.rs:74-75`) writes:

```rust
self.set_status(DeviceStatus::empty());                              // 0x0
self.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);   // 0x3
```

`0x0 -> 0x3` skips `0x0 -> 0x1`, so it is rejected and the status stays at
`INIT`. Every subsequent queue write is then refused —
`update virtio queue in invalid state 0x0` — and `activate()`, which Firecracker
only runs on the exact transition to `DRIVER_OK`, never happens. No queues, no
interrupts, no I/O. Config-space reads need no handshake, which is why the
capacity was correct and the failure looked like a filesystem problem.

The diagnostic that cracked it was **swapping the read-only virtiofs image for a
writable local copy and getting byte-identical output.** That ruled out the disk
and left the device.

QEMU ORs status bits without validating order, which is why this had never
surfaced.

Fixed **without forking the dependency**: `crates/akuma-virtio/src/transport.rs`
adds `SteppedMmioTransport`, a newtype that delegates the whole `Transport` trait
to `MmioTransport` and overrides exactly one method, `set_status`, walking from
the current status to the requested one through the intermediate milestones.
Overriding `set_status` rather than the `begin_init` default method was
deliberate: it needs no generic bounds, so it does not pull `bitflags` in as a
direct dependency, and it fixes every caller rather than one path. There is no
platform `#[cfg]` — the extra `ACKNOWLEDGE` write is a no-op on QEMU, so both
machines now follow the spec-ordered sequence.

Cost: `MmioTransport` was a concrete type parameter in ~15 places across
`akuma-virtio` and `akuma-net`, all mechanically retyped to
`akuma_virtio::VirtioTransport`.

### 3.7 `SCTLR_EL1.SA0` — inherited SP-alignment checking killed every binary

With ext2 mounted, the first userspace execution failed deterministically. Both
`/bin/hello` processes died at the identical instruction:

```
[Exception] Unknown from EL0: EC=0x26, ISS=0x0 ELR=0x4000d8 — delivering SIGILL
[PROC-EXIT] pid=20 name=/bin/hello code=-4
```

`EC=0x26` is an SP alignment fault. Measured directly rather than guessed:

| | `SCTLR_EL1` | SA (bit 3) | SA0 (bit 4) |
|---|---|---|---|
| QEMU virt | `0x3490d185` | 0 | 0 |
| Firecracker / KVM | `0x34c5d1dd` | **1** | **1** |

`boot.rs` did `mrs sctlr_el1` → a chain of `orr` → `msr`, which **inherits every
bit the reset value carried** and clears nothing. Under KVM that means EL0
SP-alignment checking is on — a constraint this kernel's userspace ABI has never
had to satisfy, because QEMU never enforced it.

Fixed by clearing SA and SA0 explicitly, in `boot.rs` **and** in
`secondary_entry_shared` (each PSCI-woken core resets its own `SCTLR_EL1`, so
fixing the BSP alone would leave every secondary enforcing it).

Deliberately *not* fixed by reconstructing `SCTLR_EL1` from scratch: the reset
value carries the architecturally RES1 fields, and hand-rolling those is how you
get a subtly wrong `SCTLR` on the next core revision. Only the bits Akuma has an
opinion about are forced.

**Left open by this fix:** whether the initial user SP is genuinely misaligned.
SA0 exposed *something*; clearing it restores QEMU's behaviour rather than
proving the stack is 16-byte aligned. If it is not, that is a latent ABI bug and
SA0 could then be enabled deliberately as a guard. Worth a look.

### 3.9 The virtio-net header is 12 bytes under `VERSION_1`, not 10

The one that blocked networking, and the one where guessing would have wasted the
most time. Symptoms:

- `[SmolNet] DHCP deconfigured - reverting to static fallback`, forever.
- The KVM host could not reach the guest: `ping` 100% loss, TCP connect
  `No route to host` — ARP unanswered.
- `dnsmasq` logged **zero** DHCP packets, despite tap0 showing guest→host traffic.
- tap0: `RX: 11 packets, 11 dropped`. The host received our frames and discarded
  every one.

3322 bytes / 11 packets ≈ 302, which is DHCP-DISCOVER sized — so Akuma *was*
building and sending DHCP correctly. A well-formed broadcast does not get dropped
by a host with dnsmasq bound to that interface, so the frames had to be malformed.

`tcpdump -i tap0 -XX` settled it in one frame:

```
ethertype Unknown (0x4500), length 302
0x0000:  ffff ffff 02fc 0000 0001 0800 4500 0122
```

Read the bytes: `ff ff ff ff | 02 fc 00 00 00 01 | 08 00 | 45 00 ...` — a
broadcast MAC with only **four** `ff` bytes instead of six, then our real MAC,
then the ethertype, then the IPv4 header. The frame is shifted **two bytes left**:
the device consumed two more bytes as virtio-net header than the driver wrote.

Cause, in `virtio-drivers` 0.7.5 (`src/device/net/mod.rs`):

```rust
pub struct VirtioNetHdr {
    flags, gso_type, hdr_len, gso_size, csum_start, csum_offset,   // = 10 bytes
    // num_buffers: u16, // only available when the feature MRG_RXBUF is negotiated.
}
```

That is the **legacy** rule. Under `VIRTIO_F_VERSION_1` — which Firecracker
mandates, and which the boot log confirms
(`negotiated_features Features(MAC | VERSION_1 | ...)`) — `num_buffers` is
unconditional and the header is 12 bytes (`virtio_net_hdr_v1`). Firecracker sizes
it by `VERSION_1`; QEMU sizes it by `MRG_RXBUF`, which was not negotiated, so QEMU
wanted 10 and worked by coincidence.

**Fixed by bumping `virtio-drivers` 0.7.5 → 0.13.0**, where upstream had already
split the two (`VirtioNetHdrLegacy` at 10 bytes, `VirtioNetHdr` with
`num_buffers` at 12) and selects by negotiated features. After the bump the wire
is correct:

```
02:fc:00:00:00:01 > ff:ff:ff:ff:ff:ff, ethertype IPv4 (0x0800), length 304:
  0.0.0.0.68 > 255.255.255.255.67: BOOTP/DHCP, Request from 02:fc:00:00:00:01
```

and dnsmasq answers `DHCPOFFER`. `0 packets dropped by kernel`.

The bump cost ~9 mechanical errors, all in `akuma-virtio`: `MmioTransport` gained
a lifetime, `config_space` split into `read_config_space`/`write_config_space`/
`read_config_generation`, `ack_interrupt` returns `InterruptStatus`, `PhysAddr`
became `u64`, and `MmioTransport::new` takes an `mmio_size` — which is exactly the
runtime slot stride the device map already tracks. QEMU verified unaffected: boot
suite 289/0/0 **and** a real SSH session into the devbox.

### 3.10 The FDT can sit above the boot identity map

A 4 GB microVM has RAM at `0x8020_0000..0x1_8020_0000`, so Firecracker places the
FDT at roughly **6 GiB**. `boot.rs` statically maps `[0, 3 GiB)`. Reading `x0`
therefore faulted before the kernel printed one word about memory — the boot
stopped dead after `WARNING: Kernel is within 4MB of stack!`.

`extend_boot_ram_identity_map` cannot help: it needs the RAM size, which is what
the FDT is being read to discover. Fixed with
`mmu::ensure_boot_identity_covers(dtb_ptr)`, called immediately before
`detect_memory`, which maps the single 1 GiB block containing the FDT. Only that
block, deliberately — mapping the whole L1 as Normal memory would invite
speculative access to addresses with no backing store.

Invisible on QEMU at any RAM size, because the DTB goes right after the kernel.

### 3.11 Not a bug: two hypervisors fighting over host port 2222

Recorded because it burned real time and looked exactly like a regression. QEMU's
devbox runner forwards host `:2222`, and Lima **auto-forwards guest listening
sockets to the host** — so the `socat` set up to reach the microVM's sshd also
claimed `:2222`:

```
limactl    127.0.0.1:2222 (LISTEN)
qemu-syst  *:2222         (LISTEN)
```

`127.0.0.1` beats `*` for specificity, so `ssh -p 2222 localhost` went to Lima →
socat → a Firecracker microVM that was no longer running. Presented as
`Connection closed by 127.0.0.1 port 2222`, then as `rc=255` with **empty
stderr** — which reads as "networking is broken" rather than "you have two
listeners". The Firecracker forward now lives on **4444**
(`overlays/devbox-firecracker/guest-setup.sh`).

### 3.8 virtio-rng is not optional

Not a kernel bug — a config omission that looked like one. Firecracker attaches
no entropy device unless the config says `"entropy": {}`. Without it:

```
[RNG] Hardware RNG not available
[Test] rng entropy-live FAILED (ok=false nonzero=false differ=false)
[Test] syscall-bkl-optout: getrandom FAILED r=-5      (EIO)
[Test] syscall_bkl_optout FAILED (1 cases)
```

Three failures, one cause. QEMU's runner always provides an RNG, so its absence
reads as a kernel regression. With it added:

```
[RNG] Found virtio-rng at slot 2
[RNG] Test read successful
```

**290 PASSED, 0 FAILED, 0 POISON** — one more than the QEMU baseline of 289
(Firecracker's boot exercises a path QEMU's does not).

Device slot order follows Firecracker's device-creation order, which is the order
of the config: block at slot 0, net at slot 1, rng at slot 2. `[Net] virtio-net
IRQ: slot 1 -> INTID 33` confirms `VIRTIO_INTID_BASE = 32` is right — 32 + slot 1.

### 3.12 Printing from a preemption-disabled section wedges a single-vCPU guest

**Self-inflicted, and the most instructive of the set.** Removing `max_level_off`
from `akuma-net`'s `log` dependency (§5.1's diagnostic work) resurrected **25
previously-dead `log::` statements** — including several inside
`smoltcp_net::poll`'s `NETWORK` critical section, which is explicitly documented
as running with preemption disabled:

```rust
// Hold preemption disabled for the whole NETWORK critical section so the
// spinlock is never stranded across a context switch (fatal under the BKL)
let _pg = PreemptGuard::new();
let mut guard = NETWORK.lock();
...
    log::info!("[SmolNet] DHCP configured");   // ← now real console I/O
```

`console::emit` runs inside `with_irqs_disabled` and, when `kernel_console_lock`
is set (default-on for `release`), acquires a `Spinlock`. A print from a
preemption-disabled section therefore spins on a lock whose holder may be a
thread that cannot be scheduled. On a multi-core guest another core drains it; on
a **single-vCPU** guest nothing can, and the kernel wedges silently.

That is why it presented as "QEMU fine, Firecracker hangs": QEMU's devbox runs
`SMP=4`, Firecracker runs one vCPU.

**Two corrections to earlier conclusions in this document's history:**

1. The deterministic hang was blamed on declining `RING_EVENT_IDX`. Wrong — it
   appeared when an RX diagnostic `log::info!` was added to `Device::receive`,
   which smoltcp calls *from inside* the `NETWORK` critical section. The EVENT_IDX
   experiment merely ran at the same time.
2. The mechanism was briefly doubted on the grounds that `CONSOLE_LOCK` is
   `#[cfg(kernel_console_lock)]` and therefore opt-in. It is opt-**out**:
   `build.rs` sets `console_lock_default_on = !size_opt_for_console`, so every
   `release` build has it. Verified by symbol count: the default build carries
   `CONSOLE_LOCK`/`CONSOLE_OWNER`, the size profiles do not.

Fixes, in order of how much they matter:

- **`akuma-net` prints with `safe_print!`, not `log::`.** The `log` dependency is
  there for **smoltcp**, and `max_level_off` existed to compile smoltcp's
  per-packet tracing out. Routing our own messages through the same facade meant
  either resurrecting that tracing or losing our messages with it. All 19 of
  akuma-net's own messages are now `safe_print!`, which is the `CLAUDE.md`
  convention anyway. `log` is retained, filtered to `release_max_level_info`, so
  third-party crates still report — and that is what produced the
  `negotiated_features Features(MAC | VERSION_1)` line that cracked §3.9.
- **Nothing prints from inside the `NETWORK` critical section.** `poll()` records
  what happened into a `DhcpReport` and emits it after `drop(guard)`, next to the
  pre-existing comment warning about the identical hazard with `SOCKET_TABLE`.
  RX-path observability is plain atomic counters (`rx_counters()`), not prints.
- **`kernel_console_lock` now defaults OFF for `platform-firecracker`.** The lock
  prevents cross-core PL011 byte-interleaving, which cannot occur on a guest that
  only supports one vCPU — while the deadlock it enables certainly can.
  `CONSOLE_LOCK=1` forces it back on.

The general rule, now recorded in `docs/reference/firecracker/`: **a print is a
lock acquisition.** Treat it as one when auditing a critical section, and never
add one inside a section that disables preemption.

## 4. What the port actually needed

Smaller than the survey estimated, because the structural decision paid off.
`src/platform.rs` is the only file describing either machine.

| Change | Where |
|---|---|
| Machine descriptions (both) | `src/platform.rs` (new) |
| Device VAs become spans; GICD gets 64 KiB | `crates/akuma-primitives/src/addr.rs` |
| `DEV_PAGES` const → runtime `DevRegion` map | `crates/akuma-exec/src/mmu/mod.rs` |
| Boot asm maps **only** the UART | `src/boot.rs` |
| L1[1] Normal vs Device, per machine | `src/boot.rs` (assembler `.if`) |
| virtio stride/count become runtime | `akuma-primitives`, `akuma-virtio` |
| Spec-ordered virtio status handshake | `crates/akuma-virtio/src/transport.rs` (new) |
| `SCTLR_EL1` SA/SA0 forced off | `src/boot.rs`, `src/smp_shared.rs` |
| Runtime kernel-text window | `crates/akuma-exec/src/mmu/mod.rs` |
| virtio INTID base 48 → 32 | `src/main.rs` |
| Kernel base via linker `--defsym` | `build.rs`, `linker.ld`, `src/config.rs` |
| `platform-firecracker` feature | `Cargo.toml` |

The boot assembly maps one page — the console — because that is the only device
whose address *can* be a compile-time literal. Everything else is installed from
Rust by `mmu::rebuild_boot_device_table` before the first GIC or virtio access.
That structure is what made the Firecracker arm mostly a table of constants.

**No regression on QEMU**: `cargo run --release` still boots to 289 PASSED /
0 FAILED with `[Platform] qemu-virt device map installed`.

## 5. Open

### 5.1 Inbound (RX) never reaches the guest

The single remaining blocker for SSH. Everything else on the path works: ext2
mounts off the 6 GB devbox image, the boot suite passes 290/0/0, userspace runs,
and `[herd] Started sshd (pid= 2)`.

State of the evidence:

- **TX is correct.** Frames on the wire are well-formed (§3.9) and dnsmasq
  answers `DHCPOFFER`.
- **The guest never sees the reply.** No `DHCPREQUEST` follows the offer, and
  host ARP for the guest goes unanswered.
- **tap0 host→guest: `0 packets, 60 dropped`.** Every inbound frame is discarded
  because nothing consumes it.
- **A receive buffer *is* posted, and nothing ever fills it.** The heartbeat now
  carries `rx_counters()`:

  ```
  [Heartbeat] Loop 21112 | T1 | SmolNet Active | rx posted=1 fail=0 recvd=0
  [Heartbeat] Loop 43351 | T1 | SmolNet Active | rx posted=1 fail=0 recvd=0
  ```

  43,000 netpoll iterations, one buffer posted, zero `receive_begin` failures,
  **zero completions**. `posted` is exactly 1 because the single-buffer path only
  re-posts after a completion, which never arrives.

That last line settles two things that were repeatedly re-litigated:

- **It is not a stuck init.** The heartbeat only ticks from the netpoll loop, and
  `[herd] Started sshd (pid= 2)` precedes it. The stack is running.
- **It is not a header/format mismatch on RX.** A wrong `hdr_len` would yield
  frames at the wrong offset — `recvd` climbing with garbage contents. There are no
  completions at all, so the failure is upstream of parsing. (Reasonable hypothesis
  given §3.9 was exactly that bug on TX.)
- **And it is not selective.** "Why does it not answer TCP?" has the same answer as
  "why does it not answer ARP": it receives *nothing*. TCP is downstream of ARP,
  which is downstream of RX working at all.

**Confound to clear before testing, learned the hard way:** a failed ARP leaves
`10.0.2.15 FAILED` in the host neighbour table, and Linux then returns
`EHOSTUNREACH` immediately without re-ARPing. `sudo ip neigh flush dev tap0`
between attempts. A stale *resolved* entry is worse — it appeared once as
`10.0.2.15 lladdr 02:fc:00:00:00:01 DELAY` and briefly looked like the guest had
answered. It had not; the entry outlived the instance that produced it. Always
flush, then re-test.

Also ruled out: the MAC and the forward target. tcpdump shows the guest
transmitting *from* `02:fc:00:00:00:01` and the host addressing frames *to* the
same MAC, and socat forwards to `10.0.2.15`, which is the guest's static-fallback
address.

Two things ruled out by experiment rather than reasoning:

- **`RING_EVENT_IDX` notification suppression.** Plausible: EVENT_IDX lets the
  driver skip the queue kick, and Firecracker's net device defers RX and waits
  for one (its startup `Artificially kick devices` exists for that hazard).
  Declining the feature — masked at `read_device_features`, so driver and device
  agree — produced `Features(MAC | VERSION_1)` and RX still never delivered, so it
  is not the cause. It was *also* blamed for a deterministic hang at the time;
  that was wrong, and the real cause was a diagnostic print inside a
  preemption-disabled section (§3.12). Reverted regardless, as unproven. Note that
  masking at `write_driver_features` instead is actively harmful:
  `negotiated_features` is computed from the *read*, so the driver stays in
  EVENT_IDX mode while the device was never told.
- **A missing `event_idx` argument.** `virtio-drivers`' net driver does pass
  `negotiated_features.contains(Features::RING_EVENT_IDX)` into both its transmit
  and receive `VirtQueue::new` calls (`device/net/dev_raw.rs`), same as `blk.rs`.

Worth knowing about the code: `RxRing` in `virtio_rings.rs` is **not** the live
path — it is `#[cfg(feature = "net-noalloc")]`. The active receive path is the
single-buffer one in `smoltcp_net.rs` using `rx_buffer: [u8; 2048]` and
`rx_token`. An early diagnostic went into `RxRing::refill` and never printed on
*either* platform, which is how that was discovered; don't repeat it.

Next avenues: whether the posted descriptor is actually visible to Firecracker
(avail-ring publication / the `share`+`unshare` no-op cache-maintenance path in
`akuma-virtio`'s `Hal`), and whether the RX queue index Akuma posts to matches the
one Firecracker services.

#### Resolution (2026-08-21)

**Root cause: a delivery gate on posted receive capacity, with no guest-visible
error.** Firecracker's virtio-net does not read a single frame off the host tap
until the **total** capacity of the receive descriptors the driver has posted
reaches `MAX_BUFFER_SIZE` = **65562 bytes**
(`src/vmm/src/devices/virtio/net/device.rs`, `read_from_mmds_or_tap`). Akuma
posted one 2 KB buffer — 2048 of the required 65562 — so the gate never opened.
Every inbound frame was dropped and counted in the device's
`no_rx_avail_buffer` metric, which the guest cannot see.

That explains each observation above exactly: TX perfect, one buffer posted,
zero `receive_begin` failures, zero completions, and `tap0` showing every
host→guest frame dropped. The driver was not wrong and the buffer was not
unposted — it was too small to be *eligible*.

QEMU imposes no such threshold, which is why the same receive path had worked for
years.

**Fix:** `akuma_net::smoltcp_net::RX_BUFFER_LEN` = 65568 (65562 rounded up to a
multiple of 8), on every platform rather than behind a Firecracker `cfg` — so the
receive path exercised daily is the one Firecracker needs. The buffer lives in
BSS rather than a `VirtioSmoltcpDevice` field because `NetworkState` is built on
the kernel stack before being moved into `NETWORK`, and 64 KB would be a stack
temporary on a 96 KB system stack.

`VIRTIO_NET_F_MRG_RXBUF` would let the device chain several small buffers to the
same total, but `virtio-drivers` does not offer that feature, so the capacity has
to come from a single descriptor.

**Consequence for `extreme-size`:** it keeps the 2 KB buffer deliberately — 4 MB
of RAM makes 64 KB of BSS 1.6% of the machine, and it is a QEMU target
(`acceptance/05`). That profile has **no inbound networking under Firecracker**.

**Still unverified.** The fix landed after the last boot recorded here, so no
Akuma boot has yet demonstrated a completed receive on Firecracker. Treat SSH-in
as expected, not proven, until one does.

### 5.2 `akuma_net::init` hangs nondeterministically

Same binary, different outcomes: some runs reach `Herd started`, others freeze
inside `akuma_net::init` at ~96% CPU. Confirmed not to be output buffering
(reproduced under `stdbuf -o0`; the log stops at a fixed line count for minutes).
Note: the deterministic variant of this was **§3.12**, a print inside a
preemption-disabled section, and is fixed. What remains is the original
intermittent form, last seen before that fix; it has not recurred since but has
not been proven gone either.

### 5.3 The DHCP settle loop spins, and IRQ volume is high

`src/network_tests.rs` polls up to 5000 times with only `yield_now()` between
iterations and no `wfi`. Correctly bounded (5000 iterations, 5 s), so not a hang —
but on a single-vCPU microVM nothing else is runnable, so `yield_now()` returns
immediately and it burns 100% of the core for the full five seconds whenever no
lease arrives. On QEMU a SLIRP lease lands in milliseconds, masking it.

Separately, both platforms print `[BKL] dropped window preserved across IRQ`
doubling to `x131072` — a very large interrupt count for an idle system, and the
other half of the ~96% CPU observation. Not investigated.

### 5.4 Still outstanding

- **`vcpu_count > 1`.** Firecracker places the GIC redistributors at
  `0x3FFF_0000 - vcpu_count * 0x2_0000`, so CPU0's frames move with the count and
  Akuma's compile-time bootstrap map assumes one. The FDT-derived device map is
  the fix and is **not implemented**. Largest remaining piece after §5.1.
- **Whether the initial user SP is genuinely 16-byte aligned.** §3.7 restored
  QEMU's behaviour by clearing `SA0` rather than proving alignment.
- **`src/tests.rs` map assertions.** ~20 sites treat `0x4000_0000..0x8000_0000`
  as kernel RAM; under Firecracker that is the MMIO window. The suite reports
  290 PASSED so most run, but some of those assertions pass vacuously.
- **Building Akuma inside the Firecracker guest** — the self-host target. Blocked
  on §5.1 only for convenience (SSH); the disk and userspace already work.

## 6. The lesson worth keeping

Eight of the nine bugs were a value that had only ever been checked against one
machine — a register reset value, an address literal, a status-transition
assumption, a protocol header size — and one of those was a mirrored copy of such
a value. None were logic errors.

The two most expensive to find were the two where the *symptom pointed somewhere
else*: a 12-byte header read as 10 presented as "ext2 mount hangs" and then as
"DHCP doesn't work", and a port collision between two hypervisors presented as
"my change broke QEMU networking". In both cases the thing that actually resolved
it was cheap, direct observation — `tcpdump -XX` on one frame, `lsof -iTCP:2222` —
rather than more reasoning about the code. The device-map abstraction that came out of this — **fixed VAs, discovered
PAs** — is the shape that prevents the class, and the `const`
`DEV_WINDOW_NO_OVERLAP` assertion is what makes the layout self-checking rather
than self-documenting.

The remaining known instance of the same class is the vCPU-dependent
redistributor base (§5). It is currently a literal that is correct for exactly one
configuration, which is precisely the shape of the four bugs above.

---

## Background

- `proposals/FIRECRACKER_PORT.md` — verified constants and the port design.
- `docs/archive/GICD_IROUTER_ALIASING.md` — §3.1 in full.
- `docs/runbooks/run-on-firecracker.md` — how to reproduce all of this.
- `docs/archive/PORTING_POSSIBILITIES.md` — the original options survey.
