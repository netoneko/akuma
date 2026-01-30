# Meow-Ollama Connection Timeout Fix

## Issue

When using meow to communicate with Ollama, connections were repeatedly failing with 500 errors. Ollama logs showed:

```
[GIN] 2026/01/29 - 23:56:50 | 500 |  6.270837667s | 127.0.0.1 | POST "/api/chat"
time=... level=INFO source=runner.go:916 msg="aborting completion request due to client closing the connection"
```

The pattern was consistent:
1. Request sent to Ollama
2. Ollama starts processing (LLM inference takes time)
3. Connection abruptly closed by client
4. Ollama aborts and returns 500
5. Client retries, same cycle repeats

## Root Causes

There were **two separate timeout issues** causing connection failures.

### Issue 1: Busy-Polling Timeout (~6 seconds)

The socket receive syscall (`sys_recvfrom` in `src/syscall.rs`) uses busy-polling with an iteration limit:

```rust
const MAX_ITERATIONS: usize = 100_000;

loop {
    match socket.read(...).poll(&mut cx) {
        Poll::Pending => {
            iterations += 1;
            if iterations >= MAX_ITERATIONS {
                return (-libc_errno::ETIMEDOUT as i64) as u64;  // -110
            }
            crate::threading::yield_now();
            for _ in 0..100 { core::hint::spin_loop(); }
        }
        // ...
    }
}
```

After 100,000 iterations (~6 seconds), the kernel returns `ETIMEDOUT` (-110). Meow only treated `WouldBlock` as recoverable, so `TimedOut` caused it to drop the connection.

### Issue 2: Embassy Socket Timeout (30 seconds)

Even after fixing Issue 1, connections still failed after exactly 30 seconds. This was caused by the embassy-net socket timeout set during `block_on_connect`:

```rust
socket_ref.set_timeout(Some(embassy_time::Duration::from_secs(30)));
```

This 30-second timeout was intended for the connection phase, but it persisted and applied to subsequent read operations. After 30 seconds of waiting for data, embassy-net would close the socket.

## The Fixes

### Fix 1: Handle TimedOut in Meow (`userspace/meow/src/main.rs`)

Treat `TimedOut` the same as `WouldBlock` - both indicate "no data available yet, connection still valid":

```rust
Err(e) => {
    // WouldBlock and TimedOut both mean "no data available yet"
    // The kernel returns TimedOut after busy-polling iterations expire,
    // but the connection is still valid - just retry the read
    if e.kind == libakuma::net::ErrorKind::WouldBlock 
        || e.kind == libakuma::net::ErrorKind::TimedOut {
        read_attempts += 1;
        
        if read_attempts > 6000 {
            return Err("Timeout waiting for response");
        }
        libakuma::sleep_ms(10);
        continue;
    }
    // ...
}
```

### Fix 2: Clear Socket Timeout After Connect (`src/syscall.rs`)

Clear the embassy-net socket timeout after a successful connection, allowing reads to wait indefinitely:

```rust
Poll::Ready(Ok(())) => {
    drop(connect_fut);
    let mut socket = unsafe { socket_cell.into_inner() };
    crate::threading::disable_preemption();
    // Clear the timeout after successful connect - reads may take much longer
    // (e.g., waiting for LLM inference which can take 60+ seconds)
    socket.set_timeout(None);
    let local = socket.local_endpoint();
    crate::threading::enable_preemption();
    return Ok((socket, local));
}
```

## Why These Fixes Work

1. **Busy-polling ETIMEDOUT is not fatal**: The socket is still connected; only the polling loop gave up temporarily
2. **Connection timeout ≠ Read timeout**: A 30-second timeout makes sense for establishing connections, but LLM inference can take 60+ seconds
3. **Application controls the timeout**: Meow's own `read_attempts > 6000` check (60 seconds) provides a reasonable application-level timeout

## Lessons Learned

1. **Multiple timeout layers**: Network code often has timeouts at multiple layers (embassy-net, syscall polling, application). They all need to be configured appropriately.
2. **LLM inference is slow**: A 27B parameter model can take 30-60+ seconds to generate a response
3. **Timeout semantics differ**: A "timeout" from busy-polling is different from a socket timeout is different from an application timeout
4. **Error handling should be explicit**: Falling through to a generic error for unexpected cases can hide bugs

## Files Changed

- `userspace/meow/src/main.rs`: Added `TimedOut` to the list of recoverable errors in `read_streaming_response_with_progress()`
- `src/syscall.rs`: Clear socket timeout after successful connection in `block_on_connect()`
