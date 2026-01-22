//! Embassy-based async executor for bare-metal aarch64
//!
//! This module provides async task execution using Embassy's executor.
//! The executor integrates with our timer infrastructure for proper waking.
//!
//! ## ARM WFE/SEV Integration
//!
//! This executor uses ARM's Wait-For-Event (WFE) and Send-Event (SEV)
//! instructions for efficient waking:
//! - WFE puts the CPU in low-power state until an event is signaled
//! - SEV signals all cores to wake from WFE
//! - This avoids busy-polling while maintaining responsiveness

use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
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
/// 
/// Uses ARM WFE for efficient waiting between polls
pub fn run_blocking() {
    loop {
        run_once();
        // Wait for event (interrupt or SEV from waker)
        // This is more efficient than busy-polling
        wait_for_event();
    }
}

// ============================================================================
// ARM WFE/SEV Integration
// ============================================================================

/// Wait for an event using ARM WFE instruction
/// 
/// This puts the CPU in a low-power state until:
/// - An interrupt occurs
/// - SEV is executed (by signal_wake or another core)
/// - A spurious wakeup occurs
/// 
/// This is more efficient than busy-polling or yield_now()
#[inline(always)]
pub fn wait_for_event() {
    unsafe { asm!("wfe") }
}

/// Signal that async work is ready using ARM SEV instruction
/// 
/// This wakes any cores waiting in WFE, including:
/// - The executor's run_blocking loop
/// - Other cores waiting for work
/// 
/// Call this after waking a task (via Waker::wake) to ensure
/// the executor processes it promptly.
#[inline(always)]
pub fn signal_wake() {
    unsafe { asm!("sev") }
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

/// Types of work that can be queued from interrupt context
#[derive(Clone, Copy)]
pub enum IrqWork {
    /// No work (empty slot)
    None,
    /// Signal that the executor should poll
    RunExecutorOnce,
    /// Wake a specific task (by signaling via waker)
    WakeTask,
}

/// Lock-free IRQ work queue using a static ring buffer
/// No heap allocation - safe for IRQ context
const IRQ_QUEUE_SIZE: usize = 16;

static IRQ_WORK_QUEUE: [AtomicU8; IRQ_QUEUE_SIZE] = {
    const INIT: AtomicU8 = AtomicU8::new(0);
    [INIT; IRQ_QUEUE_SIZE]
};

static IRQ_QUEUE_HEAD: AtomicUsize = AtomicUsize::new(0);
static IRQ_QUEUE_TAIL: AtomicUsize = AtomicUsize::new(0);

impl IrqWork {
    fn to_u8(self) -> u8 {
        match self {
            IrqWork::None => 0,
            IrqWork::RunExecutorOnce => 1,
            IrqWork::WakeTask => 2,
        }
    }
    
    fn from_u8(v: u8) -> Self {
        match v {
            1 => IrqWork::RunExecutorOnce,
            2 => IrqWork::WakeTask,
            _ => IrqWork::None,
        }
    }
}

/// Queue work from IRQ context (safe to call from interrupts - no allocation!)
pub fn queue_irq_work(work: IrqWork) {
    // Simple ring buffer push - if full, just drop the work
    let tail = IRQ_QUEUE_TAIL.load(Ordering::Relaxed);
    let next_tail = (tail + 1) % IRQ_QUEUE_SIZE;
    let head = IRQ_QUEUE_HEAD.load(Ordering::Relaxed);
    
    // Check if queue is full
    if next_tail == head {
        return; // Queue full, drop work
    }
    
    // Store the work item
    IRQ_WORK_QUEUE[tail].store(work.to_u8(), Ordering::Release);
    IRQ_QUEUE_TAIL.store(next_tail, Ordering::Release);
}

/// Process pending IRQ work (call from main loop)
pub fn process_irq_work() {
    loop {
        let head = IRQ_QUEUE_HEAD.load(Ordering::Acquire);
        let tail = IRQ_QUEUE_TAIL.load(Ordering::Acquire);
        
        if head == tail {
            break; // Queue empty
        }
        
        let work = IrqWork::from_u8(IRQ_WORK_QUEUE[head].load(Ordering::Acquire));
        IRQ_QUEUE_HEAD.store((head + 1) % IRQ_QUEUE_SIZE, Ordering::Release);
        
        match work {
            IrqWork::RunExecutorOnce | IrqWork::WakeTask => {
                run_once();
            }
            IrqWork::None => {}
        }
    }
}
