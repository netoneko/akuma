//! Embassy-based async executor for bare-metal aarch64
//!
//! This module provides async task execution using Embassy's executor.
//! The executor integrates with our timer infrastructure for proper waking.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use embassy_executor::{Spawner, raw};

use crate::console;

// ============================================================================
// Executor Storage
// ============================================================================

/// Safe wrapper for executor storage
struct ExecutorStorage {
    inner: UnsafeCell<core::mem::MaybeUninit<raw::Executor>>,
    initialized: AtomicBool,
}

// SAFETY: We control access via the initialized flag and only access from main thread
unsafe impl Sync for ExecutorStorage {}

impl ExecutorStorage {
    const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(core::mem::MaybeUninit::uninit()),
            initialized: AtomicBool::new(false),
        }
    }

    fn init(&self) -> bool {
        if self.initialized.swap(true, Ordering::AcqRel) {
            return false; // Already initialized
        }

        // SAFETY: We're the only writer (protected by swap above)
        unsafe {
            (*self.inner.get()).write(raw::Executor::new(core::ptr::null_mut()));
        }
        true
    }

    fn get(&self) -> Option<&raw::Executor> {
        if self.initialized.load(Ordering::Acquire) {
            // SAFETY: Initialized and we only read
            Some(unsafe { (*self.inner.get()).assume_init_ref() })
        } else {
            None
        }
    }
}

static EXECUTOR: ExecutorStorage = ExecutorStorage::new();

// ============================================================================
// Public API
// ============================================================================

/// Initialize the Embassy executor
/// Must be called once before any async tasks can be spawned
pub fn init() {
    if EXECUTOR.init() {
        console::print("[Executor] Embassy executor initialized\n");
    }
}

/// Run the executor polling loop once
/// Call this regularly from the main loop or a dedicated thread
/// Returns true if executor is initialized and was polled
pub fn run_once() -> bool {
    let executor = match EXECUTOR.get() {
        Some(e) => e,
        None => return false,
    };

    // Poll all ready tasks
    // SAFETY: We're in single-threaded context and this is the only place we poll
    unsafe {
        executor.poll();
    }

    true
}

/// Get a spawner for creating new tasks
/// Returns None if executor is not initialized
pub fn spawner() -> Option<Spawner> {
    EXECUTOR.get().map(|e| e.spawner())
}

/// Run the executor until explicitly stopped
/// This is useful for running in a dedicated thread
pub fn run_blocking() {
    loop {
        run_once();
        // Yield to other threads
        crate::threading::yield_now();
    }
}

// ============================================================================
// Task Spawning Helpers
// ============================================================================

/// Check if executor is initialized and can spawn tasks
pub fn can_spawn() -> bool {
    EXECUTOR.get().is_some()
}

// ============================================================================
// Timer Integration
// ============================================================================

/// Call this from timer interrupt to process embassy time alarms
/// This triggers wakers for tasks waiting on embassy_time
pub fn on_timer_tick() {
    crate::embassy_time_driver::on_timer_interrupt();
}

// ============================================================================
// Legacy API (for compatibility)
// ============================================================================

/// Check if executor is initialized
pub fn has_tasks() -> bool {
    EXECUTOR.get().is_some()
}

// ============================================================================
// IRQ Work Queue (kept for interrupt -> task communication)
// ============================================================================

use alloc::vec::Vec;
use spinning_top::Spinlock;

/// Types of work that can be queued from interrupt context
pub enum IrqWork {
    /// Signal that the executor should poll
    RunExecutorOnce,
    /// Wake a specific task (by signaling via waker)
    WakeTask,
}

static IRQ_WORK_QUEUE: Spinlock<Vec<IrqWork>> = Spinlock::new(Vec::new());

/// Queue work from IRQ context (safe to call from interrupts)
pub fn queue_irq_work(work: IrqWork) {
    if let Some(mut queue) = IRQ_WORK_QUEUE.try_lock() {
        queue.push(work);
    }
}

/// Process pending IRQ work (call from main loop)
pub fn process_irq_work() {
    let work_items = {
        if let Some(mut queue) = IRQ_WORK_QUEUE.try_lock() {
            core::mem::take(&mut *queue)
        } else {
            return;
        }
    };

    for work in work_items {
        match work {
            IrqWork::RunExecutorOnce | IrqWork::WakeTask => {
                run_once();
            }
        }
    }
}
