# The RTL8169 driver, audited (2026-10-13)

**Status: review of `crates/akuma-net-rtl8169` (the driver logic) and
`crates/akuma-net-nic/src/rtl8169.rs` (the MMIO/DMA glue), by reading plus a
host reproduction against `FakeChip`. One flaw is **confirmed by a failing
test** — `resync_rx_cursor` skips a completed frame sitting under the cursor
(§2, repro committed as
`crates/akuma-net-rtl8169/tests/resync_reorder_repro.rs`). One is a
high-impact design gap — the transmit path has no error handling and no wedge
recovery at all (§3). The rest are medium and low, with the reasoning for
each so the next reader can disagree with the conclusion rather than
rediscover it. Nothing here invalidates the parts the crate gets right,
which §12 lists, because a fixer who does not know which walls are
load-bearing will knock one down.**

**Fixes landed (2026-10-13, same day as the audit).** §2, §4, §5, §6 (both
halves), §7, §8 and §9 are fixed in the driver crate and glue, with the
tests that pin them; the §2 repro is unignored and now guards the fix. §3
is fixed in its *mechanism*: the transmit stall watch (`stall::tx_decide`,
host-tested) rings the doorbell on a full-and-quiet ring and escalates to a
full re-init once the doorbell has failed across the long horizon, and
`tx_frames_completed` makes the counters honest — `tx=queued/completed` in
the probe line is the wedge signature. What is *deliberately not* fixed is
the reading of the descriptor's transmit status bits (`TABRT` et al.): no
bit of that word has been measured on silicon this crate has run on, and
inventing constants from a datasheet is what `chip.rs`'s rule exists to
prevent — `desc.rs` now says so where a reviewer would reach for them. §10
takes no code change by design: reaping already runs every lap, and the
parked lap cadence is the scheduler's decision, not the driver's. §11 is
closed with the contract in prose (`stop`/`release` docs), there being no
pollable quiesce bit to wait on and no caller today.

---

## 1. Scope and method

* **In scope:** `akuma-net-rtl8169` in full (`driver.rs`, `ring.rs`,
  `desc.rs`, `regs.rs`, `mdio.rs`, `link.rs`, `chip.rs`, `stall.rs`,
  `model.rs`), the glue in `akuma-net-nic/src/rtl8169.rs`, the call sites in
  `akuma-net-nic/src/loopback.rs`, and the platform assumptions in
  `amd64/src/net.rs`.
* **Method:** line-level read of both layers against the RTL8168 family's
  documented behaviour; the ownership protocol was checked direction by
  direction; every suspicion that `FakeChip` could express was turned into a
  test. The repro in §2 runs the real `Nic` code against the model, which
  enforces the ownership contract — so the failure shown is the driver's, not
  a mock artefact.
* **Not in scope:** the amd64 PCI enumeration, the smoltcp interface layer,
  and the wifi path. Some findings below (§3 TX recovery, §4 IMR) touch the
  platform only in the sense that the platform's current shape is what keeps
  them latent.

