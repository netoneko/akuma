# SSH Streaming Output Architecture

This document explains how SSH command output streaming works in Akuma, its current limitations, and potential future improvements.

## Overview

Akuma supports streaming output for external binaries executed via SSH. When running a long-running command like `hello`, output is written progressively rather than buffered entirely until command completion.

## Architecture

### Thread Model

```
Thread 0: Main loop + Network runner (embassy-net polling)
          └── Polls network driver to actually transmit TCP packets

Threads 1-7: SSH session threads (one per connection)
             └── Use block_on() to run async SSH protocol handling
             └── Write to TCP socket buffers

Threads 8+: User process threads
            └── Execute ELF binaries
            └── Write to ProcessChannel for IPC
```

### Data Flow for Streaming Output

```
1. User process (thread 8+) writes to stdout
   └── Data goes to ProcessChannel buffer

2. SSH session thread (1-7) polls ProcessChannel
   └── exec_streaming() reads available data

3. SSH session writes to SshChannelStream
   └── Calls send_channel_data() → TcpStream::write_all()
   └── Data goes into smoltcp's TCP send buffer

4. SSH session calls flush() and yield_now()
   └── Yields CPU to scheduler

5. Thread 0 eventually runs
   └── Polls embassy-net network runner
   └── Network runner processes smoltcp's buffers
   └── Virtio driver transmits packets to QEMU
```

### Key Components

#### `exec_streaming()` (src/process.rs)

Spawns a process and streams its output to a generic async writer:

```rust
pub async fn exec_streaming<W>(path: &str, stdin: Option<&[u8]>, output: &mut W) -> Result<i32, String>
where
    W: embedded_io_async::Write,
{
    let (thread_id, channel) = spawn_process_with_channel(path, stdin)?;
    
    loop {
        if let Some(data) = channel.try_read() {
            // Write output and flush
            output.write_all(&data).await;
            output.flush().await;
            
            // Yield to allow network transmission
            for _ in 0..100 {
                crate::threading::yield_now();
            }
        }
        
        if channel.has_exited() {
            // Drain remaining output
            break;
        }
        
        YieldOnce::new().await;
    }
}
```

#### `SshChannelStream` (src/ssh/protocol.rs)

Wraps TcpStream to provide SSH channel data framing:

```rust
impl Write for SshChannelStream<'_> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        // Send as SSH channel data packets
        send_channel_data(self.stream, self.session, buf).await?;
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.stream.flush().await?;
        crate::threading::yield_now();
        Ok(())
    }
}
```

#### `check_streamable_command()` (src/shell/mod.rs)

Determines if a command can use streaming (vs buffered) execution:

- ✓ Single external binary in /bin
- ✗ Pipelines (|)
- ✗ Redirections (>, >>)
- ✗ Command chains (;, &&)
- ✗ Builtin commands

## Current Limitation: Batched Transmission

### The Problem

Despite the streaming code being correct, output arrives in batches rather than true real-time streaming:

```
$ ssh user@host "hello"
# All output arrives ~1 second after connection, not progressively
```

### Root Cause

1. **Thread 0 is the only transmitter**: Only thread 0 polls embassy-net's network runner, which actually transmits TCP packets via virtio.

2. **block_on uses no-op waker**: SSH session threads use a simple `block_on` implementation with a no-op waker. When futures return `Pending`, the thread just yields and retries.

3. **yield_now() doesn't guarantee thread 0 runs**: With round-robin scheduling across potentially 8+ threads, thread 0 may not get CPU time for several scheduling cycles.

4. **TCP socket writes are buffered**: Writing to `TcpStream` only queues data in smoltcp's internal buffer. Actual packet transmission requires the network runner to poll.

### Timeline Analysis

For a command that runs for 1 second with 10 iterations:

```
T+0ms:    SSH session starts, sends exec request
T+1ms:    First output written to TCP buffer
T+100ms:  Second output written to TCP buffer
...
T+900ms:  Last output written to TCP buffer
T+950ms:  Command exits, SSH sends EOF
T+1000ms: Thread 0 finally polls network, ALL packets transmitted
```

The output accumulates in TCP buffers and is transmitted in one burst when thread 0 gets sufficient CPU time.

## Configuration

Streaming is controlled by `config::ENABLE_SSH_ASYNC_EXEC` (src/config.rs):

```rust
/// Enable async process execution with streaming output over SSH
pub const ENABLE_SSH_ASYNC_EXEC: bool = true;
```

## Potential Improvements

### 1. Run SSH Sessions on Thread 0 (High Effort)

Instead of dedicated session threads, run SSH sessions as embassy tasks on thread 0's executor:

**Pros:**
- Direct integration with network polling
- True async behavior with proper wakers

**Cons:**
- Major architectural change
- Single-threaded SSH limits concurrency
- Complex interaction with filesystem operations

### 2. Add Network Polling from Session Threads (Medium Effort)

Give SSH session threads direct access to poll the network runner:

```rust
// In exec_streaming, after writing output:
if let Some(runner) = get_network_runner() {
    runner.poll_once();  // Directly transmit pending packets
}
```

**Pros:**
- Minimal architecture change
- Each write triggers transmission

**Cons:**
- Requires thread-safe network runner access
- Potential lock contention with thread 0

### 3. Priority Scheduling for Thread 0 (Low Effort)

Modify scheduler to give thread 0 higher priority:

```rust
fn schedule_next() -> ThreadId {
    // Always check thread 0 first
    if thread_0_has_pending_work() {
        return 0;
    }
    // Round-robin for others
    round_robin_schedule()
}
```

**Pros:**
- Simple change
- Reduces transmission latency

**Cons:**
- May starve other threads
- Doesn't solve the fundamental issue

### 4. Explicit Wakeup Signaling (Medium Effort)

Add a mechanism for SSH threads to signal thread 0:

```rust
static NETWORK_POLL_REQUESTED: AtomicBool = AtomicBool::new(false);

// In SSH thread after write:
NETWORK_POLL_REQUESTED.store(true, Ordering::Release);

// In thread 0's yield check:
if NETWORK_POLL_REQUESTED.swap(false, Ordering::Acquire) {
    // Skip other threads, return to polling immediately
}
```

## Testing

### Verify Streaming Path

```bash
# Interactive shell shows per-command output
ssh user@localhost -p 2222
> hello  # Output appears after command completes
```

### Measure Transmission Timing

```bash
# Timestamp each line to see batching
ssh user@localhost -p 2222 "hello" | while read line; do
    echo "$(date +%S.%3N): $line"
done
```

Expected output (current behavior):
```
42.100: hello (1/10)
42.101: hello (2/10)
...
42.110: hello (10/10)
```

All lines have the same timestamp (arrived together).

## Related Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md) - Overall system architecture
- [MULTITASKING.md](MULTITASKING.md) - Thread scheduling details
- [SSH.md](SSH.md) - SSH server implementation

## Conclusion

The streaming infrastructure is correctly implemented. The limitation is architectural: SSH session threads cannot directly trigger network transmission, which is exclusively handled by thread 0's polling loop. Future work to improve real-time streaming would require changes to either the threading model or the network polling architecture.
