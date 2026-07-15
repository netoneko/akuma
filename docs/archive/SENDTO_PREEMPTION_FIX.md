# sys_sendto Preemption Fix

## Problem

The kernel watchdog was detecting preemption disabled for 5+ seconds during meow LLM API calls:

```
[WATCHDOG] Preemption disabled for 104ms at step 14
[WATCHDOG] Preemption disabled for 1105ms at step 14
[WATCHDOG] Preemption disabled for 2111ms at step 14
...
[WATCHDOG] Thread 2 preemption disabled 5011ms (critical)
```

## Root Cause

In `sys_sendto` (src/syscall.rs), after writing all data, the code would flush the socket to ensure transmission. The flush loop was **inside** `with_socket_handle`:

```rust
// BROKEN: yield_now() called with preemption disabled!
let _ = socket::with_socket_handle(socket_idx, |socket| {
    for _ in 0..100 {
        let mut flush_future = socket.flush();
        match pinned.poll(&mut cx) {
            Poll::Ready(_) => break,
            Poll::Pending => {
                crate::threading::yield_now();  // BAD!
            }
        }
    }
});
```

The problem is that `with_socket_handle` disables preemption (and IRQs) to protect embassy-net's RefCells. Calling `yield_now()` inside this closure means:

1. Preemption remains disabled during the yield
2. The loop can iterate up to 100 times waiting for flush
3. Network flush can take seconds on slow connections
4. Result: 5+ seconds with preemption disabled, triggering watchdog

## Fix

Restructure the loop so `yield_now()` is called **outside** `with_socket_handle`:

```rust
// FIXED: yield_now() called with preemption enabled
for _ in 0..100 {
    let flush_result = socket::with_socket_handle(socket_idx, |socket| {
        let mut flush_future = socket.flush();
        match pinned.poll(&mut cx) {
            Poll::Ready(_) => true,   // Flush complete
            Poll::Pending => false,   // Need to retry
        }
    });
    
    match flush_result {
        Ok(true) => break,  // Done
        Ok(false) => {
            // Yield OUTSIDE with_socket_handle (preemption enabled here)
            crate::threading::yield_now();
        }
        Err(_) => break,  // Socket error
    }
}
```

Now each poll is a brief operation with preemption disabled (microseconds), and yielding happens with preemption enabled.

## Related

- `docs/CONCURRENCY.md` - Lock hierarchy and preemption rules
- `docs/HERD_BLOCKING_FIX.md` - Similar preemption watchdog issue
- `src/socket.rs:with_socket_handle()` - Socket access with preemption control

## Key Rule

**Never call `yield_now()` or any blocking operation inside `with_socket_handle` or other preemption-disabled contexts.** Poll briefly, return a result, then yield outside.
