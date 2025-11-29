// Preemptive threading implementation
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::arch::global_asm;
use spinning_top::Spinlock;

/// Default timeout for cooperative threads in microseconds (5 seconds)
/// If 0, timeout is never enforced
pub const COOPERATIVE_TIMEOUT_US: u64 = 5_000_000;

/// Short timeout for test threads (500ms)
pub const TEST_TIMEOUT_US: u64 = 500_000;

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
    // Enable IRQs
    msr daifclr, #2
    
    // Call the thread entry function (in x19)
    blr x19
    
    // Thread returned (shouldn't happen!) - loop forever
1:  wfi
    b 1b

"#
);

const STACK_SIZE: usize = 64 * 1024; // 64KB per thread (smaller for now)
const MAX_THREADS: usize = 8;

// External assembly functions
unsafe extern "C" {
    fn switch_context(old: *mut Context, new: *const Context);
    fn thread_start() -> !;
}

/// CPU context saved during context switch
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Context {
    // Callee-saved registers (x19-x29, x30)
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
    pub elr: u64,  // Exception Link Register (return address for eret)
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
            daif: 0,  // IRQs enabled
            elr: 0,   // Exception return address
            spsr: 0,  // Processor state
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    Ready,
    Running,
    Terminated,
}

pub struct Thread {
    pub id: usize,
    pub context: Context,
    pub stack_ptr: usize,  // Raw pointer to allocated stack (0 for idle thread)
    pub stack_size: usize, // Size of allocated stack (0 for idle thread)
    pub state: ThreadState,
    pub cooperative: bool,     // If true, only yields voluntarily (not preempted by timer)
    pub start_time_us: u64,    // When thread started running (for timeout)
    pub timeout_us: u64,       // Timeout for cooperative threads (0 = no timeout)
}

impl Thread {
    /// Create a new thread with the given entry point
    /// cooperative: if true, thread only yields voluntarily (not preempted by timer)
    pub fn new(id: usize, entry: extern "C" fn() -> !, cooperative: bool) -> Self {
        use alloc::alloc::{Layout, alloc_zeroed};

        // Allocate zeroed stack using raw allocator
        let layout = Layout::from_size_align(STACK_SIZE, 16).unwrap();
        let stack_ptr = unsafe { alloc_zeroed(layout) as usize };

        if stack_ptr == 0 {
            panic!("Failed to allocate thread stack");
        }

        let stack_top = stack_ptr + STACK_SIZE;
        let sp = (stack_top & !0xF) as u64; // Align to 16 bytes

        let entry_addr = entry as *const () as u64;

        // Initialize context
        // x30 points to thread_start trampoline, x19 holds actual entry function
        // thread_start will call the entry with blr x19, enabling proper preemption
        let mut context = Context::zero();
        context.sp = sp;
        context.x19 = entry_addr; // Pass entry function in x19
        context.x30 = thread_start as *const () as u64; // Jump to trampoline
        context.x29 = 0; // Frame pointer = 0 for new thread

        Thread {
            id,
            context,
            stack_ptr,
            stack_size: STACK_SIZE,
            state: ThreadState::Ready,
            cooperative,
            start_time_us: 0,
            timeout_us: if cooperative { COOPERATIVE_TIMEOUT_US } else { 0 },
        }
    }

    /// Create the initial "idle" thread (represents main execution)
    pub fn new_idle() -> Self {
        Thread {
            id: 0,
            context: Context::zero(),
            stack_ptr: 0, // Uses boot stack
            stack_size: 0,
            state: ThreadState::Running,
            cooperative: false,
            start_time_us: 0,
            timeout_us: 0,
        }
    }
}

pub struct Scheduler {
    threads: Vec<Thread>,
    current_idx: usize,
}

impl Scheduler {
    pub fn new() -> Self {
        // Create Vec with idle thread directly
        let idle = Thread::new_idle();
        let mut threads = Vec::new();
        threads.push(idle);
        threads.reserve(MAX_THREADS - 1);

        Scheduler {
            threads,
            current_idx: 0,
        }
    }

    /// Spawn a new thread
    /// cooperative: if true, thread only yields voluntarily (not preempted by timer)
    pub fn spawn(&mut self, entry: extern "C" fn() -> !, cooperative: bool) -> Result<usize, &'static str> {
        if self.threads.len() >= MAX_THREADS {
            return Err("Too many threads");
        }

        let tid = self.threads.len();
        let thread = Thread::new(tid, entry, cooperative);
        self.threads.push(thread);

