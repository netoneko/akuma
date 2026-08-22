# A closed non-blocking connect crashed the kernel: stale `connecting` handle

**Date:** 2026-08-22
**Status:** root-caused and fixed
**Symptom that started it:** a live kernel panic reported directly by the
user — `smoltcp-0.12.0/src/iface/socket_set.rs:103: handle does not refer to
a valid socket`, immediately followed by a secondary EL1 sync exception
(`WARNING: Kernel accessing user-space address!`) from the panic-handling
path itself.

## Executive summary

`socket_close()` closed a TCP socket and queued it in `net.pending_removal`
for GC, but never removed a matching entry from `net.connecting` — the list
`poll()` uses to enforce the non-blocking-connect timeout. If an app closes a
socket while it is still `SynSent` (the standard `connect()` →
`EINPROGRESS` → later `close()` idiom, or a socket simply abandoned when its
owning process/thread is torn down), the handle ends up **in both lists at
once**. On the very next `poll()`, `pending_removal`'s sweep frees the handle
from smoltcp's `SocketSet` first; the `connecting` sweep — which runs
immediately after it, in the *same* `poll()` call — still has the stale
entry and dereferences the now-freed handle, and smoltcp panics.

| # | Defect | Site | Fix |
|---|---|---|---|
| 1 | `socket_close` queued a handle for removal without also dropping it from `connecting` | `crates/akuma-net/src/smoltcp_net.rs`, `socket_close` | purge the `connecting` entry in the same step that queues `pending_removal` |

This is **deterministic**, not a rare race: any non-blocking `connect()`
closed before the handshake finishes hits it, given the right timing (the
peer's SYN-ACK, or lack of one, has to still be pending when `close()`
lands). The reporting user's log showed a `socket(TCP)` → `connect() =
EINPROGRESS` sequence immediately preceding the crash, alongside unrelated
thread-cleanup activity (`[Cleanup] Thread N recycled`, `[TERM] tid=...`) —
consistent with a process/thread teardown closing a socket that was still
mid-handshake.

## Why it crashed, precisely

Two bookkeeping structures track a TCP handle's lifecycle outside the
smoltcp `SocketSet` itself, both `Vec<(SocketHandle, u64)>` fields on
`NetworkState`:

- `pending_removal` — sockets queued for GC after `close()`. `poll()` sweeps
  it every call: once a socket's state reaches `Closed` (or a GC timeout
  fires), the handle is `sockets.remove()`d from the *live* `SocketSet` and
  dropped from the list.
- `connecting` — sockets in `SynSent`, so `poll()` can enforce
  `CONNECT_TIMEOUT_US`. An entry leaves this list once the socket is no
  longer shaking hands (established, reset, or closed).

`socket_close()` (`crates/akuma-net/src/smoltcp_net.rs`) was the only
producer of `pending_removal` entries:

```rust
pub fn socket_close(handle: SocketHandle) {
    ...
    with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        socket.close();
        net.pending_removal.push((handle, (runtime().uptime_us)()));
        // connecting was never touched here
    });
}
```

smoltcp's `tcp::Socket::close()` transitions `SynSent` straight to `Closed`
(`tcp.rs:1006`, `State::SynSent => self.set_state(State::Closed)`). So
closing a socket that is still mid-handshake does two things at once: it
satisfies `pending_removal`'s very next sweep condition (`state ==
tcp::State::Closed`), **and** it leaves a now-meaningless entry sitting in
`connecting`, because nothing ever removed it from there.

`poll()` runs both sweeps back to back, in this order (`smoltcp_net.rs`,
~lines 1235–1285):

```rust
// 1. pending_removal sweep
while i < net.pending_removal.len() {
    let (handle, added_at) = net.pending_removal[i];
    ...
    let state = net.sockets.get::<tcp::Socket>(handle).state();
    if state == tcp::State::Closed || timed_out {
        ...
        net.sockets.remove(handle);          // <-- slot freed HERE
        net.pending_removal.swap_remove(i);
    } else { i += 1; }
}

