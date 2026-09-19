# amd64: meow's requests to the model endpoint stall and fail — 2026-09-19

Stability grade: **C** (live investigation; one fix landed and measured, the
remaining defect characterised but not fixed).

`meow` on the bare-metal box, against `api.z.ai`, fails with
`(Server returned error) retry 1..5` and sometimes never returns at all. This
records what it is, what it is not, the fix that measurably helped, and the
defect still standing.

## The short version

Two independent problems, stacked:

1. **The socket wait held the Big Kernel Lock** (`blocking_relax` was a plain
   `yield_now`), so a request whose server thinks for seconds starved the
   netpoll daemon that would deliver its reply. **Fixed**; it cut failures in
   half. §4.
2. **A live connection reports EOF.** After the TLS handshake completes and the
   request is written, the first `recv` returns **0** — "peer closed" — while
   the server is merely quiet. `embedded-tls` turns a short read into
   `TlsError::IoError`, which `meow` prints as "Server returned error". **Not
   fixed.** §5.

## 1. The measurement

Ten identical one-line prompts (`meow --no-tui --debug -c "Say only: ping N"`),
each capped at 75 s, run back to back from `/src/github.com/netoneko/akuma`:

| arm | never completed | `TlsError(IoError)` | clean runs |
|---|---|---|---|
| baseline (kernel `811d0401`) | **3** | **29** | 1 |
| kernel fix (`70213492`) | **1** | **14** | 5 |
| kernel fix + userspace retry backoff | 2 | 25 | 4 |

Successful runs took 2–39 s for a prompt that costs ~2 s when it goes well. The
third arm is inside the noise of the second, which is the honest reading: **the
userspace backoff bought nothing measurable** and is not the answer.

## 2. What it is NOT

Each of these was checked rather than assumed, and each would have been a
plausible place to spend a day:

- **Not the server.** The exact 10 126-byte request the box gave up on, lifted
  off the box byte-for-byte (`/src/.../tmp/.meow_request.json`, staged there by
  `meow` itself) and replayed from a laptop, returns **HTTP 200 in 4.7 s**.
- **Not malformed JSON.** That same body parses, and its `tool_calls` /
  `tool_call_id` pairing is correct — assistant-with-`tool_calls` followed by
  one `tool` message per call.
- **Not request size.** The failing request is 10 KB. Tool output larger than
  the in-memory cap is spilled to a file and replaced by a note, so the
  conversation does not grow the way it appears to.
- **Not chunked transfer-encoding.** `libakuma-tls` refuses chunked bodies, and
  `api.z.ai` does answer `Transfer-Encoding: chunked` — **over HTTP/1.1**.
  `meow` speaks **HTTP/1.0 with `Connection: close`**, so the server never
  chunks for it. (Reproducing with `curl --http1.1` and concluding otherwise is
  the trap here.)
- **Not the TLS stack in general, and not the host.** A freshly built `hget` —
  same `libakuma-tls`, same patched `embedded-tls` — gets a clean `401` from
  **the same host** 5/5, and clean bodies from github 5/5.

The one control that *is* conclusive: on one host, one TLS stack, one NIC, an
**instant** response never fails and a **~5-second** response fails about half
the time. The variable is the silent window, not the peer.

## 3. The instrument

`embedded-tls` collapses "the socket errored" and "the socket ended early" into
the same `TlsError::IoError`, which is why the failure said nothing for hours.
Six lines in `libakuma-tls`'s transport separate them:

```rust
Ok(0) => { /* print "read returned 0 (EOF) after N empty laps" */ }
Err(ref e) => { /* print the ErrorKind */ }
```

Result, across six runs: **20 × `read returned 0 (EOF) after 0 empty laps`**,
and not a single error kind. "0 empty laps" means the *first* read after the
request was written returned zero — no `WouldBlock` first, no waiting.

## 4. Fix that landed: the socket wait must drop the BKL

`akuma-net`'s park arm states the contract in its own comment:

> Under shared-kernel SMP this DROPS the Big Kernel Lock across the wait (a
> plain `yield_now` would spin holding it, freezing every peer core — **the
> meow->LLM `connect`+recv wedge**)

and `amd64/src/net.rs` supplied exactly that plain `yield_now`. `yield_now`
opens a 4-`pause` window, **re-takes the lock**, and switches with it held —
correct for a yield, useless for a wait. So a waiter with nothing to do sat on
the lock while the netpoll daemon, which needed it, could not run.

The fix points `blocking_relax` at `sched::allow_tick` — drop the BKL,
`sti; hlt`, take it back — the primitive added 2026-09-17 for the sibling case
(the netpoll daemon parking BKL-held). Measured effect: the table in §1, and
**zero `[BKL] stuck` lines** in a 4-vCPU Firecracker boot that previously
produced storms.

### 4a. The version of that fix that bricked the box

The first attempt called `allow_tick` unconditionally. `allow_tick` only halts
when `lapic::timer_running()`; with the timer stopped it falls back to
`sti; nop; cli`, which **keeps the lock held** — strictly worse than the
`yield_now` it replaced. The boot path is precisely where the timer is off
(`boot::self_tests` stops it, restarting it only around the SNTP sync), and the
clock bootstrap resolves `pool.ntp.org` in that window. The box came up printing