        Ok(tid)
    }

    pub fn current_thread(&self) -> &Thread {
        &self.threads[self.current_idx]
    }

    pub fn current_thread_mut(&mut self) -> &mut Thread {
        &mut self.threads[self.current_idx]
    }

    /// Select next ready thread (round-robin)
    /// voluntary: if false (timer preemption), skip cooperative threads (unless timed out)
    /// Returns (old_idx, new_idx)
    pub fn schedule_indices(&mut self, voluntary: bool) -> Option<(usize, usize)> {
        // Skip if only one thread
        if self.threads.len() <= 1 {
            return None;
        }

        let current_idx = self.current_idx;
        let current_thread = &self.threads[current_idx];
        
        // Check if cooperative thread should be preempted due to timeout
        let mut force_preempt = false;
        if !voluntary && current_thread.cooperative {
            let timeout = current_thread.timeout_us;
            if timeout > 0 && current_thread.start_time_us > 0 {
                let now = crate::timer::uptime_us();
                let elapsed = now.saturating_sub(current_thread.start_time_us);
                if elapsed >= timeout {
                    force_preempt = true; // Timeout! Force preemption
                }
            }
            
            if !force_preempt {
                return None; // Cooperative thread can't be preempted (no timeout)
            }
        }

        let thread_count = self.threads.len();

        // Find next ready thread (skip idle thread 0)
        let mut next_idx = (current_idx + 1) % thread_count;
        if next_idx == 0 {
            next_idx = 1; // Skip idle
        }
        let start_idx = next_idx;

        loop {
            // Skip idle thread (thread 0)
            if next_idx != 0 {
                let state = self.threads[next_idx].state;
                if state == ThreadState::Ready || state == ThreadState::Running {
                    break;
                }
            }

            next_idx = (next_idx + 1) % thread_count;
            if next_idx == 0 {
                next_idx = 1; // Skip idle
            }

            // Wrapped around - no ready threads
            if next_idx == start_idx {
                return None;
            }
        }

        // Don't switch if we're staying on same thread
        if next_idx == current_idx {
            return None;
        }

        // Update states - mark old thread as Ready (unless terminated)
        if self.threads[current_idx].state != ThreadState::Terminated {
            self.threads[current_idx].state = ThreadState::Ready;
        }
        self.threads[next_idx].state = ThreadState::Running;
        
        // Record when this thread started running
        self.threads[next_idx].start_time_us = crate::timer::uptime_us();

        self.current_idx = next_idx;

        Some((current_idx, next_idx))
    }
    
    /// Get thread count statistics
    pub fn thread_stats(&self) -> (usize, usize, usize) {
        let mut ready = 0;
        let mut running = 0;
        let mut terminated = 0;
        for t in &self.threads {
            match t.state {
                ThreadState::Ready => ready += 1,
                ThreadState::Running => running += 1,
                ThreadState::Terminated => terminated += 1,
            }
        }
        (ready, running, terminated)
    }
    
    /// Clean up terminated threads and free their stacks
    pub fn cleanup_terminated(&mut self) -> usize {
        use alloc::alloc::{Layout, dealloc};
        
        let mut cleaned = 0;
        // Can't remove while iterating, mark indices to clean
        let to_clean: Vec<usize> = self.threads.iter()
            .enumerate()
            .filter(|(i, t)| *i != 0 && t.state == ThreadState::Terminated && t.stack_ptr != 0)
            .map(|(i, _)| i)
            .collect();
        
        for idx in to_clean.into_iter().rev() {
            let thread = &self.threads[idx];
            if thread.stack_ptr != 0 && thread.stack_size > 0 {
                unsafe {
                    let layout = Layout::from_size_align(thread.stack_size, 16).unwrap();
                    dealloc(thread.stack_ptr as *mut u8, layout);
                }
            }
            self.threads.remove(idx);
            cleaned += 1;
            
            // Adjust current_idx if needed
            if self.current_idx >= idx && self.current_idx > 0 {
                self.current_idx -= 1;
            }
        }
        cleaned
    }
    
    /// Get direct pointers to thread contexts (unsafe - caller must ensure no aliasing)
    pub unsafe fn get_context_ptrs(&mut self, old_idx: usize, new_idx: usize) 
        -> (*mut Context, *const Context) 
    {
        let old_ptr = &mut self.threads[old_idx].context as *mut Context;
        let new_ptr = &self.threads[new_idx].context as *const Context;
        (old_ptr, new_ptr)
    }
}

static SCHEDULER: Spinlock<Option<Scheduler>> = Spinlock::new(None);

// Flag to track if current scheduling call is voluntary (yield) vs preemptive (timer)
static mut VOLUNTARY_SCHEDULE: bool = false;

