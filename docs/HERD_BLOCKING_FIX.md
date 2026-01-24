# Herd Process Blocking Fix (January 2026)

This document describes a series of blocking/deadlock issues encountered when spawning the herd process supervisor from the kernel's main async loop, and the fixes applied.

## Problem Summary

When `AUTO_START_HERD` was enabled in `src/config.rs`, the kernel would hang shortly after spawning the herd process. The symptoms evolved as each layer of issues was fixed:

1. **Timer conflict** - Embassy time driver and scheduler using the same hardware timer
2. **Interrupt storm** - Virtual timer firing continuously without being disabled
3. **Busy-wait loop** - Main loop polling herd's output channel in a tight loop
4. **Priority inversion deadlock** - Preemption-disabled section blocking on VFS lock held by herd

## Root Cause Analysis

### Issue 1: Timer Conflict

**Symptom**: User processes would block forever on `sleep_ms()` calls.

**Cause**: Both the kernel scheduler (using physical timer CNTP) and Embassy time driver were configured to use the same timer. The Embassy driver was modified to use the virtual timer (CNTV) but IRQ 27 wasn't enabled.

**Fix** (`src/main.rs`):
```rust
// Register virtual timer IRQ handler for Embassy
irq::register_handler(27, |_irq| {
    embassy_time_driver::on_timer_interrupt();
});
gic::enable_irq(27);
```

### Issue 2: Virtual Timer Interrupt Storm

**Symptom**: Log showed `[VIRT-TIMER] IRQ 27 fired` continuously with count increasing rapidly.

**Cause**: When Embassy had no pending alarms, `update_hardware_timer_locked()` set `earliest = u64::MAX` but didn't disable the timer. The timer remained armed at a stale value and kept firing.

**Fix** (`src/embassy_time_driver.rs`):
```rust
fn update_hardware_timer_locked(&self) {
    let earliest = /* ... find earliest alarm ... */;
    
    if earliest != u64::MAX {
        // Arm the timer
        unsafe {
            asm!("msr cntv_cval_el0, {}", in(reg) earliest);
            asm!("msr cntv_ctl_el0, {}", in(reg) 1u64); // Enable
        }
    } else {
        // CRITICAL: Disable timer when no alarms pending
        unsafe {
            asm!("msr cntv_ctl_el0, {}", in(reg) 0u64); // Disable
        }
    }
}
```

### Issue 3: Herd Output Busy-Wait

**Symptom**: Log showed `[Herd] Reading output... [Herd] Finished reading output: 0 bytes` in a tight loop.

**Cause**: The main async loop used `channel.read_all()` which returns an empty `Vec` immediately when no data is available, causing a busy-wait. The loop also never detected when herd exited.

**Fix** (`src/main.rs`):
```rust
// Before: let (herd_tid, herd_channel) = ...
// After:
let (_herd_tid, mut herd_channel) = /* spawn herd */;

// In the polling loop:
if let Some(ref channel) = herd_channel {
    // Use try_read() instead of read_all() - returns None if no data
    if let Some(output) = channel.try_read() {
        for &byte in &output { console::print_char(byte as char); }
    }
    
    // Check for process exit
    if channel.has_exited() {
        let output = channel.read_all(); // Drain remaining output
        if !output.is_empty() {
            for &byte in &output { console::print_char(byte as char); }
        }
        let exit_code = channel.exit_code();
        crate::safe_print!(64, "[Herd] Process exited with code {}\n", exit_code);
        herd_channel = None; // Stop polling
    }
}
```

### Issue 4: Priority Inversion Deadlock (VFS Lock)

**Symptom**: Watchdog reported `Preemption disabled for 5000ms+ at step 3` (SSH server poll).

**Cause**: Classic priority inversion:
1. Thread 8 (herd) acquires VFS spinlock to create `/etc/herd/enabled`
2. Timer IRQ fires, scheduler switches to Thread 1 (main async loop)
3. Thread 1 disables preemption for embassy-net polling
4. SSH server poll calls `init_host_key_async()` → `async_fs::read_file()` → VFS lock
5. Thread 1 spins waiting for lock, but preemption is disabled
6. Thread 8 can never run to release the lock → **deadlock**

