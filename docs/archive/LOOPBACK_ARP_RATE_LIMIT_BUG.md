# Loopback Test Crash: smoltcp Global ARP Rate Limiter

## Symptom

The network self-test (`network_tests::run_tests`) panics with a loopback
connection timeout when an external SSH client connects to the VM before the
test completes:

```
[SmolNet] IP: 10.0.2.15/24
[NetTest] DHCP settled
[NetTest] Testing loopback connection (127.0.0.1:9999)...
[NetTest] Connection failed. Client: Some(SynSent), Server: Some(Listen)

!!! PANIC !!!
Location: src/network_tests.rs:92
Message: Network Test Failed: Loopback connection timeout
```

The client socket never leaves `SynSent` and the server socket stays in
`Listen`, meaning the TCP SYN was never transmitted.

## Root Cause

A three-way interaction between DHCP address reconfiguration, an incoming SSH
SYN, and smoltcp's **global** ARP neighbor-discovery rate limiter.

### 1. DHCP flushes the neighbor cache

`Interface::update_ip_addrs()` unconditionally calls
`flush_neighbor_cache()`, clearing every learned MAC entry — including the
gateway (10.0.2.2).  This happens inside `smoltcp_net::poll()` when the DHCP
Configured event fires:

```rust
// smoltcp 0.12.0 — iface/interface/mod.rs:361-363
pub fn update_ip_addrs<F: FnOnce(&mut Vec<IpCidr, IFACE_MAX_ADDR_COUNT>)>(&mut self, f: F) {
    f(&mut self.inner.ip_addrs);
    InterfaceInner::flush_neighbor_cache(&mut self.inner);   // <-- full flush
    ...
}
```

### 2. An SSH SYN triggers gateway ARP with a global rate limit

During the first `poll()` of the test loop, `iface.poll()` processes ingress
first.  The early SSH SYN arrives via VirtIO.  smoltcp generates a TCP RST
but needs the gateway's MAC (10.0.2.2) to send it.  The cache is empty, so
`lookup_hardware_addr()` sends an ARP request for 10.0.2.2, then calls:

```rust
// smoltcp 0.12.0 — iface/interface/mod.rs:1114-1116
self.neighbor_cache.limit_rate(self.now);
Err(DispatchError::NeighborPending)
```

`limit_rate()` sets `silent_until = now + 1 000 ms` — a **global** cooldown
on **all** neighbor discovery for **any** IP address:

```rust
// smoltcp 0.12.0 — iface/neighbor.rs:170-172
pub(crate) fn limit_rate(&mut self, timestamp: Instant) {
    self.silent_until = timestamp + Self::SILENT_TIME;   // 1 second
}
```

### 3. The loopback ARP is suppressed

In the same `iface.poll()` call, `socket_egress` runs next.  The test's
client socket (SynSent) tries to send its SYN to 127.0.0.1, which also
requires ARP resolution (127.0.0.1 is not in the flushed cache).  But
`neighbor_cache.lookup(127.0.0.1)` now hits the global rate limit:

```rust
// smoltcp 0.12.0 — iface/neighbor.rs:150-168
pub(crate) fn lookup(&self, protocol_addr: &IpAddress, timestamp: Instant) -> Answer {
    if let Some(&Neighbor { expires_at, hardware_addr }) = self.storage.get(protocol_addr) {
        if timestamp < expires_at { return Answer::Found(hardware_addr); }
    }
    if timestamp < self.silent_until { Answer::RateLimited }   // <-- hit
    else { Answer::NotFound }
}
```

It returns `RateLimited` instead of `NotFound`.  **No ARP request for
127.0.0.1 is ever dispatched.**  The error propagates back to
`socket_egress`, which calls `neighbor_missing()` on the socket — silencing
it for 1 second.

### 4. The test times out

The socket is silenced for 1 second.  On every subsequent poll,
`egress_permitted()` returns `false` because `has_neighbor(127.0.0.1)` is
still false (no ARP was ever sent) and `timestamp < silent_until`.

The test's 1 000 iterations with `yield_now()` complete in roughly 100 ms —
well before the 1-second silence expires.  The SYN is never sent and the
test panics.

### Why it works without the SSH SYN

