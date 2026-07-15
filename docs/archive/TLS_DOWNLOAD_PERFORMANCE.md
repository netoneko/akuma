# TLS Download Performance Fix

**Date:** January 2026  
**Issue:** Slow HTTPS downloads (4MB repo taking minutes instead of seconds)

## Symptom

When cloning a repository via HTTPS, the download was extremely slow. A 4MB pack file was downloading at a fraction of expected speed.

## Root Cause

The TLS transport layer had a **10ms sleep** on every `WouldBlock` error from the socket read.

### The Problem Flow

```
1. Userspace calls TLS read
2. TLS calls socket recv syscall
3. Kernel tries 50 iterations with yield, returns EAGAIN (no data yet)
4. TLS transport sleeps 10ms  ← BOTTLENECK
5. Repeat
```

### Impact Calculation

For a 4MB download:
- ~1000 TLS record reads (at ~4KB each)
- If 50% hit WouldBlock before data arrives: 500 × 10ms = **5 seconds of pure sleep**
- Actual impact was even worse due to multiple WouldBlock per read

## Fix

Reduced sleep time from 10ms to 1ms in three locations:

| File | Before | After |
|------|--------|-------|
| `libakuma-tls/src/transport.rs` | 10ms | 1ms |
| `libakuma-tls/src/http.rs` | 10ms | 1ms |
| `userspace/scratch/src/http.rs` | 10ms | 1ms |

Timeout thresholds were increased proportionally (500 → 5000 iterations) to maintain the ~5 second idle timeout.

## Why 1ms Works

1. The kernel's `sys_recvfrom` already does 50 yield iterations before returning EAGAIN
2. The 1ms sleep allows the kernel's network driver (embassy-net/smoltcp) to poll for new packets
3. 1ms is the minimum granularity that reliably triggers a context switch

## Expected Improvement

~10x faster HTTPS downloads. A 4MB file that took minutes should now complete in seconds.

## Alternative Approaches Considered

| Approach | Pros | Cons |
|----------|------|------|
| Reduce sleep to 1ms | Simple, effective | Still has some overhead |
| Use blocking socket | No polling needed | Requires kernel changes |
| Increase kernel EAGAIN iterations | Reduces userspace retries | May hurt responsiveness |

The 1ms approach was chosen for simplicity and immediate impact without kernel changes.

## Additional Fix: Time-Based Network Thread Priority

A second issue was that SSH became unresponsive while scratch was downloading. This happened because the userspace process was consuming scheduler time, starving the main network thread (thread 0).

### Problem with Simple Priority Boost

Initially we tried `MAIN_THREAD_PRIORITY_BOOST = true` which always schedules thread 0 first. But this causes the opposite problem: since thread 0 is almost always READY (it's a polling loop that yields), userspace processes would get starved.

With up to 4 concurrent SSH sessions, we need fair scheduling while maintaining network responsiveness.

### Solution: Proportional Scheduling

Thread 0 gets boosted every N scheduler ticks, where N is configurable:

```rust
// In src/config.rs
pub const NETWORK_THREAD_RATIO: u32 = 4;  // Thread 0 gets 25% of slots

// In scheduler (threading.rs)
if current_idx != 0 {
    self.network_boost_counter += 1;
    if self.network_boost_counter >= config::NETWORK_THREAD_RATIO {
        self.network_boost_counter = 0;
        // Boost thread 0 if ready
    }
}
```

### CPU Distribution with NETWORK_THREAD_RATIO = 4

| Component | CPU Share |
|-----------|-----------|
| Thread 0 (network) | 25% |
| Userspace threads | 75% total |
| Each of 4 downloads | ~19% each |

### Tuning

- Lower ratio = more network responsiveness, less userspace CPU
- Higher ratio = more CPU for downloads, network may lag
- With 10ms timer: ratio=4 means network polled every ~40ms

### Syscall Network Helper

To improve syscall/network coordination, we added infrastructure in `src/async_net.rs`:

```rust
// Track network readiness
static NETWORK_READY: AtomicBool = AtomicBool::new(false);

// Syscalls check before any network operation
pub fn is_network_ready() -> bool { ... }

// Syscalls request a poll (yields to thread 0)
pub fn request_network_poll() -> bool { ... }
```

**How it works:**
1. Syscalls check `is_network_ready()` first, returning `ENETDOWN` if not ready
2. When data isn't available, `request_network_poll()` yields to thread 0
3. Scheduler uses ratio-based boosting to run thread 0 periodically
4. Syscall thread retries when scheduled again

**Current limitations:**
- The `POLL_REQUESTED` flag is **disabled** (`ENABLE_SYSCALL_POLL_REQUEST=false`)
- When enabled, it caused thread starvation: any syscall hitting WouldBlock would
  create a tight loop between that thread and thread 0, starving all other threads
- Future work: per-thread request tracking or rate limiting

**Benefits (with current implementation):**
- Clear network readiness semantics (not silently failing)
- Proper error codes (`ENETDOWN` vs `EAGAIN`)
- Statistics tracking (`get_poll_count()` for debugging)

### Future Work

**What was tried and failed:**
1. **Immediate poll request boost** - When a syscall set `POLL_REQUESTED`, scheduler
   immediately switched to thread 0. This caused thread starvation because any
   thread hitting WouldBlock would monopolize the CPU with thread 0.

**Potential solutions (not yet implemented):**

1. **Per-thread request tracking** - Each thread gets its own poll request flag.
   Scheduler honors each thread's request once, then ignores until thread runs again.
   This prevents the tight loop starvation.

2. **Rate-limited boosting** - Only honor poll requests every N milliseconds.
   This would spread out the boosting and allow other threads to run.

3. **Direct polling from syscalls** - The ideal solution would poll embassy-net
   directly from `sys_recvfrom`. Challenges:
   - Embassy-net uses `RefCell`, not designed for multi-thread access
   - Would require refactoring to use `Mutex` or a thread-local polling mechanism
   - Or: single-threaded syscall polling with a dedicated network thread

4. **Cooperative network yielding** - Instead of timer-based preemption, have
   the network thread explicitly yield after each poll cycle with a hint to
   run a specific waiting thread.

**Current workaround:**
- `NETWORK_THREAD_RATIO = 2` gives thread 0 ~50% of CPU time
- This provides reasonable network responsiveness for most use cases
- Heavy concurrent downloads may still see degraded speeds

## Files Modified

- `userspace/libakuma-tls/src/transport.rs` - TcpTransport::read() sleep reduced (10ms → 1ms)
- `userspace/libakuma-tls/src/http.rs` - read_http_response_raw() sleep reduced
- `userspace/scratch/src/http.rs` - read_http_response() sleep reduced, instantaneous speed display
- `userspace/scratch/src/stream.rs` - Instantaneous speed display
- `src/config.rs` - Added NETWORK_THREAD_RATIO (2), ENABLE_SYSCALL_POLL_REQUEST (false)
- `src/threading.rs` - Implemented proportional scheduler for thread 0
- `src/async_net.rs` - Added syscall network helper infrastructure (poll request disabled)
- `src/syscall.rs` - Network syscalls check readiness, use yield-based polling
- `src/main.rs` - Mark network ready after initialization
