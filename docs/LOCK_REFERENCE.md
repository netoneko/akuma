# Lock Quick Reference

Quick reference card for Akuma's synchronization primitives.

## Lock Hierarchy (acquire in this order)

```
┌─────────────────────────────────────────────────────────────┐
│  Level 1: MOUNT_TABLE                                       │
│  ─────────────────────────────────────────────────────────  │
│  Level 2: ext2.state, MemoryFilesystem.root                 │
│  ─────────────────────────────────────────────────────────  │
│  Level 3: BLOCK_DEVICE                                      │
│  ─────────────────────────────────────────────────────────  │
│  Level 4: TALC (always with IRQs disabled)                  │
└─────────────────────────────────────────────────────────────┘

Special:
  • POOL         → scheduler context OR with_irqs_disabled()
  • IRQ_HANDLERS → copy-out pattern (release before calling)
  • FS_MUTEX     → async mutex (can hold across await)
```

## Quick Rules

| Situation | Do This |
|-----------|---------|
| Accessing thread pool | `with_irqs_disabled(\|\| POOL.lock())` |
| Allocating memory | Just allocate (IRQs auto-disabled) |
| Waking a waker | Drop lock first, then `waker.wake()` |
| Shared with IRQ handler | Use `critical_section::with()` |
| Async mutual exclusion | Use `embassy_sync::mutex::Mutex` |
| Reading hardware timer | Direct access, no lock needed |

## Lock Locations

| Lock | File | Type |
|------|------|------|
| `TALC` | `allocator.rs:9` | `Spinlock<Talc>` |
| `POOL` | `threading.rs:462` | `Spinlock<ThreadPool>` |
| `MOUNT_TABLE` | `vfs/mod.rs:218` | `Spinlock<Option<MountTable>>` |
| `BLOCK_DEVICE` | `block.rs:217` | `Spinlock<Option<VirtioBlockDevice>>` |
| `IRQ_HANDLERS` | `irq.rs:61` | `Spinlock<IrqHandlers>` |
| `IRQ_WORK_QUEUE` | `executor.rs:146` | `Spinlock<Vec<IrqWork>>` |
| `FS_MUTEX` | `async_fs.rs:23` | `Mutex<CriticalSectionRawMutex, ()>` |
| `HOST_KEY` | `ssh.rs:75` | `Spinlock<Option<SigningKey>>` |

## Common Patterns

### Safe: IRQ-protected pool access
```rust
with_irqs_disabled(|| {
    let mut pool = POOL.lock();
    pool.spawn(entry, cooperative)
})
```

### Safe: Copy-out before handler call
```rust
let handler = {
    let handlers = IRQ_HANDLERS.lock();
    handlers.get(irq).copied()
}; // lock released
handler.map(|h| h(irq));
```

### Safe: Drop before wake
```rust
if let Some(waker) = state.rx_waker.take() {
    drop(state);
    waker.wake();
}
```

### UNSAFE: Holding lock across await
```rust
// ❌ DON'T DO THIS with Spinlock
let guard = SPINLOCK.lock();
some_future.await; // DEADLOCK RISK!
drop(guard);

// ✅ DO THIS with async Mutex
let guard = ASYNC_MUTEX.lock().await;
some_future.await; // OK!
drop(guard);
```

### UNSAFE: Wrong lock order
```rust
// ❌ DON'T DO THIS
let block = BLOCK_DEVICE.lock();
let mount = MOUNT_TABLE.lock(); // WRONG ORDER!

// ✅ DO THIS
let mount = MOUNT_TABLE.lock();
// ... later, after using mount ...
let block = BLOCK_DEVICE.lock();
```

## Debugging Deadlocks

If the system hangs:

1. **Check lock order**: Are locks acquired in hierarchy order?
2. **Check IRQ context**: Is code trying to lock from an IRQ handler?
3. **Check for recursion**: Is the same lock being acquired twice?
4. **Check await points**: Is a spinlock held across an await?

## Files to Review

When adding new locks or modifying concurrency:

- `docs/CONCURRENCY.md` - Full documentation
- `src/irq.rs` - IRQ guard implementation
- `src/allocator.rs` - IRQ-safe allocation
- `src/threading.rs` - Thread pool and scheduler
- `src/embassy_time_driver.rs` - Critical section impl