Without external traffic, `socket_egress` is the **first** thing to trigger
ARP.  It sends the ARP for 127.0.0.1 directly (`lookup` returns `NotFound`,
not `RateLimited`).  The ARP request goes through the loopback queue,
resolves within 2–3 polls, and the SYN is transmitted immediately.  The
global rate limit is set *after* the loopback ARP is already dispatched, so
it has no effect.

## Timeline Comparison

### Normal boot (no early SSH)

| Poll | Ingress | Egress | Result |
|------|---------|--------|--------|
| 1 | (nothing) | ARP for 127.0.0.1 → loopback queue | `limit_rate()` called, socket silenced |
| 2 | ARP req processed, 127.0.0.1 cached, ARP reply queued | `has_neighbor(127.0.0.1)` → Found → SYN sent | Handshake starts |
| 3 | SYN → SynReceived → SYN-ACK → Established → ACK → Established | — | Test passes |

### Early SSH connection

| Poll | Ingress | Egress | Result |
|------|---------|--------|--------|
| 1 | SSH SYN → RST → gateway ARP → **`limit_rate()`** | SYN attempt → `RateLimited` → **no ARP for 127.0.0.1** | Socket silenced 1 s |
| 2–1000 | ARP reply for gateway arrives | `egress_permitted` → false | Still silenced |
| (timeout) | — | — | **Panic** |

## Possible Fixes

### A. Time-based test timeout (simplest)

Change the test loop from an iteration count to a wall-clock timeout of at
least 2 seconds.  This gives the ARP rate limit and socket silence time to
expire naturally.

```rust
let deadline = crate::timer::uptime_us() + 2_000_000; // 2 seconds
loop {
    smoltcp_net::poll();
    // ... check states ...
    if crate::timer::uptime_us() >= deadline { break; }
    crate::threading::yield_now();
}
```

Pros: minimal change, no smoltcp internals needed.
Cons: adds up to 1 s latency to boot when the race occurs.

### B. Pre-seed the neighbor cache for local IPs (most robust)

After every `update_ip_addrs()` call (in the DHCP handler and during init),
fill the neighbor cache with entries mapping each local IP to the
interface's own MAC.  This eliminates ARP for loopback addresses entirely.

`InterfaceInner` is re-exported as `Context` and the `neighbor_cache` field
is `pub(crate)`.  Since we're in a `no_std` kernel with full control, we
can access it through the context reference:

```rust
// After update_ip_addrs(), seed local addresses into the neighbor cache
let timestamp = Instant::from_micros(crate::timer::uptime_us() as i64);
let mac = HardwareAddress::Ethernet(/* interface MAC */);
for cidr in net.iface.ip_addrs() {
    net.iface.context().neighbor_cache.fill(cidr.address(), mac, timestamp);
}
```

Pros: eliminates the entire class of loopback ARP issues; zero added
latency.
Cons: depends on smoltcp internals (`neighbor_cache` is `pub(crate)`, not
`pub`); may require a thin wrapper or fork.

### C. Drain external traffic before the test

Before creating the test sockets, drain any pending VirtIO RX packets by
calling `poll()` in a brief loop.  This lets any gateway-ARP rate limit
expire before the test's first egress attempt.

```rust
// Warm up: let any pending external traffic resolve
let warmup_deadline = crate::timer::uptime_us() + 1_100_000; // 1.1 s
while crate::timer::uptime_us() < warmup_deadline {
    smoltcp_net::poll();
    crate::threading::yield_now();
}
```

Pros: no smoltcp internals needed.
Cons: adds 1+ s to every boot, even when there's no early SSH connection.

### D. Separate ARP pre-resolution step in the test

Before creating the loopback connection, send a dummy packet to 127.0.0.1
(or explicitly trigger ARP) and wait for resolution.  Then start the actual
TCP test.

Pros: targeted, no smoltcp fork.
Cons: somewhat awkward; still subject to the rate limit if external traffic
arrives at the wrong moment.

## Recommendation

**Combine A + B**: use a time-based timeout (A) as the immediate fix, and
seed the neighbor cache (B) as the long-term fix.  Option B prevents
loopback traffic from ever needing ARP, which also benefits the SSH server
and any other service using 127.0.0.1.
