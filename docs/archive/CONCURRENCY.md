# Concurrency and Synchronization in Akuma

This document describes the concurrency model, synchronization primitives, lock hierarchy, and known issues in the Akuma kernel.

## Table of Contents

- [Overview](#overview)
- [Synchronization Primitives](#synchronization-primitives)
- [Lock Hierarchy](#lock-hierarchy)
- [Global Static Locks](#global-static-locks)
- [Safe Patterns Used](#safe-patterns-used)
- [Known Issues and Warnings](#known-issues-and-warnings)
- [Guidelines for Contributors](#guidelines-for-contributors)

---

## Overview

Akuma is a bare-metal kernel for AArch64 that uses:
- **Preemptive threading** with a fixed-size thread pool
- **Cooperative async tasks** via Embassy executor
- **Spinlocks** for synchronization in a single-core environment
- **Critical sections** (IRQ disabling) for interrupt-safe operations

The kernel runs on a single CPU core, so synchronization primarily concerns:
1. Preventing races between normal code and interrupt handlers
2. Ensuring consistent lock ordering to prevent deadlocks
3. Avoiding priority inversion and unbounded interrupt latency

---

## Synchronization Primitives

### Spinlock (`spinning_top::Spinlock`)

Used for most kernel data structures. Spins until the lock is available.

```rust
static EXAMPLE: Spinlock<Data> = Spinlock::new(Data::new());

fn use_data() {
    let guard = EXAMPLE.lock();
    // ... use data ...
} // guard dropped, lock released
```

**Warning**: Never hold a spinlock across an `await` point or while sleeping.

### Critical Section (`critical_section`)

Disables interrupts to provide mutual exclusion with interrupt handlers.

```rust
critical_section::with(|cs| {
    // IRQs disabled here
    let data = SHARED.borrow(cs).borrow_mut();
    // ...
}); // IRQs restored
```

### Embassy Async Mutex (`embassy_sync::mutex::Mutex`)

Used for async-safe mutual exclusion. Can be held across `await` points.

```rust
static FS_MUTEX: Mutex<CriticalSectionRawMutex, ()> = Mutex::new(());

async fn filesystem_op() {
    let _guard = FS_MUTEX.lock().await;
    // ... async filesystem operation ...
}
```

### IrqGuard (`crate::irq::IrqGuard`)

RAII guard that disables IRQs and restores them on drop.

```rust
pub fn with_irqs_disabled<T, F: FnOnce() -> T>(f: F) -> T {
    let _guard = IrqGuard::new();
    f()
}
```

---

## Lock Hierarchy

To prevent deadlocks, locks must always be acquired in a consistent order. The established hierarchy is:

```
Level 1: MOUNT_TABLE (VFS mount table)
    │
    ▼
Level 2: ext2.state (per-filesystem state)
    │
    ▼
Level 3: BLOCK_DEVICE (block device access)
    │
    ▼
Level 4: TALC (memory allocator) - always with IRQs disabled
```

**Special cases:**
- `POOL` (thread pool) - only accessed from scheduler context or with `with_irqs_disabled`
- `IRQ_HANDLERS` - uses copy-out pattern to avoid holding lock during handler execution
- `IRQ_WORK_QUEUE` - uses `try_lock` to avoid blocking in IRQ context

### Lock Acquisition Rules

1. **Never acquire a higher-level lock while holding a lower-level lock**
2. **Always disable IRQs before locking `TALC`** (done automatically by allocator)
3. **Always use `with_irqs_disabled` when accessing `POOL`** from non-scheduler context
4. **Never hold spinlocks across `await` points**

---

## Global Static Locks

| Lock | Module | Purpose | Special Notes |
|------|--------|---------|---------------|
| `TALC` | `allocator.rs` | Heap allocator | Always acquired with IRQs disabled |
| `POOL` | `threading.rs` | Thread pool | Scheduler context or `with_irqs_disabled` |
| `MOUNT_TABLE` | `vfs/mod.rs` | VFS mount points | Held during filesystem dispatch |
| `BLOCK_DEVICE` | `block.rs` | Block device I/O | Acquired by filesystem implementations |
| `IRQ_HANDLERS` | `irq.rs` | IRQ handler registry | Uses copy-out pattern |
| `IRQ_WORK_QUEUE` | `executor.rs` | Deferred work queue | Uses `try_lock`, may drop work |
| `HOST_KEY` | `ssh.rs` | SSH host key | Short-lived locks only |
| `FS_INITIALIZED` | `fs.rs` | FS init flag | Simple boolean flag |
| `NET_STATS` | `network.rs` | Network statistics | Counters only |
| `RTC` | `timer.rs` | Real-time clock | Read-only after init |
| `TICK_COUNT` | `timer.rs` | Tick counter | Legacy, rarely used |
| `UTC_OFFSET_US` | `timer.rs` | UTC time offset | Set once at boot |
| `FS_MUTEX` | `async_fs.rs` | Async filesystem | Embassy async mutex |

### Per-Instance Locks

| Lock | Module | Purpose |
|------|--------|---------|
| `Ext2Filesystem.state` | `vfs/ext2.rs` | Per-filesystem state |
| `MemoryFilesystem.root` | `vfs/memory.rs` | In-memory filesystem |
| `LoopbackDevice.state` | `embassy_net_driver.rs` | Loopback network device |
| `EmbassyTimeDriver.queue` | `embassy_time_driver.rs` | Timer wake queue |
| `EmbassyVirtioDriver.rx_waker/tx_waker` | `embassy_virtio_driver.rs` | Network wakers |

---

## Safe Patterns Used

### 1. IRQ-Disabled Allocation

All heap allocations disable IRQs to prevent deadlock if a context switch occurs mid-allocation:

```rust
// allocator.rs
unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    with_irqs_disabled(|| {
        TALC.lock().malloc(layout)...
    })
}
```

### 2. Copy-Out Before Handler Invocation

The IRQ dispatch copies the handler out before calling it:

```rust
// irq.rs
pub fn dispatch_irq(irq: u32) {
    let handler = {
        let handlers = IRQ_HANDLERS.lock();
        handlers.handlers.get(irq as usize).copied().flatten()
    }; // Lock released

    if let Some(handler) = handler {
        handler(irq); // Called without lock
    }
}
```

### 3. Drop Lock Before Wake

Network drivers release locks before waking waiters:

```rust
// embassy_net_driver.rs
if let Some(waker) = state.rx_waker.take() {
    drop(state); // Release lock first!
    waker.wake();
}
```

### 4. Thread Pool Access with IRQs Disabled

All non-scheduler access to the thread pool disables IRQs:

```rust
// threading.rs
pub fn spawn_with_options(...) -> Result<usize, &'static str> {
    with_irqs_disabled(|| {
        let mut pool = POOL.lock();
        pool.spawn(entry, cooperative)
    })
}
```

### 5. Critical Section Nesting

The critical section implementation supports nesting:

```rust
// embassy_time_driver.rs
static CS_NESTING: AtomicU8 = AtomicU8::new(0);

unsafe impl critical_section::Impl for CriticalSection {
    unsafe fn acquire() -> RawRestoreState {
        // Save DAIF only on first acquisition
        let nesting = CS_NESTING.fetch_add(1, Ordering::Relaxed);
        if nesting == 0 {
            CS_SAVED_DAIF.store(daif, Ordering::Relaxed);
        }
    }
}
```

---

## Known Issues and Warnings

### ⚠️ Medium Risk

#### 1. VFS Holds `MOUNT_TABLE` During Filesystem Operations

**Location**: `vfs/mod.rs:374-386`

```rust
fn with_fs<F, R>(path: &str, f: F) -> Result<R, FsError>
where
    F: FnOnce(&dyn Filesystem, &str) -> Result<R, FsError>,
{
    let table = MOUNT_TABLE.lock();
    // ...
    f(fs, relative_path) // Filesystem called with lock held!
}
```

**Risk**: If a filesystem implementation ever needs to access `MOUNT_TABLE` (e.g., for symlinks or cross-filesystem operations), this would deadlock.

**Mitigation**: Current filesystems (ext2, memory) don't access `MOUNT_TABLE`. Future implementations must maintain this invariant.

#### 2. Scheduler Uses Raw Pointer After Lock Release

**Location**: `threading.rs:555-574`

```rust
pub fn sgi_scheduler_handler(irq: u32) {
    let (switch_info, pool_ptr) = {
        let mut pool = POOL.lock();
        let ptr = &mut *pool as *mut ThreadPool;
        (pool.schedule_indices(voluntary), ptr)
    }; // Lock released

    if let Some((old_idx, new_idx)) = switch_info {
        unsafe {
            let pool = &mut *pool_ptr; // Raw pointer used after lock release
            // ...
        }
    }
}
```

**Risk**: Race condition if another thread modifies the pool between lock release and context switch.

**Mitigation**: Safe because this only runs from IRQ context and all other pool accesses use `with_irqs_disabled`.

### ⚠️ Low Risk

#### 3. `IRQ_WORK_QUEUE` Silently Drops Work

**Location**: `executor.rs:148-153`

```rust
pub fn queue_irq_work(work: IrqWork) {
    if let Some(mut queue) = IRQ_WORK_QUEUE.try_lock() {
        queue.push(work);
    }
    // Work silently dropped if lock unavailable!
}
```

**Risk**: Missed wakeups or lost events under high contention.

**Recommendation**: Consider logging dropped work or using a lock-free queue.

#### 4. Waker Invoked Inside Critical Sections

**Location**: `embassy_time_driver.rs:163-178`

```rust
critical_section::with(|cs| {
    // ...
    if let Some(waker) = entry.waker.take() {
        waker.wake(); // Called with IRQs disabled
    }
});
```

**Risk**: Increased interrupt latency if waker performs complex operations.

**Mitigation**: Embassy wakers are lightweight. Consider moving wake outside critical section for other waker types.

#### 5. Async FS Yields While Holding Mutex

**Location**: `async_fs.rs:65-67`

```rust
let _guard = FS_MUTEX.lock().await;
yield_now().await; // Yielding with mutex held!
fs::list_dir(path)
```

**Risk**: Reduced filesystem concurrency - all FS operations serialized.

**Note**: This is intentional for correctness but could be optimized if needed.

### ℹ️ Design Notes

#### 6. RefCell Without External Synchronization

**Location**: `embassy_virtio_driver.rs:54`

```rust
pub struct EmbassyVirtioDriver {
    rx_data: RefCell<RxData>, // No Mutex!
}
```

**Status**: Safe because the driver is only used from the single-threaded async main loop. Would panic if accessed from multiple threads.

---

## Guidelines for Contributors

### Do ✅

1. **Always acquire locks in hierarchy order** (MOUNT_TABLE → ext2.state → BLOCK_DEVICE → TALC)
2. **Use `with_irqs_disabled` when accessing `POOL`** from non-scheduler code
3. **Drop locks before calling wakers** to avoid latency
4. **Use critical sections for data shared with IRQ handlers**
5. **Keep critical sections short** to minimize interrupt latency

### Don't ❌

1. **Never hold spinlocks across `await` points** - use Embassy's async Mutex instead
2. **Never acquire higher-level locks while holding lower-level locks**
3. **Never allocate while holding locks** without IRQs disabled (allocator handles this)
4. **Never call potentially blocking code while holding spinlocks**
5. **Never access `POOL` from interrupt handlers** except through `sgi_scheduler_handler`

### Adding New Locks

When adding a new global lock:

1. Document it in this file with its purpose and level
2. Determine its position in the lock hierarchy
3. Ensure all acquisition sites follow the hierarchy
4. Consider whether it needs IRQ protection
5. Add appropriate comments in the code

### Testing for Deadlocks

The kernel currently doesn't have automated deadlock detection. When making changes:

1. Review the lock acquisition order in all affected code paths
2. Check for potential IRQ handler interactions
3. Test under load with preemption enabled
4. Watch for system hangs during testing

---

## Appendix: Lock Acquisition Call Graphs

### Filesystem Read Path

```
async_fs::read_file()
  └─ FS_MUTEX.lock().await
      └─ fs::read_file()
          └─ vfs::read_file()
              └─ MOUNT_TABLE.lock()
                  └─ ext2::read_file()
                      └─ ext2.state.lock()
                          └─ block::read_sectors()
                              └─ BLOCK_DEVICE.lock()
                                  └─ (allocations with IRQs disabled)
                                      └─ TALC.lock()
```

### Timer Interrupt Path

```
timer_irq_handler()
  └─ gic::trigger_sgi(SGI_SCHEDULER)
      └─ (SGI delivered)
          └─ sgi_scheduler_handler()
              └─ POOL.lock() (brief, then released)
                  └─ switch_context() (no locks held)
```

### Thread Spawn Path

```
threading::spawn_fn()
  └─ with_irqs_disabled()
      └─ POOL.lock()
          └─ (allocations for closure)
              └─ with_irqs_disabled() [nested, no-op]
                  └─ TALC.lock()
```

