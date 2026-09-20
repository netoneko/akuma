# `bind()` never returned `EADDRINUSE` — found via meow's litter-raft coordinator race

**Status: FIXED 2026-09-20.** `crates/akuma-net/src/socket.rs`, shared by both
kernels — this is not an amd64-vs-aarch64 divergence, it is an Akuma-vs-Linux
one that happened to be found on the amd64/Firecracker path first.

## Symptom

`userspace/meow`'s litter-raft coordinator election
(`docs/LITTER_RAFT_LOOP.md` in that submodule) has each `meow litter live`
resident try `TcpListener::bind("127.0.0.1:7700")` at startup: whoever's
`bind()` succeeds becomes the hub (raft/serve thread) for as long as it lives,
and everyone else's failing `bind()` is how they learn to become a follower
instead. Three residents deployed under `herd` on an Akuma-amd64 Firecracker
guest (Ryzen host, `docs/reference/firecracker-amd64/`) each logged, repeatedly:

```
[live] tiger holds the hub at 127.0.0.1:7700 (won the bind race) — raft thread up
[live] jaguar holds the hub at 127.0.0.1:7700 (won the bind race) — raft thread up
[live] panther holds the hub at 127.0.0.1:7700 (won the bind race) — raft thread up
```

All three, in the same ~45 s window, each independently believing it holds the
one listener — not a one-time race at boot, but a sustained flap: `herd`
restarting each of them multiple times, an operator `meow litter peers` query
getting `hub at '127.0.0.1:7700' went silent before answering` on roughly half
of attempts, and no query ever seeing more than one or two names in the roster
at once. A `TcpListener::bind()` design that relies on the OS enforcing "only
one process can hold a given address:port" cannot converge if the OS does not
enforce that.

## Root cause

`crates/akuma-net/src/socket.rs`'s `socket_bind` and `socket_listen` never
checked the socket table for a port already claimed by another live socket.
`socket_bind` set `sock.bind_port = Some(port)` on the caller's own table
entry and nothing else; `socket_listen` built a fresh
`KernelSocket::new_listener(port, backlog)` the same way. Neither scanned the
other `MAX_SOCKETS` slots. So three separate processes each got their own
independent smoltcp listening handle bound to port 7700, and which handle
smoltcp handed a given incoming (loopback) connection to next was arbitrary —
observed as "leadership" bouncing between residents and the hub going
intermittently unreachable, when in fact there was never a single hub to
begin with.

Real Linux's `bind()` returns `EADDRINUSE` in exactly this situation; Akuma's
never did, on either architecture — `amd64/src/sock.rs::sys_bind` and the
AArch64 syscall path both call straight into this same `akuma_net::socket`
function, so the gap was never arch-specific, only *found* on amd64 because
that is where three-resident litter deployment happened first.

## Fix

`socket_bind` now scans the table (still under the single `SOCKET_TABLE`
spinlock `with_table` already held, so there is no check-then-set race) for
another live socket already occupying the requested port **in the same
protocol's namespace** — a new `occupies_port` helper treats TCP (`Stream`/
`Listener`) and UDP (`Datagram`) as separate spaces, matching Linux, so a TCP
and a UDP socket may still share a port number. A conflict returns
`EADDRINUSE`. Port `0` ("pick one for me") is exempt, since
`alloc_ephemeral_port` is what keeps those unique; only an explicit port is
checked. `socket_listen` needed no change — it can no longer be reached by a
second bind on the same port, since the conflict is caught earlier at
`socket_bind`.

## Verification

- Host tests: `cargo test -p akuma-net` — 40/40 passing, no regressions
  (`bind_port_for`/ephemeral-port logic untouched).
- Both kernels still build: `cargo build --release` (AArch64) and
  `cargo build -p akuma-amd64 --target x86_64-unknown-none --release`.
- Live, on the actual failing configuration (three `meow litter live`
  residents under `herd`, Akuma-amd64 Firecracker guest on Ryzen): after the
  fix, a fresh boot's logs show exactly the intended shape — two residents
  restart a few times and then log `joined the litter at 127.0.0.1:7700 (hub
  already up)` instead of re-claiming the bind, while the third keeps holding
  it without ever needing to "re-win":

  ```
  [live] tiger holds the hub at 127.0.0.1:7700 (won the bind race) — raft thread up   (x3, at boot)
  [live] tiger joined the litter at 127.0.0.1:7700 (hub already up)                   (converged)

  [live] jaguar holds the hub at 127.0.0.1:7700 (won the bind race) — raft thread up  (x4, at boot)
  [live] jaguar joined the litter at 127.0.0.1:7700 (hub already up)                  (converged)

  [live] panther holds the hub at 127.0.0.1:7700 (won the bind race) — raft thread up (the actual leader — never needs to re-win)
  ```

  The remaining startup noise (a few `ConnectionRefused`/`went silent` lines
  before each resident's *own* first bind attempt resolves) is expected and
  matches the design: nobody knows who holds the hub until either their own
  `bind()` succeeds or fails.

## Background

- `userspace/meow/docs/LITTER_RAFT_LOOP.md` (submodule) — the coordinator
  design this bug broke: "whoever wins the bind race" as the mutual-exclusion
  primitive.
- [`REDIS_END_TO_END.md`](REDIS_END_TO_END.md) §2 — an earlier, unrelated
  `socket_bind` bug in the same function (`bind(0.0.0.0:0)` on TCP storing
  literal port 0), fixed 2026-08-16. That fix and this one are independent;
  worth knowing both touched the same ~20-line function within five weeks.
- [`docs/reference/firecracker-amd64/README.md`](../reference/firecracker-amd64/README.md)
  — the Ryzen/Firecracker environment this was found on.
