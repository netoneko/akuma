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
