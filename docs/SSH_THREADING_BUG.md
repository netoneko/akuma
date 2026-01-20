# SSH Multi-Session Threading Bug

This document describes a threading bug that causes crashes when multiple SSH sessions are active concurrently.

## Symptoms

### Crash Pattern
```
[Thread0] loop=1148000 | run=1 rdy=4 wait=2 term=0 init=0 free=25
hello (387/2000)
hello (458/5000)
[Exception] Sync from EL1: EC=0x25, ISS=0x44
  ELR=0x400d7cfc, FAR=0x680a303632313930, SPSR=0x200023c5
  Thread=1, TTBR0=0x402f0000, TTBR1=0x402f0000
```

### QEMU Errors
```
qemu-system-aarch64: virtio: zero sized buffers are not allowed
qemu-system-aarch64: Slirp: Failed to send packet, ret: -1
qemu-system-aarch64: Guest moved used index from 158 to 0
```

### Key Observations

1. **FAR contains ASCII data**: `0x680a303632313930` decodes to `"091260\nh"` - this is hello process output being interpreted as a memory address

2. **Crash in SSH key exchange code**: `handle_kex_ecdh_init` even though sessions were already established

3. **Virtio ring corruption**: "Guest moved used index from 158 to 0" indicates the virtqueue descriptor ring was corrupted

4. **Reproducible with concurrent output**: Running `hello 5000 100` and `hello 2000 100` from two SSH sessions triggers the crash

## Root Cause

### Architecture Issue

The SSH server uses a thread-per-session model:
- Thread 0: Accept loop and network runner (embassy executor)
- Threads 1-7: SSH session threads
- Threads 8+: User process threads

Each SSH session thread has its own `TcpStream` wrapped in `SendableTcpStream`:

```rust
struct SendableTcpStream(TcpStream);

// SAFETY: We ensure that:
// 1. The network runner is continuously polled on thread 0
// 2. Socket operations use internal synchronization
// 3. Each socket is only accessed from one thread at a time
unsafe impl Send for SendableTcpStream {}
```

**The problem**: Assumption #2 is FALSE. Embassy-net does not have internal synchronization for multi-threaded access. It was designed for single-threaded async operation.

### Race Condition Flow

```
Thread 1 (SSH session A)          Thread 2 (SSH session B)
        |                                 |
   hello output                      hello output
        |                                 |
   socket.write()                    socket.write()
        |                                 |
        +-------> embassy-net <-----------+
                (no internal sync)
                     |
              virtio driver
                     |
              RING CORRUPTION!
```

When two SSH sessions write concurrently:
1. Both call `socket.write()` from different threads
2. Embassy-net's internal state (socket buffers, queues) accessed without synchronization
3. Virtio driver's ring buffer descriptors get corrupted
4. QEMU detects invalid ring state and reports errors
5. Corrupted pointers cause data abort (FAR = ASCII data)

### Why FAR Contains ASCII

The hello process output like `"091260\n"` overwrites a buffer pointer due to:
1. Buffer overflow from race condition
2. Or use-after-free where buffer was reallocated for output data

When SSH code later dereferences this "pointer", it crashes with FAR = ASCII data.

## Current Mitigations

### VIRTIO_LOCK (Partial)

The virtio driver has a global lock for MMIO operations:

```rust
static VIRTIO_LOCK: Spinlock<()> = Spinlock::new(());

fn with_virtio_lock<R, F: FnOnce() -> R>(f: F) -> Option<R> {
    for _ in 0..VIRTIO_LOCK_MAX_ATTEMPTS {
        if let Some(guard) = VIRTIO_LOCK.try_lock() {
            let result = f();
            drop(guard);
            return Some(result);
        }
        // spin...
    }
    None
}
```

This protects low-level MMIO access but NOT:
- Embassy-net's internal socket state
- TcpSocket buffers and queues
- Higher-level protocol state

## Proposed Solutions

### Solution 1: Global Network Write Lock (Quick Fix)

Add a lock around all socket write operations:

```rust
static NETWORK_WRITE_LOCK: Spinlock<()> = Spinlock::new(());

impl TcpStream {
    pub async fn write(&mut self, data: &[u8]) -> Result<usize, TcpError> {
        let _guard = NETWORK_WRITE_LOCK.lock();
        // ... existing write code ...
    }
}
```

**Pros**: Simple, immediate fix
**Cons**: Serializes all network writes, reduces throughput

### Solution 2: Message Queue Architecture

Route all network I/O through thread 0:

```rust
// Session threads push to queue
static TX_QUEUE: Spinlock<VecDeque<(SocketId, Vec<u8>)>> = ...;

// Thread 0 drains queue and sends
fn network_runner() {
    loop {
        // Drain TX queue
        while let Some((socket_id, data)) = TX_QUEUE.lock().pop_front() {
            sockets[socket_id].write(&data).await;
        }
        // Poll network stack
        stack.run_until_stalled().await;
    }
}
```

**Pros**: Proper thread isolation, no races
**Cons**: More complex, higher latency

### Solution 3: Async-Only Architecture (Proper Fix)

Run SSH sessions as async tasks on thread 0's executor instead of separate threads:

```rust
// All sessions on one executor
#[embassy_executor::main]
async fn main(spawner: Spawner) {
    spawner.spawn(network_runner()).unwrap();
    spawner.spawn(ssh_accept_loop(spawner)).unwrap();
}

async fn ssh_accept_loop(spawner: Spawner) {
    loop {
        let socket = accept().await;
        spawner.spawn(ssh_session(socket)).unwrap();
    }
}
```

**Pros**: Eliminates threading issues entirely, embassy works as designed
**Cons**: Major refactor, loses preemptive multitasking for sessions

## Related Issues

- Thread 0 format panic (see CONTEXT_SWITCH_FIX_2026.md) - Fixed with safe_print
- Context switch race conditions - Fixed with INITIALIZING state
- Heap allocation in IRQ context - Fixed with non-allocating prints

## Testing

To reproduce the bug:
1. Start akuma with SSH enabled
2. Open two SSH sessions
3. In each session, run: `hello 5000 100`
4. Wait for crash (usually within 1-2 minutes)

## Files Involved

- `src/ssh/server.rs`: SendableTcpStream, session threading
- `src/async_net.rs`: TcpStream wrapper
- `src/embassy_virtio_driver.rs`: Virtio network driver
- `src/embassy_net_driver.rs`: Embassy driver trait implementation
