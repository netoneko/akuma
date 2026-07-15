# DHCP Loopback Test Fix

## Problem

With `ENABLE_DHCP=true`, the loopback network self-test panics at boot:

```
[NetTest] Testing loopback connection (127.0.0.1:9999)...
[SmolNet] DHCP deconfigured - reverting to static fallback
[SmolNet] DHCP configured
[SmolNet] IP: 10.0.2.15/24
[NetTest] Connection failed. Client: Some(Established), Server: Some(SynReceived)

!!! PANIC !!!
Network Test Failed: Loopback connection timeout
```

The client socket reaches `Established` but the server stays stuck in `SynReceived`,
meaning the final ACK of the TCP three-way handshake is never delivered.

## Root Cause

The loopback TCP handshake spans multiple `iface.poll()` calls:

1. **Poll N egress** — client sends SYN → loopback queue
2. **Poll N+1 ingress** — SYN processed, SYN-ACK sent, SYN-ACK processed, ACK
   generated (via egress or ingress tx-token) → loopback queue
3. **Poll N+2 ingress** — ACK delivered to server → `Established`

DHCP's initial `Deconfigured` and `Configured` events fire during these same poll
calls (shortly after `smoltcp_net::init()`). Each event handler calls:

```rust
net.iface.update_ip_addrs(|addrs| {
    addrs.clear();           // removes ALL addresses including 127.0.0.1/8
    addrs.push(...);         // re-adds primary + loopback
});
```

Although 127.0.0.1/8 is immediately re-added, the address set change between
consecutive `iface.poll()` calls disrupts smoltcp's processing of the in-flight
loopback handshake. The server never receives the final ACK.

## Fix

Two complementary changes in `src/smoltcp_net.rs` and `src/network_tests.rs`:

### 1. Re-poll after DHCP reconfiguration (`smoltcp_net.rs`)

After handling a DHCP event that changes IP addresses, `iface.poll()` is called a
second time within the same `poll()` invocation. This ensures any loopback packets
queued before the address change are immediately processed with the updated
configuration, rather than waiting for the next external `poll()` call.

```rust
if dhcp_changed {
    let timestamp = Instant::from_micros(crate::timer::uptime_us() as i64);
    net.iface.poll(timestamp, &mut net.device, &mut net.sockets);
}
```

### 2. DHCP warm-up before tests (`network_tests.rs`)

Before running the loopback test, `run_tests()` now polls the network in a loop
until DHCP reports `Configured` (tracked via a new `DHCP_CONFIGURED` atomic flag
and `is_dhcp_configured()` API). This drains the initial `Deconfigured`/`Configured`
event pair so the loopback test runs with stable network configuration. Times out
after 5 seconds and falls back to static IP.

## Files Changed

- `src/smoltcp_net.rs` — added `DHCP_CONFIGURED` flag, `is_dhcp_configured()`,
  re-poll after DHCP events
- `src/network_tests.rs` — DHCP warm-up loop before `test_loopback_connection()`
