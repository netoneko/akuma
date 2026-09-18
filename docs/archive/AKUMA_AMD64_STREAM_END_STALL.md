# amd64: a chunked stream's **last** chunk can arrive 60 s late

**Found:** 2026-09-18, chasing `nca` reporting
`[custom stream error: error decoding response body]` against a z.ai endpoint.
**Status: open, reproduced, not root-caused.** Reproduced against a *local*
server on nca's exact client stack — no third-party API, no credentials, and
nothing to get rate-limited by.

---

## 1. The symptom, and what it is not

`nca` streams SSE from an OpenAI-compatible endpoint and intermittently fails
with `error decoding response body` — hyper's error for a chunked body that
ended before its terminating chunk.

It is **not** the delayed-*first*-byte family
(`SOCKET_DELAYED_FIRST_BYTE_HANG.md`, four defects, all fixed): first byte here
is fast and healthy. It is the **other end of the stream**.

## 2. The reproduction

```sh
# laptop
python3 scripts/net_delay_server.py --port 18080

# box (nettest-reqwest = tokio + hyper 1.x + reqwest 0.12 + rustls, nca's stack)
nettest-reqwest stream http://<laptop>:18080/sse/1/3
```

`/sse/1/3` sends three SSE events one second apart, then `data: [DONE]`
immediately after the third. Two consecutive runs, same command:

```
run 0: chunk n=4 at=3039ms  len=14  gap=0ms      total_ms=3040    <- correct
run 1: chunk n=4 at=63043ms len=14  gap=60015ms  total_ms=63043   <- 60 s late
```

and an earlier `/sse/1/5`: last data chunk at **5034 ms**, `last_byte` at
**65100 ms**.

**Hit rate, six consecutive runs of `/sse/1/3`:**

```
run 0: total_ms=3038   ok
run 1: total_ms=63073  STALL
run 2: total_ms=63268  STALL
run 3: total_ms=3066   ok
run 4: total_ms=63209  STALL
run 5: total_ms=63119  STALL
=> 4/6 stalled
```

**The stalled chunk is the 14-byte terminator**, which the server writes in the
same breath as the preceding event. The body is *complete* and *correct* when it
finally lands (`RESULT ok … body=89 chunks=4`) — nothing is lost, it is only
late.

Two numbers matter, and both point away from jitter:

- **The stall is ~2 in 3**, so a repro takes seconds, not a soak.
- **The delay is a timer.** Across six captures the *gap* from the previous
  chunk is 60.0 s to within ~200 ms (totals 63.07/63.12/63.21/63.27 s on a
  stream whose real content ends at ~3.0 s). Nothing in a scheduler or a poll
  loop is that repeatable; something waits a fixed 60 s and then delivers.

That is enough to explain the `nca` symptom: any client whose read timeout is
under 60 s aborts the body mid-stream, and hyper reports exactly
`error decoding response body`.

## 3. What has been ruled out, with evidence

| candidate | verdict |
|---|---|
| DNS / connected-UDP (`AKUMA_AMD64_DNS_CONNECTED_UDP.md`) | fixed and verified — `resprobe2` passes every step, errno=0 throughout |
| TLS, and HTTPS streaming generally | a `stream` of a real HTTPS site returned `first_byte=209ms` and five chunks with 0–3 ms gaps |
| the 30 s blocking-recv cap (`SOCKET_DELAYED_FIRST_BYTE_HANG.md`) | fixed in the **shared** `akuma-net` crate (`socket.rs:195`, `rcvtimeo_us: Option<u64>`), so amd64 has it — and the stall is 60 s, not 30 |
| `STUCK_WAITER_US = 60_000_000` (`akuma-syscalls-glue/src/sync.rs:494`) | **wrong 60 s.** It is a futex *diagnostic* threshold; it unblocks nothing. Matching the number was not enough — the code had to be read |
| long idle windows as such | `/gap/1/20` (a 20 s idle mid-stream) completes at exactly 21036 ms, no stall |
| slow steady streaming | the 1 s-apart events themselves arrive on time, gaps 977–1043 ms |

So it is specific to the **final** segment of a stream, and intermittent.

## 4. The leading hypothesis, and why it fits

**This target has no NIC interrupt.** `amd64/src/net.rs:433` states it plainly:
"with no NIC interrupt on this target … an arriving packet waits for the next
tick", and the MMIO IRQ the command line already names
(`virtio_mmio.device=512@0xfeb00000:5`) is not registered. What covers the gap
is `NETPOLL_IDLE_PARK_US` — one LAPIC tick — plus a **doorbell**
(`wake_netpoll`) rung by anything that hands the stack *local* work.

A client parked waiting for the last chunk generates no local work, so nothing
rings the doorbell; delivery depends entirely on the polling lap. A one-tick
park should still notice within ~10 ms, so the tick cadence alone does not
explain 60 s — **which is the open question**: what makes the lap not happen, or
not see the segment, for a minute?

Two things worth checking first, in this order:

1. **The known RTL8169 receive stall.** `docs/runbooks/amd64-bare-metal-loop.md`
   § "Reading the probe" records `rx` stuck at exactly 16 (`RING_LEN`) as a known
   stall on this NIC, and the box's own description opens with "a NIC that needs
   restarting". A stalled RX ring that recovers on a timer is the shape of this
   bug. Capture `[rtl]`/`[NICSTAT]` across a stalled run — but **drain `dmesg -c`
   first**, because ssh session logging overwrites the 64 KiB ring in minutes and
   an empty grep afterwards proves nothing.