**Fix** (`src/ssh/server.rs` + `src/main.rs`):

Added initialization tracking flag:
```rust
static SSH_INIT_DONE: AtomicBool = AtomicBool::new(false);

pub fn is_initialized() -> bool {
    SSH_INIT_DONE.load(Ordering::Acquire)
}

pub async fn run(stack: Stack<'static>) {
    // Filesystem I/O during initialization
    protocol::init_host_key_async().await;
    super::config::ensure_default_config().await;
    super::config::load_config().await;
    
    // Mark initialization complete
    SSH_INIT_DONE.store(true, Ordering::Release);
    
    // Now safe to run with preemption disabled (accept loop only)
    loop { /* accept connections */ }
}
```

Pre-initialization loop with preemption **enabled**:
```rust
// Poll SSH server with preemption ENABLED until init completes
while !ssh::server::is_initialized() {
    let _ = runner_pinned.as_mut().poll(&mut cx);
    let _ = loopback_runner_pinned.as_mut().poll(&mut cx);
    let _ = ssh_pinned.as_mut().poll(&mut cx);
    executor::process_irq_work();
    executor::run_once();
    threading::yield_now(); // Allow other threads to run
}

// Now safe to enter main loop with preemption-disabled sections
loop {
    threading::disable_preemption();
    // ... poll embassy-net (uses RefCells, needs preemption disabled) ...
    threading::enable_preemption();
    // ...
}
```

## Why Preemption Must Be Disabled for Embassy-Net

Embassy-net uses `RefCell` for interior mutability in its network stack. If a timer interrupt preempts mid-borrow and the interrupt handler tries to borrow the same `RefCell`, it will panic ("already borrowed").

The solution is to disable preemption during embassy-net polling, but **only** for the network operations. Filesystem I/O must run with preemption enabled to avoid deadlocks with other threads.

## Debugging Tools Added

### Poll Step Tracking

A global counter tracks which step of the main loop is executing:
```rust
pub static GLOBAL_POLL_STEP: AtomicU64 = AtomicU64::new(0);

// In main loop:
GLOBAL_POLL_STEP.store(1, Ordering::Relaxed);
let _ = runner_pinned.as_mut().poll(&mut cx);  // Step 1: Network runner
GLOBAL_POLL_STEP.store(2, Ordering::Relaxed);
let _ = loopback_runner_pinned.as_mut().poll(&mut cx);  // Step 2: Loopback
GLOBAL_POLL_STEP.store(3, Ordering::Relaxed);
let _ = ssh_pinned.as_mut().poll(&mut cx);  // Step 3: SSH server
// ... etc
```

### Watchdog with Step Reporting

The preemption watchdog reports which step is blocking:
```rust
// In timer.rs:
let step = crate::GLOBAL_POLL_STEP.load(Ordering::Relaxed);
crate::safe_print!(96, "[WATCHDOG] Preemption disabled for {}ms at step {}\n", 
    duration_us / 1000, step);
```

This immediately identifies which poll operation is stuck.

## Key Lessons

1. **Don't hold preemption disabled across lock acquisitions** - If another thread holds the lock, you'll deadlock.

2. **Spinlocks + preemption disabled = danger** - The holder can't run to release the lock.

3. **Async doesn't mean non-blocking** - `async_fs` calls synchronous `fs::` functions that acquire spinlocks.

4. **Separate initialization from steady-state** - Initialization often does I/O; steady-state should be lock-free.

5. **Watchdogs are essential** - Without the preemption watchdog, this would have been extremely hard to debug.

## Files Modified

- `src/main.rs` - Pre-initialization loop, poll step tracking
- `src/ssh/server.rs` - `SSH_INIT_DONE` flag, `is_initialized()`
- `src/embassy_time_driver.rs` - Disable timer when no alarms
- `src/timer.rs` - Watchdog step reporting
- `src/threading.rs` - Removed debug prints

## Related Documentation

- [CONCURRENCY.md](CONCURRENCY.md) - Threading model overview
- [HERD.md](HERD.md) - Herd supervisor design and API
- [LOCK_REFERENCE.md](LOCK_REFERENCE.md) - All spinlocks in the kernel
