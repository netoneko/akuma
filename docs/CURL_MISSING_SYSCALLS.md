# curl / wget — TCP Connection Close Fix

## Problem

`wget` (and any HTTP client using `Connection: close` or HTTP/1.0) would hang for
~30 seconds after receiving the full response before returning. The data was received
correctly, but the connection teardown stalled.

## Root Cause

In `src/socket.rs` `socket_recv()`, the blocking wait condition and EOF detection
only checked two states:

```rust
socket.can_recv() || !socket.is_active()
```

When the remote server finishes sending and closes the connection (sends TCP FIN),
smoltcp transitions the socket to **CloseWait**. In this state:

| smoltcp method    | Value   | Why                                          |
|-------------------|---------|----------------------------------------------|
| `can_recv()`      | `false` | All buffered data already consumed           |
| `is_active()`     | `true`  | CloseWait is still "active" (not Closed/TimeWait) |
| `may_recv()`      | `false` | Remote sent FIN — no more data will arrive   |

Since neither condition (`can_recv()` or `!is_active()`) was satisfied, `wait_until`
spun for the full 30-second timeout before returning `ETIMEDOUT`. The client then
treated the timeout as EOF and proceeded, but with a 30-second delay.

## Fix

Added `!socket.may_recv()` to both the wait condition and the EOF return path:

```rust
// Wait: also unblock when remote has closed its send side
socket.can_recv() || !socket.is_active() || !socket.may_recv()

// Return 0 (EOF) when remote has closed
} else if !socket.is_active() || !socket.may_recv() { Ok(0) }
```

`may_recv()` returns `false` once the remote FIN is received (CloseWait, LastAck,
Closing, TimeWait, Closed). This correctly signals EOF immediately instead of
waiting for the full socket to transition to Closed.

## Affected File

- `src/socket.rs` — `socket_recv()` function
