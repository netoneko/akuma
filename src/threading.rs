// Preemptive threading implementation
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::arch::global_asm;
use spinning_top::Spinlock;

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
    
    // Load DAIF (restore interrupt mask)
    ldr x9, [x1, #104]
    msr daif, x9
    
    // Return (will jump to x30 - the new thread's entry point or return address)
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
    pub x29: u64, // Frame pointer
    pub x30: u64, // Link register (return address)
    pub sp: u64,  // Stack pointer
    pub daif: u64, // Interrupt mask (IMPORTANT!)
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
            daif: 0, // IRQs enabled by default
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
}

impl Thread {
    /// Create a new thread with the given entry point
    pub fn new(id: usize, entry: extern "C" fn() -> !) -> Self {
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

    pub fn spawn(&mut self, entry: extern "C" fn() -> !) -> Result<usize, &'static str> {
        if self.threads.len() >= MAX_THREADS {
            return Err("Too many threads");
        }

        let tid = self.threads.len();
        let thread = Thread::new(tid, entry);
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
    /// Returns (old_idx, new_idx)
    pub fn schedule_indices(&mut self) -> Option<(usize, usize)> {
        // Skip if only one thread
        if self.threads.len() <= 1 {
            return None;
        }

        let current_idx = self.current_idx;
        let thread_count = self.threads.len();

        // Find next ready thread (skip idle thread 0)
        // Start from thread 1 if we'd wrap to 0
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

        self.current_idx = next_idx;

        Some((current_idx, next_idx))
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

pub fn init() {
    // Try to lock - use blocking lock since we're single threaded at init
    let mut sched = SCHEDULER.lock();

    // Create scheduler (includes idle thread) - no Box needed!
    *sched = Some(Scheduler::new());
}

pub fn spawn(entry: extern "C" fn() -> !) -> Result<usize, &'static str> {
    let mut sched = SCHEDULER.lock();
    if let Some(ref mut scheduler) = *sched {
        scheduler.spawn(entry)
    } else {
        Err("Scheduler not initialized")
    }
}

/// SGI handler for scheduling - called when scheduler SGI fires
/// This is the only place where context switching happens
pub fn sgi_scheduler_handler(irq: u32) {
    static mut SGI_COUNT: usize = 0;
    unsafe {
        SGI_COUNT += 1;
        if SGI_COUNT <= 5 || SGI_COUNT % 100 == 1 {
            crate::console::print("S");
        }
    }
    
    // Signal EOI BEFORE context switching
    // This allows new interrupts to be accepted even if we don't return
    crate::gic::end_of_interrupt(irq);
    
    // Get indices and raw scheduler pointer, then release lock
    let (switch_info, sched_ptr) = {
        let mut sched = SCHEDULER.lock();
        
        let info = if let Some(scheduler) = sched.as_mut() {
            let ptr = scheduler as *mut Scheduler;
            (scheduler.schedule_indices(), Some(ptr))
        } else {
            (None, None)
        };
        
        info
        // Lock released here
    };
    
    // Now context switch WITHOUT holding lock, using raw pointer
    if let (Some((old_idx, new_idx)), Some(sched_ptr)) = (switch_info, sched_ptr) {
        unsafe { 
            if SGI_COUNT <= 5 {
                crate::console::print(&alloc::format!("[{}->{}]X", old_idx, new_idx));
            }
            
            // SAFETY: Pointer is valid as long as scheduler exists (static lifetime)
            // and threads vector doesn't reallocate (we reserve MAX_THREADS at init)
            let scheduler = &mut *sched_ptr;
            let (old_ptr, new_ptr) = scheduler.get_context_ptrs(old_idx, new_idx);
            
            // Context switch - saves current state directly to old thread's context
            switch_context(old_ptr, new_ptr);
        }
    }
}

// (switch_context declared in extern block above)

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