// 2. connecting sweep, immediately after, same poll() call
while i < net.connecting.len() {
    let (handle, started_at) = net.connecting[i];
    if !is_valid_handle(handle) { net.connecting.swap_remove(i); continue; }
    if net.sockets.get::<tcp::Socket>(handle).state() != tcp::State::SynSent {
        //   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
        //   panics: `handle` was just removed from the SocketSet above
        net.connecting.swap_remove(i);
        continue;
    }
    ...
}
```

`is_valid_handle()` only bounds-checks the handle's index against
`MAX_SOCKETS` — it cannot know whether that index's *slot* was just freed
from the live `SocketSet`, so it does not guard this at all. smoltcp's
`SocketSet::get` has no fallible variant; an already-removed handle panics
unconditionally:

```rust
// smoltcp-0.12.0/src/iface/socket_set.rs
pub fn get<T: AnySocket<'a>>(&self, handle: SocketHandle) -> &T {
    match self.sockets[handle.0].inner.as_ref() {
        Some(item) => T::downcast(&item.socket).expect(...),
        None => panic!("handle does not refer to a valid socket"),
    }
}
```

The secondary EL1 exception the user's log showed right after the Rust panic
(`ELR`/`FAR` pointing at a user-space address, "Kernel accessing user-space
address!") is collateral from the panic-handling path itself faulting while
unwinding — not a second, independent bug. It was not investigated further;
the panic is what needed fixing.

## The fix

`socket_close()` now purges the `connecting` entry in the same step that
queues the handle for removal, so the two lists can never disagree about
whether a handle is still live:

```rust
pub(crate) fn purge_connecting(connecting: &mut Vec<(SocketHandle, u64)>, handle: SocketHandle) {
    connecting.retain(|(h, _)| *h != handle);
}

pub fn socket_close(handle: SocketHandle) {
    ...
    with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        socket.close();
        net.pending_removal.push((handle, (runtime().uptime_us)()));
        purge_connecting(&mut net.connecting, handle);
    });
}
```

`socket_close` is the *only* place that ever pushes into `pending_removal`
(confirmed by grep), so this one call site is sufficient to keep the
invariant everywhere — no other producer needs the same fix.

## Verification

The crashed VM's state was already corrupted by the panic by the time this
was diagnosed, so there was no live A/B against a running kernel — the fix
was verified statically instead:

- Read the exact smoltcp 0.12.0 source for `SocketSet::get`/`remove` to
  confirm the panic message and the no-fallible-accessor claim.
- Confirmed `tcp::Socket::close()`'s `SynSent -> Closed` transition in
  smoltcp's own `tcp.rs`.
- `cargo check` (both `-p akuma-net` and the full kernel) clean.
- `purge_connecting` extracted as a pure function over plain
  `Vec<(SocketHandle, u64)>` data (mirroring the existing pattern —
  `connect_step`/`bind_port_for` in `crates/akuma-net/src/socket.rs` — of
  pulling policy out of the `with_network` closures so it is host-testable
  without a real smoltcp `SocketSet`), plus a `#[cfg(test)]`
  `test_socket_handle(idx)` constructor mirroring the existing
  `socket_handle_index` transmute. Three new tests in
  `crates/akuma-net/src/tests.rs::connecting_bookkeeping_tests` pin the
  invariant: closing a handle removes only its own `connecting` entry,
  leaves unrelated entries alone, and is a no-op for a handle never in the
  list. `cargo test -p akuma-net` — 48 passed, 0 failed.

This is a genuine gap: before this fix, `crates/akuma-net`'s test suite had
no coverage at all for `net.connecting`/`net.pending_removal` — every
existing test was a pure-function test over state-machine predicates
(`connect_step`, `tcp_recv_ready`, `backlog_handle_is_live`), and this
particular interaction lived entirely inside `with_network` closures against
the live `NETWORK` global and a real `SocketSet`, which nothing had made
host-testable before.

## Fixed in

- `crates/akuma-net/src/smoltcp_net.rs` — `purge_connecting`, called from
  `socket_close`; `test_socket_handle` (`#[cfg(test)]` handle constructor)
- `crates/akuma-net/src/tests.rs` — `connecting_bookkeeping_tests` (3 tests)

## Background

- [`AKUMA_NET_ISSUES.md`](AKUMA_NET_ISSUES.md) — the broader native-stack
  investigation log this bug is part of.
- [`../reference/subsystems/networking.md`](../reference/subsystems/networking.md)
  § "Socket lifetime" — `pending_removal`/GC and the rest of the socket
  teardown machinery this bug lived in.
- [`../runbooks/debug-network.md`](../runbooks/debug-network.md) — general
  native-stack debugging.
