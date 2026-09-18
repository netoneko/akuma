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
