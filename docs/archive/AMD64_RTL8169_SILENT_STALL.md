# The RTL8169 receiver that stopped, and the detector that could not see it

**Status: FIXED 2026-09-20**, verified on the bare-metal box (the trashcan, HP
500-502nj, RTL8168g XID `0x4c0`). `crates/akuma-net-rtl8169/src/stall.rs` (new,
host-tested) and `crates/akuma-net-nic/src/rtl8169.rs`.

## Symptom

The box boots, runs, and is unreachable — at `192.168.1.123` **and** at the
pre-DHCP static `192.168.1.220`. The console shows the machine is alive and the
receive loop is running:

```
[probe] t=1294s ticks=117129(cal) link=up/1000M/full ip=192.168.1.220/24 dhcp=pending dns=1.1.1.1 clk=dns-fail/try73
[probe]   rx=35 tx=350 drop=0 isr=0x4095 dry=1 kicks=0 polls=156 posted=0 rxfail=0 irq=0 laps=83892
  dns: 1.1.1.1: no reply before timeout
  dns: 8.8.8.8: no reply before timeout
```

`tx` climbs, `rx` does not. Nothing arrives, so there is no DHCP offer, no DNS
reply and no ARP reply — the box transmits into a void at whatever address it
has. It stayed like this for **20+ minutes** across three separate boots, on
three different kernels (a 06:29 build, the current tree, and
`akuma-amd64.good` from the day before), and recovered only when a person power
cycled it.

**`posted=0` in that line means nothing** and cost some minutes: `rx_buffers_posted`
is bumped only on the virtio path (`akuma-net-nic/src/device.rs`), never by this
driver. `dry=`, `kicks=` and `rx=` are the real numbers here.

## What it was not

Worth recording, because each was a plausible theory that the evidence killed:

* **Not the network.** Ubuntu on the *same box, cable, switch and router* leased
  fine minutes earlier — the kernel was installed over that very ssh session.
  Same NIC hardware, different driver, different outcome.
* **Not a recent regression.** Suspicion first fell on the same day's network
  commits (`c6f36eb4` 05:38 → `a672d771` 06:44) because the running kernel
  looked older. It was not: **the timestamp was read from inside Akuma, whose
  clock is UTC**, so a file Ubuntu called 06:29 read as 03:29 there. The
  known-good kernel from the previous day failed identically, which settles it.
* **Not stale DMA state across a warm reboot.** `perform_reset` quiesces xHCI and
  not the NIC, which made this attractive — but `Nic::init` opens with
  `reset()` → `CR_RST`, and a full cold power-off reproduced the fault anyway.
* **Not a dead link.** `link=up/1000M/full` every lap.

## Root cause: the first frame disarms both detectors, permanently

The stall watch had two arms, and the fault is neither of them.

`on_rx_frame` (`rtl8169.rs`) runs on every frame that comes off the ring:

```rust
self.last_rx_us = now_us();
self.rx_backpressure = false;   // "the next stall must produce its own evidence"
self.rx_seen = true;            // "retires the `blind` arm for the rest of the boot"
```

and the decision was:

```rust
let blind   = !self.rx_seen;
let stalled = quiet && (self.rx_backpressure || blind);
```

* **`Backpressure`** is the chip's own evidence: the `RDU` bit of the `ISR` the
  poll loop already harvests, meaning "the ring ran dry with a frame waiting".
  It is the best signal there is and it is free. But a receiver that has
  **stopped** takes nothing off the wire, so it has nothing to report — `RDU`
  cannot fire for the fault that most needs catching. (The file already knew
  this shape: the same reasoning is why `blind` was added for a *gated*
  receiver.)
* **`Blind`** is `!rx_seen`, and the first frame retires it for the whole boot.

So the sequence "a few frames arrive, the receiver stops" disarms **both** on the
way past: `rx_seen` is now true, and `rx_backpressure` was cleared by the last
frame with nothing able to set it again. `stalled` can never be true for the rest
of the boot, `kicks` stays at `0`, and the resync / `kick_receiver` / full
`init()` ladder sits intact and unreachable.

**The 35 frames are what made this unrecoverable.** Had *zero* arrived, the
`blind` arm would have kicked every ~5 s and re-initialised the chip every
fourth attempt. Receiving a little is strictly worse than receiving nothing.

## Fix: a third arm, and the decision moved somewhere testable

`akuma-net-rtl8169::stall` is the arming decision as pure logic — no register,
no I/O, no state — beside the bring-up sequence the crate already owns, under
its `#![forbid(unsafe_code)]`:

```rust
pub enum StallArm { None, Backpressure, Blind, Silent }
pub fn decide(policy: &StallPolicy, i: &StallInputs) -> StallArm
```