Severity: **confirmed** (failing repro), **high** (silent permanent damage on
a plausible fault), **medium** (correctness gap, currently guarded by luck or
by this machine's shape), **low** (latent, portability, hygiene).

---

## 2. CONFIRMED — `resync_rx_cursor` skips the frame under the cursor and
### reorders delivery

**Where:** `driver.rs:288-301`; call sites `rtl8169.rs:705-717` (the
`RESYNC_AFTER_IDLE_LAPS` guard) and `rtl8169.rs:506` (`on_stall`, every arm
after the first).

**What it does:** the scan starts at `step in 1..len` — cursor+1, never the
cursor itself — and seeks to the first not-chip-owned slot it finds.

**Why that is wrong.** The doc's own safety argument ("`OWN=0` … can never
mean a stale one … the driver re-posts on consumption") proves that every
`OWN=0` slot holds an unconsumed frame. It then never explains why *skipping
the unconsumed frames between the cursor and the found slot* is safe, because
it cannot: the `1..` start buys the rewind case (chip restarted at the ring
base — the HP-box 2026-09-20 failure) at the price of the forward case. In
the forward case the chip has merely advanced *past* the cursor in normal
order, and the slot under the cursor holds the **oldest** frame of the burst.

**The premise is stale by one lap.** The glue fires the scan only after ≥64
idle laps — which means the previous lap's `receive()` returned `None`
because the cursor slot was chip-owned *then*. On the scan lap the scan runs
**before** `receive()` (`rtl8169.rs:705` → `:719`), and in between the chip
may have completed the cursor slot and the one behind it. That is a legal,
ordinary state for a burst's head. The scan then seeks past the cursor slot.

**Demonstrated** (`tests/resync_reorder_repro.rs`, real `Nic` against
`FakeChip`; cursor at 6, chip completes slots 6 and 7):

```text
resync moved cursor to Some(7) (was 6)
first frame out: Some(60) bytes, first byte 0xbb   <- the NEWER frame
second frame out: None                             <- 0xAA is stranded
assertion failed: both frames must be delivered (true, false)
```

Worse than the reordering: after the mis-seek, `receive()` delivers slot 7
and the cursor wraps to 0. Slot 6 — `OWN=0`, a perfectly good frame — is now
*behind* the cursor, invisible to everything until the cursor travels the
whole ring again, and every subsequent scan skips it too (they also start
forward). The poll loop counts idle laps while a frame sits in the ring with
`MPC` at zero, which is exactly the evidence picture that arms
`StallArm::Silent` five seconds later.

**Candidate root cause for the field symptoms.** The glue's own log quotes a
healthy-looking kick — `stall #3: kick misc 0x3f -> 0x3f mpc=0` — a stall
arm with *no* chip evidence. A mis-seek produces precisely that signature on
a working link: nothing arriving from the ring's side, no `RDU`, no misses.
This does not retroactively explain every `SILENT` arm (the 2026-09-20 case
plausibly was a genuinely stopped receiver), but it is the first mechanism
found that makes a *healthy* receiver look stalled with `mpc=0`, and it
fires on exactly the workload this box runs — bursty LLM streaming over an
otherwise idle link.

**Frequency estimate.** The parked loop laps at ~`NETPOLL_IDLE_PARK_US`
(~10 ms); the scan fires once per 64 idle laps (~640 ms of silence). A burst
whose head completes on a scan lap — roughly 1–2 % of post-idle bursts, and
on a quiet LAN most bursts are post-idle — mis-seeks. Each mis-seek costs an
out-of-order TCP segment plus a stranded duplicate until the cursor wraps:
DUPACKs or a full RTO per occurrence, and a false `SILENT` stall arm to
boot.

**Fix direction.** Scan from `step in 0..len`. Because the driver consumes
strictly in cursor order, the first not-chip-owned slot at or after an
chip-owned cursor is exactly the oldest unconsumed frame in *both* cases: at
`step 0` it repairs the forward case (no-op when the cursor is honest — the
premise of `resync_invents_nothing_when_the_ring_is_empty` still holds), and
at `step ≥ 1` it still finds the ring-base frame in the rewind case. One
character-class change, both repairs, no new state. Then unignore the repro.

---

## 3. HIGH — the transmit path has no error handling and no wedge recovery

**Where:** `driver.rs:198-222` (`transmit`), `driver.rs:306-316`
(`reclaim_tx`), glue `rtl8169.rs:879-897` (`emit_frame`), counters at
`rtl8169.rs:890-893`.

Three separate holes, one theme — the whole stall machinery (§`stall.rs`,
`on_stall`, `kick_receiver`, the re-init ladder) is receive-side only:

1. **Completion errors are invisible.** `reclaim_tx` checks only `OWN`. A
   descriptor the chip aborted (`TABRT` — e.g. a bad DMA address — or `OWC`,
   `CRS`) clears `OWN` like a success. `INT_TER` is latched into `ISR` and
   the glue merely ORs it into the `rx_isr_seen` counter (`rtl8169.rs:628`).
   No path anywhere reads a transmit descriptor's status bits.
2. **A TX wedge is unrecoverable.** If the chip stops draining the TX ring,
   `reclaim_tx` returns 0 forever, `transmit` returns `TxError::Full`
   forever — and none of the recovery arms can fire, because every one of
   them requires receive silence. A chip that transmits nothing but happily
   receives is a permanent silent outage: TCP retransmits into a full ring
   until the connection dies, while every receive-side health number stays
   green.
3. **`tx_frames_sent` counts queued, not sent** — bumped at queue time in
   `emit_frame`, never reconciled with completions. On a wedge, the counter
   keeps climbing while nothing goes on the wire. `ifstats` will testify to
   traffic that never existed.

**Fix direction.** Read the completed descriptor's status word in
`reclaim_tx` (it is already in `cmdstat`'s low bits after the chip clears
`OWN` on this family); count `TABRT` separately; and let a TX ring that has
been full with zero reclaim for `STALL_QUIET_US` arm a TX-side recovery
(a doorbell re-ring first, then the same re-init ladder the RX side has).

---

## 4. MEDIUM — `init` arms `IMR` although nothing owns the interrupt line

**Where:** `driver.rs:152` (`IMR ← INT_DEFAULT_MASK`), against the crate's
own contract at `lib.rs:34-35` ("**No interrupt handler.** … routing the line
and deciding when to call it are the consumer's") and the platform reality
(`amd64/src/net.rs:343`: "one-core, no-IRQ machine"; no handler is
registered anywhere).

On this target the line is never routed, so it is latent. But the chip is
told to raise interrupts that no one will take: on any future platform that
unmasks the pin, or shares the line with a device that *is* serviced, this
is an un-acked level-triggered interrupt — a storm, or another device's
handler spinning on a line this driver keeps asserted. A polled driver's
default should be `IMR = 0`, with arming behind the same explicit decision
as registering a handler.

---

## 5. MEDIUM — `init` violates its own ordering rule: receiver on before
### `RCR`/`MAR`/`TCR`

**Where:** `driver.rs:138-148`. The module header (`driver.rs:16-20`) states
the rule — "the moment the receiver is enabled the chip may DMA into
whatever the base points at" — and `init` honours it for the ring bases…
then sets `CR_RE | CR_TE` at line 138 and *only afterwards* writes `TCR`
(140), `RCR` (142) and the multicast table (147).

The receiver is live for several bus cycles under the reset defaults of
`RCR`/`MAR0`, and the transmitter under default `TCR`. Mitigation is real —
the PHY is still negotiating at this point (~3 s, per the glue's own
measurements) — but that hedge evaporates on a warm reset with link already
up: the first frames are accepted under whatever `RCR` reset left (the
reset value's `AER`/`AR`-class bits accept errored/runt frames into a
freshly posted ring). The rule exists precisely because this class of bug
does not announce itself; `init` should follow it end to end: config block
first, `CR_RE | CR_TE` last.

---

## 6. MEDIUM — `kick_receiver` misorders in-flight frames rather than
### "losing" one, and never waits for the receiver to stop

**Where:** `driver.rs:354-375`.

Two corrections to the record, because the doc under- and over-states the
damage at the same time:

* The comment (and `stall.rs`'s trade note) says the kick "loses a frame the
  chip has written but the driver has not yet read". It does not lose it —
  it *misorders* it. `kick_receiver` resets the cursor to 0 while slots with
  `OWN=0` (pre-restart frames, unconsumed) sit ahead in the ring. The chip's
  restart walk takes only `OWN=1`, so those slots keep their old frames; the
  driver's cursor delivers them stale, interleaved with the chip's new
  frames, whenever the cursor reaches them. Note this is also the one
  operation in the crate that creates the exact state the `resync` doc
  declares impossible ("`OWN=0` … can never mean a stale one") — the two
  recovery paths contradict each other's invariants.
* Between `CR ← cr & !CR_RE` (line 364) and the `RDSAR` rewrite (line 365)
  there is no wait for the receiver to actually stop. The datasheet's reason
  for not restating `RDSAR` under a running receiver applies for a few DMA
  cycles after `RE` falls, too; the write is back-to-back MMIO. A bounded
  poll (the reset pattern already exists) between the two would close it.

On an all-chip-owned ring (the canonical stall) the kick is clean; the
misordering case is the partial ring, which is what a burst-at-stall
leaves behind.

---

## 7. MEDIUM — silent-truncation contracts in the memory trait

**Where:** `lib.rs:175-188` (`rx_buf_read` returns `min(len, dst.len())`),
`driver.rs:230-253` (`receive` reports that truncated count as the frame
length), `rtl8169.rs:215-223` (`tx_buf_write` truncates to `BUF_LEN`),
`driver.rs:198-222` (`transmit` reports `Ok` after a truncated copy).

The glue's buffers (2048 everywhere) make these unreachable today, and
`transmit` at least guards `MAX_FRAME`. But the trait contract does not
require `RX_BUF_SIZE`-sized buffers or `MAX_FRAME`-sized TX buffers, so a
future consumer with tighter memory gets: receive → a silently truncated
frame handed up as a valid packet; transmit → a silently truncated frame
acknowledged as sent. The FakeChip cannot catch either (its buffers are
2048 ≥ `MAX_FRAME` by construction — `model.rs:196-199` says so out loud).
Both paths should refuse (`BadLength`-style) rather than clamp, or the
contract should demand the sizes and the glue should assert them.

---

## 8. LOW-MEDIUM — `THPDS` is pointed at physical address 0 with `TE` armed

**Where:** `driver.rs:136` — "point it nowhere rather than leaving whatever
the previous owner left in it", where "nowhere" is `0`.

Physical 0 is a real address. Nothing writes `TPPOLL_HPQ` today, so the HP
ring is never scanned — but the ring base is still programmed while the
transmitter is enabled, and any future errant `HPQ` write (or a part whose
doorbell semantics differ) sends the chip walking descriptors from page 0:
DMA into whatever lives there, no error bit anywhere. "Unused" should mean
"aliased onto the normal ring" or "a dummy descriptor that is never owned",
not "the null page".

---

## 9. LOW — `probe` can be called twice; the second call silently aliases
### the rings

**Where:** `rtl8169.rs:436` (`unsafe fn probe`, safety contract: "called
once (it claims the module's ring statics)").

The guard is prose. A second call constructs a second `Nic` over the *same*
`.bss` rings with its own cursors — the ownership protocol cannot survive
two drivers, and nothing structurally prevents it. A
`static CLAIMED: AtomicBool` with `compare_exchange` turns the prose into
code and deletes a whole class of future accident for one line.

---

## 10. LOW — throughput shape: 16-deep ring, one frame per lap, TX reclaim
### piggybacked on RX laps

**Where:** `RING_LEN = 16` (`rtl8169.rs:45`), one frame per `take_rx_frame`
by construction (`receive` returns the first good frame), TX reaped once per
lap at `rtl8169.rs:609`, lap cadence parked at ~10 ms when idle
(`NETPOLL_IDLE_PARK_US`).

Not a correctness flaw; a shape worth writing down before someone measures
and panics:

* Sustained RX above roughly one ring per park period overruns: the chip
  drops into `MPC`, which is only ever read on the stall dump — the overflow
  would be invisible in the counters.
* A TX-only workload reclaims at the *park* cadence (~100 fps ceiling while
  nothing arrives). TCP self-corrects (ACKs keep the loop awake); unidirectional
  UDP would bump into it silently. The doorbell (`TPPOLL`) fires per frame —
  fine — but completions wait for a receive lap to be harvested.

If either ever matters, the fix is reap-before-park in `take_rx_frame`'s
idle path (it already runs every lap) and a slightly deeper ring.

---

## 11. LOW — `stop`/`release` hand back a device that may still be DMAing

**Where:** `driver.rs:443-452`. `stop` writes `IMR=0`, `CR=0`, acks `ISR`
— correct, and the rings survive as advertised — but nothing waits for
in-flight descriptor DMA to quiesce before `release` returns the memory
interface to a consumer that may unmap or reuse it. Unused today (no caller
in the tree), so this is a contract gap to close with one bounded wait when
it gains a caller, or a doc line making the caller own the quiesce.

---

## 12. What survives review

For the fixer's benefit — these are load-bearing, and "tidying" them is how
the bugs above were born:

* **The W1C discipline** in `take_interrupts` (`driver.rs:329-335`):
  read, then write back exactly the observed bits. New bits raised between
  the read and the write survive. Correct, and the `FakeChip` enforces it
  (`model.rs:449-454`).
* **Zero-padding instead of length inflation** (`driver.rs:207-212`,
  `lib.rs:181-188`): a short frame is padded by writing zeroes, never by
  declaring a longer length over stale buffer contents. That is an
  information-leak fix that a "simplification" would silently undo.
* **The ownership fence order** in the glue (`rtl8169.rs:92-101`): payload
  words, `compiler_fence(SeqCst)`, `OWN` last; and `read_desc` reads
  `cmdstat` first. On x86 (TSO + coherent DMA) this is exactly right.
* **The `PHYAR` busy-bit asymmetry** (`mdio.rs`) — pinned by a test that
  fails if either direction is "unified". The module header's story about
  why is worth reading before touching it.
* **Bounded polls everywhere** (`POLL_ATTEMPTS`), the `reset` countdown in
  the model that punishes an unpolled reset, and the golden-register
  fixture that already caught one assumed-vs-measured `RCR` field.
* **`stall.rs`'s ordering** (`Backpressure` > `Blind` > `Silent`) is the
  part of the recovery machinery that is *not* implicated by §2 — the flaw
  is in the cursor repair, not in the arm logic or its tests.

---

## 13. Suggested fix order

1. §2 `resync_rx_cursor`: change `1..len` to `0..len`, unignore the repro.
   One line; removes a mis-order, a stranded-frame stall-generator, and the
   best candidate for the unexplained `mpc=0` `SILENT` arms.
2. §3 TX status + TX-stall arm: reads one extra word per reclaimed
   descriptor; turns a permanent silent outage into a logged, self-healing
   one; makes `tx_frames_sent` honest.
3. §5 move `CR_RE | CR_TE` after the config block in `init`; §8 alias
   `THPDS` onto the NP ring instead of 0. Both are two-line order changes
   with tests that already exist to pin them (`bringup.rs`'s ordering test
   grows one assertion).
4. §4 `IMR = 0` default with an explicit arm; §9 probe claim guard.
5. §6/§7/§11 contract tightening as touched.
