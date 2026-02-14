# Implementation Plan: Fixing Loopback Connection Timeout

## Problem Statement
The network self-tests intermittently panic with a "Loopback connection timeout".
Log analysis shows: `Client: Some(SynSent), Server: Some(Listen)`.
This indicates the initial SYN packet (or the preceding ARP request) from the client never reached the server, despite both being on the same interface with `127.0.0.1` configured.

## Potential Root Causes
1.  **ARP Resolution Failure**: `smoltcp` requires a MAC address for `127.0.0.1`. If the ARP request isn't correctly intercepted by `is_loopback_frame`, it is sent to the physical VirtIO device and lost.
2.  **Timing & Retransmission**: The test loop (1000 iterations) may complete in as little as 20-50ms. If a packet is lost or delayed, the test times out before `smoltcp`'s TCP retransmission timer (typically 1s) fires.
3.  **DHCP Interference**: DHCP events trigger `update_ip_addrs`, which clears and re-adds all addresses. This can disrupt `smoltcp`'s internal state (neighbor cache, socket state) if it occurs during the handshake.

## Proposed Fixes

### 1. Robust Loopback Interception (`src/smoltcp_net.rs`)
Update `is_loopback_frame` to be more resilient. Instead of manual byte indexing, it should handle potential Ethernet padding and more robustly identify both ARP and IPv4 traffic destined for the `127.x.x.x` range.

### 2. Increase Test Timeout (`src/network_tests.rs`)
The current 1000-iteration loop is too fast.
- Add a small delay (e.g., `nanosleep` or `yield_now` with a counter) to ensure the test runs for at least 2-3 seconds before giving up.
- This allows `smoltcp`'s internal timers (for ARP and TCP retransmission) to fire.

### 3. Ensure Neighbor Cache Stability (`src/smoltcp_net.rs`)
When DHCP updates the IP configuration, we must ensure the loopback address and its associated neighbor entries (MAC address for `127.0.0.1`) are preserved or immediately restored.

### 4. Aggressive Handshake Polling (`src/network_tests.rs`)
Modify the test to call `poll()` multiple times per iteration. Loopback traffic depends on the `loopback_queue`, which requires a `transmit` (to queue) followed by a `receive` (to process) in subsequent `poll()` calls.

## Implementation Steps
1.  **Phase 1**: Modify `is_loopback_frame` in `src/smoltcp_net.rs` to ensure ARP requests for `127.0.0.1` are never missed.
2.  **Phase 2**: Update the loopback test in `src/network_tests.rs` to use a time-based timeout (e.g., 5 seconds) rather than a fixed iteration count.
3.  **Phase 3**: Add extra `smoltcp_net::poll()` calls in the test loop to flush the `loopback_queue` more aggressively.
4.  **Phase 4**: Verify the fix by running the kernel with `config::RUN_NETWORK_TESTS = true`.