`Silent` is the new arm: **frames arrived, then stopped, and the chip is not
complaining.**

**The order is the substance**, and it has its own tests because a test that
sets one condition at a time passes under every ordering:

1. `Backpressure` outranks everything — the chip's own report beats any
   inference from elapsed time, and the two recoveries differ.
2. `Blind` is checked against `rx_seen`, not against a duration: "nothing has
   ever arrived" is a stronger statement than any threshold, and it must never
   be reachable at the same time as `Silent`.
3. `Silent` needs a clock and **abstains without one**. There is no honest way
   to measure "stopped" in laps once the loop's rate is a scheduling decision —
   `STALL_LAPS` is 2,000,000, which is five and a half hours at the parked rate.

The numbers stay in the consumer (`rtl8169.rs`), because they are statements
about its poll loop and its LAN rather than about the chip:

| constant | value | why |
|---|---|---|
| `STALL_QUIET_US` | 5 s | silence that makes a *complaining* chip a stalled one |
| `STALL_SILENT_US` | **10 s** | silence that makes a quiet, previously-working receiver a stalled one |
| `STALL_SILENT_RETRY_US` | 5 s | between subsequent `Silent` firings |
| `BLIND_REINIT_EVERY` | 4 | attempts before escalating to a full `init()` |

The **asymmetry between 10 s and 5 s is deliberate**: slow to first accuse a
quiet link of being broken, quick to retry once it has proved itself
pathological. Without it the recovery cadence equals the accusation threshold
and the escalation to a re-init lands four windows later, with the box off the
network throughout.

Two rules from earlier incidents are preserved and restated at the call site:

* **No register reads on the per-lap path.** The inputs are `rx_seen`, the `RDU`
  bit already harvested, and a clock. `MPC` would be better evidence, and
  reading it here took the box down twice on 2026-09-19 — `snapshot()` is eleven
  MMIO reads and there is no "occasionally" on a per-lap path
  (`AMD64_TRASHCAN_ISSUES.md` §7b, `AKUMA_NET_ISSUES.md` §11.7).
* **The recovery is never capped**, only its printing. A chip that needs
  restarting every ten seconds still needs restarting, and a cap makes "the
  recovery does not work" indistinguishable from "the recovery was allowed
  three tries".

## Verification

Host: `cargo test -p akuma-net-rtl8169` — 45 passing, 11 of them new in
`stall::tests`, covering the observed failure
(`a_receiver_that_started_and_then_went_quiet_is_a_stall`), both conflicts
(`backpressure_outranks_silent_when_both_hold`,
`silent_never_fires_before_the_first_frame`), the retry asymmetry and the
no-clock abstention. Full workspace suite green (148 test binaries).

On the metal, first boot carrying the arm (at a 60 s horizon, before it was
tightened):

```
[rtl] SILENT: no frame for 60000000 us with no RDU (rx=16 frames) - receiver stopped
[rtl] cr=0x0c isr=0x0001 imr=0x002f rcr=0x0002c70e mpc=0 misc=0x0000003f rxdv_gated=0 cursor=0/16
[rtl] rdsar=0x00000000005b7d00 tnpds=0x00000000005b7e00
[probe] rx=357 tx=101 drop=0 isr=0x4095 dry=5 kicks=1 ... ip=192.168.1.123/24 dhcp=leased dns=1.1.1.1 clk=set
```

`rx=16` is **exactly `RING_LEN`** — the signature this driver's own note
describes. One detection, one kick, full recovery, and DHCP completed on its own
with no one at the machine. The horizon was then cut to 10 s, since the box is
deaf for every second of it.

## What this does not fix

`kicks=1` says the *detection* gap is closed. It says nothing about why the
receiver halts after one ring in the first place — that fault is still there and
is now merely survivable. The next read is the stall dump's `rdsar=` against the
bring-up `nic: rx_desc pa=` line (whether the chip is writing where the driver
reads) and `cursor=N/16` against the `rx[N] cmdstat=` ownership bits.

## Background

* [`AKUMA_SELF_HOSTING_AMD64.md`](AKUMA_SELF_HOSTING_AMD64.md) § "`netprobe`:
  the answer was a command-line flag away the whole time" — why the probe line
  exists.
* [`AMD64_TRASHCAN_ISSUES.md`](AMD64_TRASHCAN_ISSUES.md) §7b — the per-lap MMIO
  rule this fix had to respect.
* [`../runbooks/amd64-bare-metal-loop.md`](../runbooks/amd64-bare-metal-loop.md)
  § "The `Silent` arm" — the operational version, and the `kicks=` reading that
  separates a detection failure from a recovery failure.