2. **Whether the peer is retransmitting.** 60 s is close to the cumulative TCP
   RTO backoff ladder (1+2+4+8+16+32 = 63 s). A `tcpdump` on the *server* side
   across a stalled run separates "Akuma never got the segment until a
   retransmit" from "Akuma had it and did not deliver it". That single capture
   decides which half of the stack to look in, and nothing else here does.

## 5. Where this connects

Three open symptoms on this target are all "an edge that should have woken a
reader did not, and something else eventually did":

- this stall;
- `git clone` hanging with the transport helper started but never fed
  (`AKUMA_FROM_SCRATCH.md` §3.1, third cause);
- the runbook's OPEN "an interactive ssh session needs one extra event to
  register the previous one", whose mechanism (a) is *lost wakeup*.

They may be one bug or three. Nothing here establishes that they are one — it is
recorded because the shapes rhyme and a fix for one is worth testing against the
other two.

## 6. Background

- [`SOCKET_DELAYED_FIRST_BYTE_HANG.md`](SOCKET_DELAYED_FIRST_BYTE_HANG.md) — the
  first-byte half, resolved; the probes here are its regression harness.
- [`NGINX_MISSING_SYSCALLS.md`](NGINX_MISSING_SYSCALLS.md) § "Why `http` mode
  couldn't get clean numbers" — an **unresolved** 2026-08 sighting of the same
  family: "Akuma's TCP stack delivering the FIN late for this specific
  short-lived-connection shape". Closest prior art, and still open.
- [`../../userspace/nettest/README.md`](../../userspace/nettest/README.md) — the
  probe, its modes, and the same-binary-on-Linux A/B that this investigation
  could **not** run (no Docker daemon on the laptop; the box's Ubuntu side was
  down). Running it is the first thing to do with a working Linux host: it would
  turn "we think this is the kernel" into proof in one command.
- [`AKUMA_AMD64_DNS_CONNECTED_UDP.md`](AKUMA_AMD64_DNS_CONNECTED_UDP.md) — the
  resolver fixes this sits downstream of.

---

## 7. 2026-09-18: the terminator identified, and candidate 1 patched

**Status: candidate 1 is fixed. The 60 s stall is not confirmed closed** — that
needs a run on the box, and the two captures §4 asks for are still the way to
prove it.

### 7.1 The 14-byte chunk is `data: [DONE]\n\n`

§2 records the stalled chunk as "the 14-byte terminator" without saying what it
contains. It is the SSE sentinel: `"data: [DONE]\n\n"` is exactly 14 bytes. That
matters because it is *not* part of the answer — every byte of the model's
response has already arrived by then, and the only thing waiting on it is a
client that treats the sentinel as the end of stream.

Two consequences:

* It explains why only the **last** segment stalls, and why the body is always
  "complete and correct when it finally lands". Nothing is lost because nothing
  was still owed.
* It is separable from the kernel bug. `userspace/meow` now ends a stream on a
  non-null `choices[0].finish_reason` as well as on `[DONE]`
  (`src/api/client.rs`, `parse_streaming_line`), so the answer is complete and
  returned before the late chunk is even due. `nca` cannot be fixed this way —
  hyper is failing at the *chunked-body* layer, below SSE — so for nca the
  kernel-side defect is still the whole story.

### 7.2 The receive-stall watchdog had stopped being a watchdog

§4's candidate 1 was "the known RTL8169 receive stall … a stalled RX ring that
recovers on a timer is the shape of this bug". The recovery exists —
`Rtl8169Device::on_stall` → `Nic::kick_receiver` — and it was **unreachable in
practice on this target**, which no one had noticed because the one time it was
seen firing was during bring-up.

`crates/akuma-net-nic/src/rtl8169.rs` armed it off a lap count:

```rust
const STALL_LAPS: u32 = 2_000_000;   // "a second or two of genuine silence"
```

That comment was true when it was written and is not true now.
`amd64/src/net.rs`'s `netpoll_daemon` parks for one LAPIC tick whenever a lap
moved nothing (`NETPOLL_IDLE_PARK_US`, added for the idle-CPU fix), so an idle
receive lap runs about **100 times a second, not hundreds of thousands**.
2,000,000 of them is **five and a half hours**. After boot the watchdog could
not fire.

The `[rtl] STALL #1 after 2000000 idle laps` line quoted in §4 is from the
bring-up window, while the loop was still busy-spinning — which is exactly why
the calibration breaking afterwards was invisible.

**Fix:** the horizon is wall-clock now (`STALL_QUIET_US = 5_000_000`), measured
from the last frame that actually came off the ring, with the lap count kept
only as the pre-clock fallback. A lap count cannot express "two seconds" on a
loop whose rate is a scheduling decision.

Five seconds rather than the two the lap count meant to express: `kick_receiver`
resets the ring cursor, so a frame the chip has written and the driver has not
yet read is lost across it. The crate's own bring-up note — "on any real LAN
this climbs within seconds from broadcast traffic alone" — is what makes five
seconds still a stall rather than an idle link.

### 7.3 Why this is a candidate and not yet a root cause

The mechanism it would complete: the ring goes dry, an arriving segment is
dropped for want of a descriptor (`MPC` counts it), the peer retransmits on the
standard backoff ladder (1+2+4+8+16+32 = 63 s, against a measured 63.0–63.3 s
total on a stream whose content ends at ~3.0 s), and by the time a retransmit
lands the driver has drained the ring. That fits the timing, the ~2-in-3 hit
rate, and "only the final segment" — but **none of it is measured**, and a
5.5-hour watchdog cannot by itself produce a 60 s delay. The watchdog being
broken is a real defect either way; whether it is *this* defect is what §4's two
captures decide.

Check `MPC` (it is already in `Nic::snapshot`) in the stalled run: a non-zero
missed-packet count is the difference between "the ring dropped it" and "it
never arrived".
