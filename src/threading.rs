// Preemptive threading with fixed-size thread pool
// No dynamic allocation during spawn/cleanup - all memory pre-allocated at init

use core::arch::global_asm;
use spinning_top::Spinlock;

/// Default timeout for cooperative threads in microseconds (5 seconds)
pub const COOPERATIVE_TIMEOUT_US: u64 = 5_000_000;

/// Stack size per thread (32KB)
const STACK_SIZE: usize = 32 * 1024;

/// Maximum threads - with 32KB stacks, 32 threads = 1MB
/// Reasonable for 120MB heap
const MAX_THREADS: usize = 32;

/// Thread 0 is the boot/idle thread - always protected, never terminated
const IDLE_THREAD_IDX: usize = 0;

/// Run a closure with IRQs disabled to prevent scheduler lock deadlocks
#[inline]
fn with_irqs_disabled<T, F: FnOnce() -> T>(f: F) -> T {
    let daif: u64;
    unsafe {
        core::arch::asm!("mrs {}, daif", out(reg) daif);
        core::arch::asm!("msr daifset, #2"); // Disable IRQs
    }
    let result = f();
    unsafe {
        core::arch::asm!("msr daif, {}", in(reg) daif);
    }
    result
}

// Assembly context switch implementation
global_asm!(
    r#"
.section .text
.global switch_context
.global thread_start

// void switch_context(Context* old, const Context* new)
// x0 = pointer to old context (save here)
// x1 = pointer to new context (load from here)
switch_context:
    // Save old context
    stp x19, x20, [x0, #0]
    stp x21, x22, [x0, #16]
    stp x23, x24, [x0, #32]
    stp x25, x26, [x0, #48]
    stp x27, x28, [x0, #64]
    stp x29, x30, [x0, #80]
    
    // Save stack pointer
    mov x9, sp
    str x9, [x0, #96]
    
    // Save DAIF (interrupt mask)
    mrs x9, daif
    str x9, [x0, #104]
    
    // Save ELR_EL1 (exception return address)
    mrs x9, elr_el1
    str x9, [x0, #112]
    
    // Save SPSR_EL1 (exception saved processor state)
    mrs x9, spsr_el1
    str x9, [x0, #120]
    
    // Load new context
    ldp x19, x20, [x1, #0]
    ldp x21, x22, [x1, #16]
    ldp x23, x24, [x1, #32]
    ldp x25, x26, [x1, #48]
    ldp x27, x28, [x1, #64]
    ldp x29, x30, [x1, #80]
    
    // Load stack pointer
    ldr x9, [x1, #96]
    mov sp, x9
    
    // Load DAIF
    ldr x9, [x1, #104]
    msr daif, x9
    
    // Load ELR_EL1
    ldr x9, [x1, #112]
    msr elr_el1, x9
    
    // Load SPSR_EL1
    ldr x9, [x1, #120]
    msr spsr_el1, x9
    
    // Return
    ret

// Thread entry trampoline
// x19 holds the actual thread entry function
thread_start:
    // Enable IRQs for this thread
    msr daifclr, #2
    
    // Call the thread entry function (in x19)
    blr x19
    
    // Thread returned - mark as terminated and yield
    // (This shouldn't happen for -> ! functions, but just in case)
    b thread_exit_asm

thread_exit_asm:
    wfi
    b thread_exit_asm
"#
);

// External assembly functions
unsafe extern "C" {
    fn switch_context(old: *mut Context, new: *const Context);
    fn thread_start() -> !;
}

/// CPU context saved during context switch
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Context {
    pub x19: u64,
    pub x20: u64,
    pub x21: u64,
    pub x22: u64,
    pub x23: u64,
    pub x24: u64,
    pub x25: u64,
    pub x26: u64,
    pub x27: u64,
    pub x28: u64,
    pub x29: u64,  // Frame pointer
    pub x30: u64,  // Link register (return address)
    pub sp: u64,   // Stack pointer
    pub daif: u64, // Interrupt mask
    pub elr: u64,  // Exception Link Register
    pub spsr: u64, // Saved Program Status Register
}

impl Context {
    pub const fn zero() -> Self {
        Self {
            x19: 0,
            x20: 0,
            x21: 0,
            x22: 0,
            x23: 0,
            x24: 0,
            x25: 0,
            x26: 0,
            x27: 0,
            x28: 0,
            x29: 0,
            x30: 0,
            sp: 0,
            daif: 0,
            elr: 0,
            spsr: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    Free,       // Slot is available
    Ready,      // Ready to run
    Running,    // Currently running
    Terminated, // Finished, slot can be reclaimed
}

/// Thread slot in the pool
#[repr(C)]
pub struct ThreadSlot {
    pub state: ThreadState,
    pub context: Context,
    pub cooperative: bool,
    pub start_time_us: u64,
    pub timeout_us: u64,
}

impl ThreadSlot {
    pub const fn empty() -> Self {
        Self {
            state: ThreadState::Free,
            context: Context::zero(),
            cooperative: false,
            start_time_us: 0,
            timeout_us: 0,
        }
    }
}

/// Fixed-size thread pool with pre-allocated stacks
pub struct ThreadPool {
    slots: [ThreadSlot; MAX_THREADS],
    stacks: [usize; MAX_THREADS], // Pointers to pre-allocated stacks
    current_idx: usize,
    initialized: bool,
}

impl ThreadPool {
    pub const fn new() -> Self {
        Self {
            slots: [const { ThreadSlot::empty() }; MAX_THREADS],
            stacks: [0; MAX_THREADS],
            current_idx: 0,
            initialized: false,
        }
    }

    /// Initialize the pool - allocate all stacks upfront
    pub fn init(&mut self) {
        use alloc::alloc::{Layout, alloc_zeroed};

        // Slot 0 is the idle/boot thread (uses boot stack, never terminated)
        self.slots[IDLE_THREAD_IDX].state = ThreadState::Running;
        self.stacks[IDLE_THREAD_IDX] = 0; // Boot stack, don't allocate

        // Pre-allocate stacks for all other slots
        let layout = Layout::from_size_align(STACK_SIZE, 16).unwrap();
        for i in 1..MAX_THREADS {
            let stack = unsafe { alloc_zeroed(layout) as usize };
            if stack == 0 {
                panic!("Failed to allocate thread stack {}", i);
            }
            self.stacks[i] = stack;
        }

        self.initialized = true;
    }

    /// Spawn a new thread
    pub fn spawn(
        &mut self,
        entry: extern "C" fn() -> !,
        cooperative: bool,
    ) -> Result<usize, &'static str> {
        if !self.initialized {
            return Err("Thread pool not initialized");
        }

        // Find first free slot (skip slot 0 = idle)
        for i in 1..MAX_THREADS {
            if self.slots[i].state == ThreadState::Free {
                // Setup the thread
                let stack_base = self.stacks[i];
                let stack_top = stack_base + STACK_SIZE;
                let sp = (stack_top & !0xF) as u64; // 16-byte aligned

                let entry_addr = entry as *const () as u64;

                let mut ctx = Context::zero();
                ctx.sp = sp;
                ctx.x19 = entry_addr;
                ctx.x30 = thread_start as *const () as u64;
                ctx.x29 = 0;

                self.slots[i] = ThreadSlot {
                    state: ThreadState::Ready,
                    context: ctx,
                    cooperative,
                    start_time_us: 0,
                    timeout_us: if cooperative {
                        COOPERATIVE_TIMEOUT_US
                    } else {
                        0
                    },
                };

                return Ok(i);
            }
        }

        Err("No free thread slots")
    }

    /// Reclaim a terminated thread slot (just mark as Free)
    pub fn reclaim(&mut self, idx: usize) {
        if idx > 0 && idx < MAX_THREADS && self.slots[idx].state == ThreadState::Terminated {
            self.slots[idx].state = ThreadState::Free;
            // Stack stays allocated - will be reused
        }
    }

    /// Clean up all terminated threads
    pub fn cleanup_terminated(&mut self) -> usize {
        let mut count = 0;
        for i in 1..MAX_THREADS {
            if self.slots[i].state == ThreadState::Terminated {
                self.slots[i].state = ThreadState::Free;
                count += 1;
            }
        }
        count
    }

    /// Select next ready thread (round-robin)
    pub fn schedule_indices(&mut self, voluntary: bool) -> Option<(usize, usize)> {
        let current_idx = self.current_idx;
        let current = &self.slots[current_idx];

        // Check cooperative timeout
        if !voluntary && current.cooperative && current.state == ThreadState::Running {
            let timeout = current.timeout_us;
            if timeout > 0 && current.start_time_us > 0 {
                let now = crate::timer::uptime_us();
                let elapsed = now.saturating_sub(current.start_time_us);
                if elapsed < timeout {
                    return None; // Not timed out yet
                }
            } else {
                return None; // No timeout, can't preempt
            }
        }

        // Find next ready thread
        let mut next_idx = (current_idx + 1) % MAX_THREADS;
        if next_idx == 0 {
            next_idx = 1;
        } // Skip idle
        let start_idx = next_idx;

        loop {
            if next_idx != 0 {
                let state = self.slots[next_idx].state;
                if state == ThreadState::Ready || state == ThreadState::Running {
                    break;
                }
            }

            next_idx = (next_idx + 1) % MAX_THREADS;
            if next_idx == 0 {
                next_idx = 1;
            }

            if next_idx == start_idx {
                return None; // No ready threads
            }
        }

        if next_idx == current_idx {
            return None;
        }

        // Update states (thread 0 stays Running, never set to Ready)
        if current_idx != IDLE_THREAD_IDX
            && self.slots[current_idx].state != ThreadState::Terminated
        {
            self.slots[current_idx].state = ThreadState::Ready;
        }
        self.slots[next_idx].state = ThreadState::Running;
        self.slots[next_idx].start_time_us = crate::timer::uptime_us();

        self.current_idx = next_idx;
        Some((current_idx, next_idx))
    }

    pub fn thread_stats(&self) -> (usize, usize, usize) {
        let mut ready = 0;
        let mut running = 0;
        let mut terminated = 0;
        for slot in &self.slots {
            match slot.state {
                ThreadState::Free => {}
                ThreadState::Ready => ready += 1,
                ThreadState::Running => running += 1,
                ThreadState::Terminated => terminated += 1,
            }
        }
        (ready, running, terminated)
    }

    pub fn thread_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.state != ThreadState::Free)
            .count()
    }

    pub unsafe fn get_context_ptrs(
        &mut self,
        old_idx: usize,
        new_idx: usize,
    ) -> (*mut Context, *const Context) {
        let old_ptr = &mut self.slots[old_idx].context as *mut Context;
        let new_ptr = &self.slots[new_idx].context as *const Context;
        (old_ptr, new_ptr)
    }
}

static POOL: Spinlock<ThreadPool> = Spinlock::new(ThreadPool::new());
static mut VOLUNTARY_SCHEDULE: bool = false;

/// Initialize the thread pool
pub fn init() {
    let mut pool = POOL.lock();
    pool.init();
}

/// Spawn a new preemptible thread
pub fn spawn(entry: extern "C" fn() -> !) -> Result<usize, &'static str> {
    spawn_with_options(entry, false)
}

/// Spawn a cooperative thread (only yields voluntarily)
pub fn spawn_cooperative(entry: extern "C" fn() -> !) -> Result<usize, &'static str> {
    spawn_with_options(entry, true)
}

/// Spawn with options
pub fn spawn_with_options(
    entry: extern "C" fn() -> !,
    cooperative: bool,
) -> Result<usize, &'static str> {
    with_irqs_disabled(|| {
        let mut pool = POOL.lock();
        pool.spawn(entry, cooperative)
    })
}

/// SGI handler for scheduling
pub fn sgi_scheduler_handler(irq: u32) {
    crate::gic::end_of_interrupt(irq);

    let voluntary = unsafe { VOLUNTARY_SCHEDULE };
    unsafe {
        VOLUNTARY_SCHEDULE = false;
    }

    let (switch_info, pool_ptr) = {
        let mut pool = POOL.lock();
        let ptr = &mut *pool as *mut ThreadPool;
        (pool.schedule_indices(voluntary), ptr)
    };

    if let Some((old_idx, new_idx)) = switch_info {
        unsafe {
            let pool = &mut *pool_ptr;
            let (old_ptr, new_ptr) = pool.get_context_ptrs(old_idx, new_idx);
            switch_context(old_ptr, new_ptr);
        }
    }
}

/// Yield to another thread
pub fn yield_now() {
    unsafe {
        VOLUNTARY_SCHEDULE = true;
    }
    crate::gic::trigger_sgi(crate::gic::SGI_SCHEDULER);
}

/// Get thread stats (ready, running, terminated)
pub fn thread_stats() -> (usize, usize, usize) {
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        pool.thread_stats()
    })
}

/// Clean up terminated threads (mark slots as free)
pub fn cleanup_terminated() -> usize {
    with_irqs_disabled(|| {
        let mut pool = POOL.lock();
        pool.cleanup_terminated()
    })
}

/// Get active thread count
pub fn thread_count() -> usize {
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        pool.thread_count()
    })
}

/// Mark current thread as terminated (thread 0 cannot be terminated)
pub fn mark_current_terminated() {
    with_irqs_disabled(|| {
        let mut pool = POOL.lock();
        let idx = pool.current_idx;
        // Never allow terminating thread 0 (boot/idle thread)
        if idx != IDLE_THREAD_IDX {
            pool.slots[idx].state = ThreadState::Terminated;
        }
    })
}

/// Get current thread ID
pub fn current_thread_id() -> usize {
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        pool.current_idx
    })
}

/// Get max thread count
pub fn max_threads() -> usize {
    MAX_THREADS
}
