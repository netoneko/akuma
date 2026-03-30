# SSH Staggering: Root Cause & Options

## Symptom

Typed characters appear in bursts during SSH sessions rather than immediately. The `[SSH-ECHO]` instrumentation in `full.log` confirms this: keystroke read gaps are normally 50–300µs (matching typing speed), but stagger events produce gaps of **800ms–1.83s**.

## What the log shows

```
[Mem] Threads: 9/64 (1r 8rd)     ← 8 threads competing for CPU
[SSH-ECHO] read gap=806898us
[TMR] t=25000 T=0 f=0
[SSH-ECHO] read gap=913515us
[SSH-ECHO] read gap=1083019us
...
[Cleanup] Thread 3 recycled       ← extra thread dies
[Mem] Threads: 8/64 (1r 7rd)     ← stagger stops immediately
```

The stagger window coincides exactly with 9 ready threads; it stops the moment Thread 3 is recycled and the count drops to 8.

There is also one write-side symptom:
```
[SSH-ECHO-SLOW] echo took 25206us for 't'
```
A single keystroke echo took 25ms to write back to the client.

## Root cause

The scheduler has a `NETWORK_THREAD_RATIO=4` boost designed to give the smoltcp polling thread
25% of CPU time. However, the boost was hardcoded to **slot 0** (the boot/idle thread), while
the actual network poller (`run_async_main`) runs on a dynamically spawned thread (TN). The boost
was being wasted on the idle/cleanup loop, so TN competed in plain round-robin alongside all
other threads.

With 8 ready threads and 10ms timeslices, TN got scheduled roughly every 80ms. Since incoming
keystrokes need TN to call `smoltcp_net::poll()` to move from the VirtIO RX ring into the TCP
socket buffer — and then the SSH session thread needs its own scheduling slot to read from that
buffer — both threads missing in sequence compounds into 800ms+ gaps.

The extra ready thread during the stagger period made the round-robin longer, increasing the miss
probability and worsening the spikes.

The 25ms echo write delay (`[SSH-ECHO-SLOW]`) is the same problem on the write side: sent bytes
sit in the smoltcp TX buffer until TN's next scheduling slot.

## Remaining stagger (post-fix)

The boost fix is confirmed working — `[TMR]` entries now show `T=1` (TN) frequently. But
800ms–1.4s spikes persist. Two compounding causes remain:

### 1. No-op waker in `block_on` (`src/ssh/server.rs:121–126`)

```rust
static VTABLE: RawWakerVTable = RawWakerVTable::new(
    |_| RawWaker::new(core::ptr::null(), &VTABLE),
    |_| {},   // wake      — no-op
    |_| {},   // wake_by_ref — no-op
    |_| {},
);
```

When `TcpStream::read` suspends, it calls `socket.register_recv_waker(cx.waker())`
(`smoltcp_net.rs:924`). When TN later calls `smoltcp_net::poll()` and delivers the
packet to the socket, smoltcp fires that waker. But the waker does nothing — it does
not re-queue the SSH session thread. The SSH thread only discovers the data on its next
scheduled slot (~100ms with the boost in place).

**What needs investigating:**
- Implement a real waker that stores the SSH session thread's TID and calls
  `threading::mark_thread_ready(tid)` when fired. This would let smoltcp notify the SSH
  thread immediately when data arrives instead of relying on round-robin scheduling.
- The waker data pointer (`core::ptr::null()` today) should carry the thread ID or a
  pointer to an atomic flag the SSH thread checks.
- Edge case: the waker may be called from TN's context (inside `smoltcp_net::poll()`
  which holds the NETWORK spinlock). `mark_thread_ready` must be safe to call while
  the NETWORK lock is held — check for lock ordering issues.
- Edge case: the waker may be called after the SSH session has already been rescheduled
  and read the data. The implementation must be idempotent (calling wake on an already-
  ready thread is a no-op).

### 2. Multi-await chain in `read_until_channel_data` (`src/ssh/protocol.rs:91`)

Each call to `self.stream.read(&mut buf).await` at line 101 is a suspend point. One
SSH channel data message (a single keystroke) may require the loop to iterate multiple
times if non-data SSH packets (window adjustments, keepalives, etc.) arrive first. Each
iteration that suspends costs one full scheduling round (~100ms). Multiple iterations
multiply the latency: 3 iterations = up to 300ms from this source alone, on top of the
waker issue above.

**What needs investigating:**
- Whether SSH keepalives or window-adjust messages arrive frequently between keystrokes
  (add a counter/log for non-data messages processed in `read_until_channel_data`).
- Whether batching the TCP reads (larger buffer, or reading all available TCP data before
  processing SSH packets) would reduce the number of await suspensions per keystroke.

---

## Fix (implemented)

Replaced the hardcoded slot-0 boost with a registered network thread ID:

**`crates/akuma-exec/src/threading/mod.rs`**
- Added `static NETWORK_THREAD_ID: AtomicUsize` (initialized to `usize::MAX` = unset)
- Added `pub fn set_network_thread_id(tid: usize)`
- Scheduler boost now reads `NETWORK_THREAD_ID` instead of hardcoding `0`

**`src/main.rs`**
- `run_async_main()` calls `threading::set_network_thread_id(current_thread_id())` at startup
- Removed the `COOPERATIVE_MAIN_THREAD` branch; always uses the preemptive path

**`crates/akuma-exec/src/runtime.rs`** / **`src/config.rs`**
- Removed `cooperative_main_thread` field and `COOPERATIVE_MAIN_THREAD` constant (now redundant)
- Updated `NETWORK_THREAD_RATIO` comment to say "network thread" instead of "thread 0"

The thread name lookup in `kthreads` output was also updated to identify the network thread by
its registered ID rather than by slot position.
