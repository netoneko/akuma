# Networking Polling and ACK Fixes

**Date:** February 12, 2026
**Branch:** `improve-tls-networking-strategy-c`

## Problems

1. **Download speed collapses over time** — `scratch clone` starts fast then drops to ~2 kbps.
2. **`top` displays garbage numbers** — CPU percentages are astronomical or truncated.
3. **SSH login fails during active downloads** — new connections can't complete handshake.

## Root Causes Found

### 1. smoltcp 10ms Delayed ACK (primary throughput killer)

smoltcp 0.12 defaults to a 10ms delayed ACK. For receive-heavy workloads (downloads), there is no outgoing data to piggyback the ACK onto, so every ACK is delayed by the full 10ms. Combined with scheduler delays, the effective ACK round-trip becomes 10–20ms, capping throughput at roughly `65 KB / 20 ms ≈ 3.25 MB/s` and causing progressive TCP window collapse as congestion control interprets the delayed ACKs as a slow receiver.

**Fix:** `socket.set_ack_delay(None)` when creating sockets in `smoltcp_net::socket_create()`.

### 2. `poll_input_event` syscall argument order mismatch

The libakuma wrapper passed arguments as `(timeout, buf_ptr, buf_len)` while the kernel expected `(buf_ptr, buf_len, timeout_us)`. This caused:

- The kernel to use `buf.len()` (1) as the timeout → 1 µs instead of 1 second.
- `top` refreshing thousands of times per second with near-zero deltas.
- Potential memory corruption if terminal input arrived (write to address 0x3E8).

**Fix:** Corrected argument order in `userspace/libakuma/src/lib.rs` and added ms→µs conversion.

### 3. Main loop always yields

Thread 0 (network poll loop) called `yield_now()` on every iteration, even during bursts. This created ~10ms gaps where no ACKs, window updates, or retransmissions were processed.

**Fix:** Conditional yield — only yield when `smoltcp_net::poll()` reports no progress.

### 4. `wait_until` only polled once per iteration

The blocking helper used by `socket_recv`/`socket_send` processed one batch of packets then yielded, even though the calling thread was about to block anyway.

**Fix:** Drain loop (up to 64 iterations) in `wait_until`, with conditional yield on no-progress.

### 5. No poll after recv/send

After `socket_recv` freed RX buffer space (enabling a larger TCP window), the window update ACK waited until the next thread-0 poll. Similarly, `socket_send` queued data but didn't trigger transmission.

**Fix:** Added `smoltcp_net::poll()` call after `socket_recv` and `socket_send`.

### 6. `top` first-frame calculation

On the first display frame, `last_stats` was all zeros and `delta_time` was near-zero, producing astronomical CPU% values that overflowed the 3-digit display.

**Fix:** Pre-populate `last_stats` with a real `get_cpu_stats()` call before the display loop, and clamp CPU% to 100%.

## Files Changed

| File | Changes |
|------|---------|
| `src/smoltcp_net.rs` | Disable delayed ACK on new sockets |
| `src/main.rs` | Drain-loop polling, conditional yield |
| `src/socket.rs` | Drain-loop in `wait_until`, post-recv/send poll |
| `userspace/libakuma/src/lib.rs` | Fix `poll_input_event` argument order |
| `userspace/top/src/main.rs` | First-frame stats init, clamp CPU% |

## Known Issue

An EL1 data abort (FAR=0x65, null-ish pointer) was observed once inside `TcpStream::read` → `with_network` on the SSH session thread while running `curl https://ifconfig.me`. The crash is a dereference of corrupted SocketSet data. An earlier attempt to add aggressive drain loops (64 iterations) inside the SSH server's `block_on` and accept loop triggered the issue — those changes were reverted. The crash may be a pre-existing latent bug exposed by timing changes; it has not reproduced after reverting the SSH server drain loops.
