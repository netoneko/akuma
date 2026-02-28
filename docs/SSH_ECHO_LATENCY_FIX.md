# SSH Echo Latency Fix (February 2026)

## Problem

Intermittent "stagger" in SSH sessions: typing a character would sometimes
produce no echo, forcing the user to type again, at which point both characters
appeared simultaneously. The issue was infrequent but noticeable.

## Root Cause

The stagger was caused by two cooperative-scheduling bottlenecks in the SSH
echo path.

### 1. `block_on` yielded unconditionally after every network poll

```rust
// src/ssh/server.rs — block_on (before)
Poll::Pending => {
    smoltcp_net::poll();
    crate::threading::yield_now();  // always yields
}
```

When a keystroke TCP packet arrived, `smoltcp_net::poll()` processed it and
placed the data in the TCP socket's receive buffer. But `block_on` yielded
to the scheduler *before* re-polling the future. The SSH thread had to wait
10–80 ms (depending on how many threads were ready) to be rescheduled, even
though the data was already waiting.

### 2. `flush()` added a redundant yield and the auto-flush blocked on ACK

```rust
// src/ssh/protocol.rs — SshChannelStream::flush (before)
async fn flush(&mut self) -> Result<(), Self::Error> {
    self.stream.flush().await.map_err(|_| SshStreamError)?;
    crate::threading::yield_now();  // extra context switch
    Ok(())
}
```

`stream.flush()` waits for `send_queue() == 0`, which means waiting for the
remote TCP ACK — not just for the data to hit the wire. The auto-flush wrapped
this in a 10 ms timeout, but the timeout was cooperative: it only fired when
polled. A single `yield_now()` could delay rescheduling by 30+ ms, making the
"10 ms" timeout take 36 ms in practice.

During those 36 ms the `write()` call hadn't returned, so the next `read()`
couldn't start, and the next keystroke's echo was delayed.

### Combined effect

The slow write path (flush + yield) accidentally hid part of the read-side
problem: during the 36 ms flush window, `block_on` was repeatedly calling
`smoltcp_net::poll()`, which caught incoming keystrokes. Fixing the write
path alone made the stagger *more pronounced* because the SSH thread now
yielded sooner, exposing the read-side unconditional yield.

## Diagnosis

Added instrumentation to the SSH echo path:

- `[SSH-TX-DROP]` — VirtIO TX send failures (confirmed: zero drops)
- `[SSH-ECHO]` — time gap between successive `read()` returns
- `[SSH-ECHO-SLOW]` — echo `write()` calls taking > 5 ms
- `[SSH-FLUSH-TIMEOUT]` — auto-flush exceeding 10 ms timeout

Key finding: `VirtIONetRaw::send()` is synchronous (`add_notify_wait_pop`),
so the TX ring (size 16) was never full. The stagger was entirely caused by
scheduler delays in the cooperative polling loop.

## Fixes

### 1. `block_on`: only yield when idle (src/ssh/server.rs)

```rust
Poll::Pending => {
    if !smoltcp_net::poll() {
        crate::threading::yield_now();
    }
}
```

If `smoltcp_net::poll()` made progress (returned `true`), re-poll the future
immediately — the incoming packet may have satisfied the pending read. Only
yield when the network is idle. The preemptive 10 ms timer still ensures
other threads get CPU time.

This matches the pattern already used by thread 0's main network loop in
`src/main.rs`.

### 2. Remove yield from flush (src/ssh/protocol.rs)

Removed the `yield_now()` call from `SshChannelStream::flush()`. The
`block_on` loop already yields on `Pending`, so the extra yield was
redundant and added a full scheduler round-trip after every write.

### 3. Skip ACK-flush for small interactive writes (src/ssh/protocol.rs)

For writes ≤ 128 bytes (keystroke echoes), call `smoltcp_net::poll()` once
to push the TCP segment out and return immediately — no need to wait for the
remote ACK. For larger writes, keep the flush with a 10 ms timeout to
respect TCP backpressure.

```rust
if buf.len() > 128 {
    let _ = with_timeout(SSH_INTERACTIVE_READ_TIMEOUT, self.flush()).await;
} else {
    crate::smoltcp_net::poll();
}
```

## Result

The echo path for a single keystroke went from:

    write → flush-wait-for-ACK → yield → reschedule (10–80 ms total)

to:

    write → poll (transmit) → re-poll read immediately (< 1 ms total)

Interactive SSH sessions no longer exhibit the stagger / double-character
symptom.