pub fn init() {
    // Try to lock - use blocking lock since we're single threaded at init
    let mut sched = SCHEDULER.lock();

    // Create scheduler (includes idle thread) - no Box needed!
    *sched = Some(Scheduler::new());
}

/// Spawn a new preemptible thread (can be interrupted by timer)
pub fn spawn(entry: extern "C" fn() -> !) -> Result<usize, &'static str> {
    spawn_with_options(entry, false)
}

/// Spawn a cooperative thread (only yields voluntarily, not preempted by timer)
pub fn spawn_cooperative(entry: extern "C" fn() -> !) -> Result<usize, &'static str> {
    spawn_with_options(entry, true)
}

/// Spawn a thread with options
/// cooperative: if true, thread only yields voluntarily
pub fn spawn_with_options(entry: extern "C" fn() -> !, cooperative: bool) -> Result<usize, &'static str> {
    let mut sched = SCHEDULER.lock();
    if let Some(ref mut scheduler) = *sched {
        scheduler.spawn(entry, cooperative)
    } else {
        Err("Scheduler not initialized")
    }
}

/// SGI handler for scheduling - called when scheduler SGI fires
/// This is the only place where context switching happens
pub fn sgi_scheduler_handler(irq: u32) {
    // Signal EOI BEFORE context switching
    crate::gic::end_of_interrupt(irq);
    
    // Check if this is a voluntary yield or timer preemption
    let voluntary = unsafe { VOLUNTARY_SCHEDULE };
    unsafe { VOLUNTARY_SCHEDULE = false; } // Reset for next time
    
    // Get indices and raw scheduler pointer, then release lock
    let (switch_info, sched_ptr) = {
        let mut sched = SCHEDULER.lock();
        
        let info = if let Some(scheduler) = sched.as_mut() {
            let ptr = scheduler as *mut Scheduler;
            (scheduler.schedule_indices(voluntary), Some(ptr))
        } else {
            (None, None)
        };
        
        info
        // Lock released here
    };
    
    // Now context switch WITHOUT holding lock, using raw pointer
    if let (Some((old_idx, new_idx)), Some(sched_ptr)) = (switch_info, sched_ptr) {
        unsafe { 
            let scheduler = &mut *sched_ptr;
            let (old_ptr, new_ptr) = scheduler.get_context_ptrs(old_idx, new_idx);
            switch_context(old_ptr, new_ptr);
        }
    }
}

// (switch_context declared in extern block above)

/// Voluntarily yield the CPU to another thread
pub fn yield_now() {
    unsafe { VOLUNTARY_SCHEDULE = true; }
    crate::gic::trigger_sgi(crate::gic::SGI_SCHEDULER);
}

/// Get thread statistics: (ready, running, terminated)
pub fn thread_stats() -> (usize, usize, usize) {
    let sched = SCHEDULER.lock();
    if let Some(ref scheduler) = *sched {
        scheduler.thread_stats()
    } else {
        (0, 0, 0)
    }
}

/// Clean up terminated threads and free their stacks
/// Returns number of threads cleaned
pub fn cleanup_terminated() -> usize {
    let mut sched = SCHEDULER.lock();
    if let Some(ref mut scheduler) = *sched {
        scheduler.cleanup_terminated()
    } else {
        0
    }
}

/// Get total thread count
pub fn thread_count() -> usize {
    let sched = SCHEDULER.lock();
    if let Some(ref scheduler) = *sched {
        scheduler.threads.len()
    } else {
        0
    }
}

/// Mark the current thread as terminated
pub fn mark_current_terminated() {
    let mut sched = SCHEDULER.lock();
    if let Some(ref mut scheduler) = *sched {
        scheduler.current_thread_mut().state = ThreadState::Terminated;
    }
}

/// Check if current thread is cooperative
pub fn is_current_cooperative() -> bool {
    let sched = SCHEDULER.lock();
    if let Some(ref scheduler) = *sched {
        scheduler.current_thread().cooperative
    } else {
        false
    }
}

/// Get current thread ID
pub fn current_thread_id() -> usize {
    let sched = SCHEDULER.lock();
    if let Some(ref scheduler) = *sched {
        scheduler.current_idx
    } else {
        0
    }
}

/// Thread entry wrapper - called when thread function returns
extern "C" fn thread_exit() -> ! {
    // Mark thread as terminated
    {
        let mut sched = SCHEDULER.lock();
        if let Some(scheduler) = sched.as_mut() {
            let current = scheduler.current_thread_mut();
            current.state = ThreadState::Terminated;
        }
    }

    // Yield forever
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}