```
[BKL] stuck: owner=2 waiter=3 tag=502 (aff0+1)     <- forever, owner never changes
dns: 1.1.1.1: no reply before timeout
clock: retry: could not resolve pool.ntp.org via 1.1.1.1
```

and served nothing. `tag=502` is `HOLD_TAG_IDLE`: the tag is only stamped at
syscall entry, in the idle loop and by netpoll, so a boot-time kernel worker
shows whatever its core last set — **read the tag as "unattributed", not as
"the idle thread"**.

The shipped version tests `timer_running()` and falls back to `yield_now`, so
the worst case is the old behaviour rather than a new one.

Three process lessons, all of them written down somewhere already:

- **The fast lane exists for this.** `amd64_trials.py --local-only` boots the
  kernel in ~25 s and the box's Firecracker rig does it under KVM at 4 vCPU.
  Going straight to `/boot` skipped both and cost a physical rescue.
- **Two variables, one experiment.** The bricking kernel was built on a laptop
  whose nightly is three weeks older than the box's. Build on the box (or with
  the box's toolchain through the musl loader, `amd64-bare-metal-loop.md`
  § "Staging or repairing it") so the only difference is the patch.
- **`e2fsck` after a hard power cycle.** Three forced resets left real damage on
  `sdb1` (block/inode bitmap differences, wrong free counts) and the *known-good*
  kernel then misbehaved too — which reads exactly like "the good kernel is
  broken as well" and sends you hunting the wrong thing.

## 5. The defect still standing: EOF on a live connection

Sequence, from one failing run (`--debug` plus the transport instrument):

```
[meow:debug] POST https://api.z.ai/api/coding/paas/v4/chat/completions
[meow:debug] resolving api.z.ai:443
[meow:debug] connecting to 8.217.233.95:443
.] waiting[tls-diag] read returned 0 (EOF) after 0 empty laps
[meow:debug] stream error: TlsError(IoError)
 (Server returned error) retry 1 …                 <- a NEW connection each time
```

So: DNS fine, connect fine, handshake fine, request written, first read of the
response returns 0. Five fresh connections in a row can do this, then the sixth
works and the answer arrives 38 s after the prompt.

`socket_recv` has exactly three ways to hand back `Ok(0)`:

| path | `crates/akuma-net/src/socket.rs` | plausible here? |
|---|---|---|
| `recv_shutdown` latch | ~1486 | set only by `shutdown(2)`, cleared in all four constructors — no evidence, but a **stale latch on a recycled slot** would produce precisely "0 empty laps" and deserves a direct check |
| `!may_recv() && state == Closed && was_connected` | ~1534 | would return `ECONNRESET`, not 0 |
| `!may_recv() && tcp_reached_established(state)` | ~1541 | the likely one: smoltcp thinks the peer closed its write half |

So the question is why `may_recv()` is false on a connection that just completed
a handshake. Two candidates, in the order worth testing:

1. **We tear it down ourselves.** `meow` opens and closes a connection per
   request; if a `close` frees a smoltcp handle a newly created socket has
   already been handed, the new connection is aborted moments after it is
   established. This predicts a dependence on how many connections the session
   has already made — testable by running the probe against a fresh boot versus
   a long-lived one, and by watching `SOCKETS_LIVE` / the GC
   (`SOCKET_GC_TIMEOUT_US`, 30 s) across the failures.
2. **The peer really does close.** Testable with a packet-level view from the
   Ubuntu side (the box's own NIC is the one under test, so capture from a tap
   or a mirror rather than from the box).

An inactivity timeout is *not* a candidate: `smoltcp`'s `set_timeout` is
deliberately unused (`smoltcp_net/consts.rs` explains why, naming this exact
failure class), and only `SynSent` is bounded.

Note the family resemblance to
[`SOCKET_DELAYED_FIRST_BYTE_HANG.md`](SOCKET_DELAYED_FIRST_BYTE_HANG.md), where
the dominant bug on AArch64 was a socket in `SynSent` reporting read-closed —
four separate defects wearing one symptom. This is the same shape on a target
that shares the socket crate but not the wait wiring, so **check whether each of
those four fixes is reachable from `amd64/src/sock.rs`** before assuming this is
new.

## 6. State of the box at the end of this session

- `/boot/akuma-amd64` = the fixed kernel (`70213492`), booted, **775 passed, 0 failed**.
- `/boot/akuma-amd64.good` = `811d0401`, the pre-fix kernel that ran for hours —
  deliberately **not** overwritten with a kernel one boot old.
- `/bin/meow` = an **instrumented** build that prints `[tls-diag]` lines;
  `/bin/meow.bak` is the previous one. Rebuild without the instrument
  (`mbuild`) once §5 is closed.
- `sdb1` was `e2fsck`ed clean after the forced resets.

## Background

- [`AKUMA_AMD64_BKL_NETWORKING.md`](AKUMA_AMD64_BKL_NETWORKING.md) — the audit
  that found `blocking_relax`, the unconditional BKL at syscall entry, and the
  missing per-syscall opt-out on this target.
- [`SOCKET_DELAYED_FIRST_BYTE_HANG.md`](SOCKET_DELAYED_FIRST_BYTE_HANG.md) — the
  AArch64 history of "a slow first byte kills the connection".
- [`../runbooks/amd64-bare-metal-loop.md`](../runbooks/amd64-bare-metal-loop.md)
  — the box, the fast lane that should have caught §4a, and the staging
  procedure that builds with the box's own toolchain.
