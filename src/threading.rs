// Preemptive threading with fixed-size thread pool
// Supports per-thread stack sizes and stack overflow detection

#![allow(dead_code)]

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, Ordering};
use spinning_top::Spinlock;

// Use the shared IRQ guard from the irq module
use crate::config;
use crate::irq::with_irqs_disabled;

/// Default timeout for cooperative threads in microseconds (5 seconds)
pub const COOPERATIVE_TIMEOUT_US: u64 = 5_000_000;

/// Thread 0 is the boot/idle thread - always protected, never terminated
const IDLE_THREAD_IDX: usize = 0;

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
    
    // Save TTBR0_EL1 (user address space)
    // This is critical for thread safety: each thread may have different TTBR0
    // (kernel threads use boot TTBR0, user processes use their own)
    mrs x9, ttbr0_el1
    str x9, [x0, #128]
    
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
    
    // Load DAIF (but we'll override IRQ mask below)
    ldr x9, [x1, #104]
    msr daif, x9
    
    // Always enable IRQs after context switch
    // This is necessary because context switches happen inside exception handlers
    // (SGI handler), where DAIF.I is automatically set by the CPU.
    // Without this, threads would inherit the masked IRQ state.
    msr daifclr, #2
    isb  // Ensure DAIF update takes effect
    
    // Load ELR_EL1
    ldr x9, [x1, #112]
    msr elr_el1, x9
    
    // Load SPSR_EL1
    ldr x9, [x1, #120]
    msr spsr_el1, x9
    
    // Load TTBR0_EL1 (user address space)
    // Must restore before returning so the thread sees the correct address space
    ldr x9, [x1, #128]
    msr ttbr0_el1, x9
    isb  // Ensure TTBR0 change takes effect before any memory access
    
    // Return
    ret

// Thread entry trampoline for extern "C" functions
// x19 holds the actual thread entry function
thread_start:
    // Enable IRQs for this thread
    msr daifclr, #2
    
    // Call the thread entry function (in x19)
    blr x19
    
    // Thread returned - mark as terminated and yield
    // (This shouldn't happen for -> ! functions, but just in case)
    b thread_exit_asm

// Thread entry trampoline for Rust closures
// x19 holds pointer to the closure trampoline function
// x20 holds the raw pointer to the boxed closure data
thread_start_closure:
    // Enable IRQs for this thread
    msr daifclr, #2
    
    // Call the trampoline with closure pointer as argument
    // x19 = trampoline function pointer
    // x20 = closure data pointer (passed as x0)
    mov x0, x20
    blr x19
    
    // Thread returned - should not happen for -> ! closures
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
    fn thread_start_closure() -> !;
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
    pub ttbr0: u64, // User address space (TTBR0_EL1)
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
            ttbr0: 0, // Will be initialized to boot TTBR0
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

/// Stack information for a thread
#[derive(Debug, Clone, Copy)]
pub struct StackInfo {
    /// Stack base address (lowest address)
    pub base: usize,
    /// Stack size in bytes
    pub size: usize,
    /// Stack top address (highest address, where SP starts)
    pub top: usize,
}

impl StackInfo {
    /// Create empty/unallocated stack info
    pub const fn empty() -> Self {
        Self {
            base: 0,
            size: 0,
            top: 0,
        }
    }

    /// Create new stack info
    pub fn new(base: usize, size: usize) -> Self {
        Self {
            base,
            size,
            top: base + size,
        }
    }

    /// Check if this stack overlaps with another
    pub fn overlaps(&self, other: &StackInfo) -> bool {
        if self.base == 0 || other.base == 0 {
            return false; // Unallocated stacks don't overlap
        }
        self.base < other.top && other.base < self.top
    }

    /// Check if an address is within this stack
    pub fn contains(&self, addr: usize) -> bool {
        self.base != 0 && addr >= self.base && addr < self.top
    }

    /// Check if this stack is allocated
    pub fn is_allocated(&self) -> bool {
        self.base != 0
    }
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

/// Fixed-size thread pool with per-thread stack sizes
pub struct ThreadPool {
    slots: [ThreadSlot; config::MAX_THREADS],
    stacks: [StackInfo; config::MAX_THREADS],
    current_idx: usize,
    initialized: bool,
}

impl ThreadPool {
    pub const fn new() -> Self {
        Self {
            slots: [const { ThreadSlot::empty() }; config::MAX_THREADS],
            stacks: [const { StackInfo::empty() }; config::MAX_THREADS],
            current_idx: 0,
            initialized: false,
        }
    }

    /// Initialize the pool - allocate stacks with sizes based on thread role
    ///
    /// Thread 0: Boot stack (1MB, fixed location) - cooperative for I/O protection
    /// Threads 1 to RESERVED_THREADS-1: System threads (256KB each) - preemptible
    /// Threads RESERVED_THREADS to MAX_THREADS-1: User process threads (64KB each) - preemptible
    pub fn init(&mut self) {
        // Get the current (boot) TTBR0 value - all kernel threads will use this
        let boot_ttbr0: u64;
        unsafe {
            core::arch::asm!("mrs {}, ttbr0_el1", out(reg) boot_ttbr0);
        }

        // Slot 0 is the idle/boot thread (uses boot stack, never terminated)
        // It runs the async executor and network runner, so mark it cooperative
        // to avoid preemption during critical I/O operations. It still gets
        // preempted after the timeout to allow other threads to run.
        self.slots[IDLE_THREAD_IDX].state = ThreadState::Running;
        self.slots[IDLE_THREAD_IDX].cooperative = true;
        self.slots[IDLE_THREAD_IDX].timeout_us = COOPERATIVE_TIMEOUT_US;
        self.slots[IDLE_THREAD_IDX].start_time_us = crate::timer::uptime_us();
        self.slots[IDLE_THREAD_IDX].context.ttbr0 = boot_ttbr0;

        // Boot stack info (fixed location from boot.rs)
        self.stacks[IDLE_THREAD_IDX] = StackInfo::new(
            0x41F00000, // STACK_TOP - STACK_SIZE = 0x42000000 - 0x100000
            config::KERNEL_STACK_SIZE,
        );

        // Threads 1 to RESERVED_THREADS-1: System threads with large stacks (256KB)
        // Used for shell, SSH sessions, async executor, etc.
        for i in 1..config::RESERVED_THREADS {
            self.allocate_stack_for_slot(i, config::SYSTEM_THREAD_STACK_SIZE);
        }

        // Threads RESERVED_THREADS to MAX_THREADS-1: User process threads with smaller stacks (64KB)
        // Used for running user processes
        for i in config::RESERVED_THREADS..config::MAX_THREADS {
            self.allocate_stack_for_slot(i, config::USER_THREAD_STACK_SIZE);
        }

        self.initialized = true;
    }

    /// Allocate a stack for a specific slot
    fn allocate_stack_for_slot(&mut self, slot_idx: usize, size: usize) {
        let stack_vec: Vec<u8> = alloc::vec![0u8; size];
        let stack_box = stack_vec.into_boxed_slice();
        let stack_ptr = Box::into_raw(stack_box) as *mut u8;
        let stack_info = StackInfo::new(stack_ptr as usize, size);

        // Initialize canary at bottom of stack
        if config::ENABLE_STACK_CANARIES {
            init_stack_canary(stack_info.base);
        }

        self.stacks[slot_idx] = stack_info;
    }

    /// Reallocate stack for a slot with new size (only if slot is Free)
    fn reallocate_stack(&mut self, slot_idx: usize, new_size: usize) -> Result<(), &'static str> {
        if slot_idx == 0 {
            return Err("Cannot reallocate boot stack");
        }
        if slot_idx >= config::MAX_THREADS {
            return Err("Invalid slot index");
        }
        if self.slots[slot_idx].state != ThreadState::Free {
            return Err("Can only reallocate stack for free slot");
        }

        let old_stack = &self.stacks[slot_idx];

        // Free old stack if allocated
        if old_stack.is_allocated() {
            unsafe {
                let ptr = old_stack.base as *mut u8;
                let slice = core::slice::from_raw_parts_mut(ptr, old_stack.size);
                let _ = Box::from_raw(slice as *mut [u8]);
            }
        }

        // Allocate new stack
        self.allocate_stack_for_slot(slot_idx, new_size);

        // Check for overlaps with other stacks
        let new_stack = &self.stacks[slot_idx];
        for i in 0..config::MAX_THREADS {
            if i != slot_idx && new_stack.overlaps(&self.stacks[i]) {
                // This shouldn't happen with heap allocation, but check anyway
                return Err("New stack overlaps with existing stack");
            }
        }

        Ok(())
    }

    /// Spawn a new thread with extern "C" entry function and default stack
    pub fn spawn(
        &mut self,
        entry: extern "C" fn() -> !,
        cooperative: bool,
    ) -> Result<usize, &'static str> {
        self.spawn_with_stack_size(entry, config::DEFAULT_THREAD_STACK_SIZE, cooperative)
    }

    /// Spawn a new thread with extern "C" entry function and custom stack size
    pub fn spawn_with_stack_size(
        &mut self,
        entry: extern "C" fn() -> !,
        stack_size: usize,
        cooperative: bool,
    ) -> Result<usize, &'static str> {
        if !self.initialized {
            return Err("Thread pool not initialized");
        }

        // Find first free slot (skip slot 0 = idle)
        for i in 1..config::MAX_THREADS {
            if self.slots[i].state == ThreadState::Free {
                // Reallocate stack if size differs
                if self.stacks[i].size != stack_size {
                    self.reallocate_stack(i, stack_size)?;
                }

                // Setup the thread
                let stack = &self.stacks[i];
                let sp = (stack.top & !0xF) as u64;
                let entry_addr = entry as *const () as u64;

                // Write context fields individually
                // Get current (boot) TTBR0 for kernel threads
                let boot_ttbr0: u64;
                unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) boot_ttbr0); }

                self.slots[i].context.x19 = entry_addr;
                self.slots[i].context.x20 = 0;
                self.slots[i].context.x21 = 0;
                self.slots[i].context.x22 = 0;
                self.slots[i].context.x23 = 0;
                self.slots[i].context.x24 = 0;
                self.slots[i].context.x25 = 0;
                self.slots[i].context.x26 = 0;
                self.slots[i].context.x27 = 0;
                self.slots[i].context.x28 = 0;
                self.slots[i].context.x29 = 0;
                self.slots[i].context.x30 = thread_start as *const () as u64;
                self.slots[i].context.sp = sp;
                self.slots[i].context.daif = 0;
                self.slots[i].context.elr = 0;
                self.slots[i].context.spsr = 0;
                self.slots[i].context.ttbr0 = boot_ttbr0;

                // Write slot metadata
                self.slots[i].cooperative = cooperative;
                self.slots[i].start_time_us = 0;
                self.slots[i].timeout_us = if cooperative {
                    COOPERATIVE_TIMEOUT_US
                } else {
                    0
                };

                // Set state last (makes thread visible to scheduler)
                self.slots[i].state = ThreadState::Ready;

                return Ok(i);
            }
        }

        Err("No free thread slots")
    }

    /// Spawn a new thread with a boxed closure and default stack
    pub fn spawn_closure(
        &mut self,
        trampoline_fn: fn(*mut ()) -> !,
        closure_ptr: *mut (),
        cooperative: bool,
    ) -> Result<usize, &'static str> {
        self.spawn_closure_with_stack_size(
            trampoline_fn,
            closure_ptr,
            config::DEFAULT_THREAD_STACK_SIZE,
            cooperative,
        )
    }

    /// Spawn a new thread with a boxed closure and custom stack size
    pub fn spawn_closure_with_stack_size(
        &mut self,
        trampoline_fn: fn(*mut ()) -> !,
        closure_ptr: *mut (),
        stack_size: usize,
        cooperative: bool,
    ) -> Result<usize, &'static str> {
        if !self.initialized {
            return Err("Thread pool not initialized");
        }

        // Find first free slot (skip slot 0 = idle)
        for i in 1..config::MAX_THREADS {
            if self.slots[i].state == ThreadState::Free {
                // Reallocate stack if size differs
                if self.stacks[i].size != stack_size {
                    self.reallocate_stack(i, stack_size)?;
                }

                let stack = &self.stacks[i];
                let sp = (stack.top & !0xF) as u64;

                // Get current (boot) TTBR0 for kernel threads
                let boot_ttbr0: u64;
                unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) boot_ttbr0); }

                // x19 = trampoline function pointer
                // x20 = closure data pointer
                self.slots[i].context.x19 = trampoline_fn as *const () as u64;
                self.slots[i].context.x20 = closure_ptr as u64;
                self.slots[i].context.x21 = 0;
                self.slots[i].context.x22 = 0;
                self.slots[i].context.x23 = 0;
                self.slots[i].context.x24 = 0;
                self.slots[i].context.x25 = 0;
                self.slots[i].context.x26 = 0;
                self.slots[i].context.x27 = 0;
                self.slots[i].context.x28 = 0;
                self.slots[i].context.x29 = 0;
                self.slots[i].context.x30 = thread_start_closure as *const () as u64;
                self.slots[i].context.sp = sp;
                self.slots[i].context.daif = 0;
                self.slots[i].context.elr = 0;
                self.slots[i].context.spsr = 0;
                self.slots[i].context.ttbr0 = boot_ttbr0;

                self.slots[i].cooperative = cooperative;
                self.slots[i].start_time_us = 0;
                self.slots[i].timeout_us = if cooperative {
                    COOPERATIVE_TIMEOUT_US
                } else {
                    0
                };

                self.slots[i].state = ThreadState::Ready;

                return Ok(i);
            }
        }

        Err("No free thread slots")
    }

    /// Spawn a thread for system services (SSH sessions, etc.)
    ///
    /// Only searches slots 1..RESERVED_THREADS.
    /// Uses SYSTEM_THREAD_STACK_SIZE (256KB).
    /// System threads are preemptible (not cooperative).
    pub fn spawn_system_closure(
        &mut self,
        trampoline_fn: fn(*mut ()) -> !,
        closure_ptr: *mut (),
    ) -> Result<usize, &'static str> {
        if !self.initialized {
            return Err("Thread pool not initialized");
        }

        // Only search in system thread range (skip thread 0 = boot/async)
        for i in 1..config::RESERVED_THREADS {
            if self.slots[i].state == ThreadState::Free {
                let stack = &self.stacks[i];
                let sp = (stack.top & !0xF) as u64;

                // Get current (boot) TTBR0 for kernel threads
                let boot_ttbr0: u64;
                unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) boot_ttbr0); }

                // x19 = trampoline function pointer
                // x20 = closure data pointer
                self.slots[i].context.x19 = trampoline_fn as *const () as u64;
                self.slots[i].context.x20 = closure_ptr as u64;
                self.slots[i].context.x21 = 0;
                self.slots[i].context.x22 = 0;
                self.slots[i].context.x23 = 0;
                self.slots[i].context.x24 = 0;
                self.slots[i].context.x25 = 0;
                self.slots[i].context.x26 = 0;
                self.slots[i].context.x27 = 0;
                self.slots[i].context.x28 = 0;
                self.slots[i].context.x29 = 0;
                self.slots[i].context.x30 = thread_start_closure as *const () as u64;
                self.slots[i].context.sp = sp;
                self.slots[i].context.daif = 0;
                self.slots[i].context.elr = 0;
                self.slots[i].context.spsr = 0;
                self.slots[i].context.ttbr0 = boot_ttbr0;

                // System threads are preemptible (not cooperative)
                self.slots[i].cooperative = false;
                self.slots[i].start_time_us = 0;
                self.slots[i].timeout_us = 0;

                self.slots[i].state = ThreadState::Ready;

                return Ok(i);
            }
        }

        Err("No free system thread slots")
    }

    /// Spawn a thread for user processes (only in user thread range)
    ///
    /// Only searches slots RESERVED_THREADS..MAX_THREADS.
    /// Uses USER_THREAD_STACK_SIZE (64KB).
    pub fn spawn_user_closure(
        &mut self,
        trampoline_fn: fn(*mut ()) -> !,
        closure_ptr: *mut (),
    ) -> Result<usize, &'static str> {
        if !self.initialized {
            return Err("Thread pool not initialized");
        }

        // Only search in user thread range
        for i in config::RESERVED_THREADS..config::MAX_THREADS {
            if self.slots[i].state == ThreadState::Free {
                let stack = &self.stacks[i];
                let sp = (stack.top & !0xF) as u64;

                // Get current (boot) TTBR0 for kernel threads
                // Note: User process threads will update TTBR0 when entering user mode
                let boot_ttbr0: u64;
                unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) boot_ttbr0); }

                // x19 = trampoline function pointer
                // x20 = closure data pointer
                self.slots[i].context.x19 = trampoline_fn as *const () as u64;
                self.slots[i].context.x20 = closure_ptr as u64;
                self.slots[i].context.x21 = 0;
                self.slots[i].context.x22 = 0;
                self.slots[i].context.x23 = 0;
                self.slots[i].context.x24 = 0;
                self.slots[i].context.x25 = 0;
                self.slots[i].context.x26 = 0;
                self.slots[i].context.x27 = 0;
                self.slots[i].context.x28 = 0;
                self.slots[i].context.x29 = 0;
                self.slots[i].context.x30 = thread_start_closure as *const () as u64;
                self.slots[i].context.sp = sp;
                self.slots[i].context.daif = 0;
                self.slots[i].context.elr = 0;
                self.slots[i].context.spsr = 0;
                self.slots[i].context.ttbr0 = boot_ttbr0;

                // User threads are preemptible (not cooperative)
                self.slots[i].cooperative = false;
                self.slots[i].start_time_us = 0;
                self.slots[i].timeout_us = 0;

                self.slots[i].state = ThreadState::Ready;

                return Ok(i);
            }
        }

        Err("No free user thread slots")
    }

    /// Reclaim a terminated thread slot (just mark as Free)
    pub fn reclaim(&mut self, idx: usize) {
        if idx > 0 && idx < config::MAX_THREADS && self.slots[idx].state == ThreadState::Terminated
        {
            self.slots[idx].state = ThreadState::Free;
            // Re-initialize canary for reuse
            if config::ENABLE_STACK_CANARIES && self.stacks[idx].is_allocated() {
                init_stack_canary(self.stacks[idx].base);
            }
        }
    }

    /// Clean up all terminated threads
    pub fn cleanup_terminated(&mut self) -> usize {
        let mut count = 0;
        for i in 1..config::MAX_THREADS {
            if self.slots[i].state == ThreadState::Terminated {
                self.slots[i].state = ThreadState::Free;
                // Re-initialize canary for reuse
                if config::ENABLE_STACK_CANARIES && self.stacks[i].is_allocated() {
                    init_stack_canary(self.stacks[i].base);
                }
                count += 1;
            }
        }
        count
    }

    /// Select next ready thread (round-robin)
    ///
    /// # Preemption rules:
    /// - `voluntary=true`: Thread yielded voluntarily (yield_now) - always switch
    /// - `voluntary=false`: Timer-triggered preemption
    ///   - Cooperative threads (thread 0): Only switch after timeout elapses
    ///   - Non-cooperative threads (sessions, user processes): Always preemptible
    pub fn schedule_indices(&mut self, voluntary: bool) -> Option<(usize, usize)> {
        let current_idx = self.current_idx;
        let current = &self.slots[current_idx];

        // For timer-triggered preemption, check if the current thread is cooperative.
        // Cooperative threads (like thread 0 running the async executor) get time-slice
        // protection to avoid corruption during I/O operations.
        // Non-cooperative threads (session threads 1-7, user threads 8+) are always
        // immediately preemptible for true multitasking.
        if !voluntary && current.cooperative && current.state == ThreadState::Running {
            let timeout = current.timeout_us;
            if timeout > 0 && current.start_time_us > 0 {
                let now = crate::timer::uptime_us();
                let elapsed = now.saturating_sub(current.start_time_us);
                if elapsed < timeout {
                    // Cooperative thread's time-slice not yet expired
                    return None;
                }
                // Timeout expired - allow preemption
            } else {
                // No timeout set - don't preempt cooperative thread
                return None;
            }
        }
        // Non-cooperative threads: fall through to scheduling (always preemptible)

        // Find next ready thread (including thread 0)
        let mut next_idx = (current_idx + 1) % config::MAX_THREADS;
        let start_idx = next_idx;

        loop {
            let state = self.slots[next_idx].state;
            if state == ThreadState::Ready || state == ThreadState::Running {
                break;
            }

            next_idx = (next_idx + 1) % config::MAX_THREADS;

            if next_idx == start_idx {
                return None;
            }
        }

        if next_idx == current_idx {
            return None;
        }

        // Update states
        if self.slots[current_idx].state != ThreadState::Terminated {
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

// ============================================================================
// Stack Canary Functions
// ============================================================================

/// Initialize stack canary at the bottom of a stack
fn init_stack_canary(stack_base: usize) {
    if stack_base == 0 {
        return;
    }
    unsafe {
        let ptr = stack_base as *mut u64;
        for i in 0..config::CANARY_WORDS {
            ptr.add(i).write_volatile(config::STACK_CANARY);
        }
    }
}

/// Check if stack canary is intact
fn check_stack_canary(stack_base: usize) -> bool {
    if stack_base == 0 {
        return true; // Boot stack or unallocated
    }
    unsafe {
        let ptr = stack_base as *const u64;
        for i in 0..config::CANARY_WORDS {
            if ptr.add(i).read_volatile() != config::STACK_CANARY {
                return false; // Corrupted!
            }
        }
    }
    true
}

// ============================================================================
// Global Thread Pool
// ============================================================================

static POOL: Spinlock<ThreadPool> = Spinlock::new(ThreadPool::new());
static VOLUNTARY_SCHEDULE: AtomicBool = AtomicBool::new(false);

/// Initialize the thread pool
pub fn init() {
    let mut pool = POOL.lock();
    pool.init();
}

/// Spawn a new preemptible thread with extern "C" entry and default stack
pub fn spawn(entry: extern "C" fn() -> !) -> Result<usize, &'static str> {
    spawn_with_options(entry, false)
}

/// Spawn a cooperative thread with extern "C" entry and default stack
pub fn spawn_cooperative(entry: extern "C" fn() -> !) -> Result<usize, &'static str> {
    spawn_with_options(entry, true)
}

/// Spawn with options and default stack (extern "C" entry)
pub fn spawn_with_options(
    entry: extern "C" fn() -> !,
    cooperative: bool,
) -> Result<usize, &'static str> {
    with_irqs_disabled(|| {
        let mut pool = POOL.lock();
        pool.spawn(entry, cooperative)
    })
}

/// Spawn with custom stack size (extern "C" entry)
pub fn spawn_with_stack_size(
    entry: extern "C" fn() -> !,
    stack_size: usize,
    cooperative: bool,
) -> Result<usize, &'static str> {
    with_irqs_disabled(|| {
        let mut pool = POOL.lock();
        pool.spawn_with_stack_size(entry, stack_size, cooperative)
    })
}

/// Trampoline function that calls a boxed FnOnce closure
fn closure_trampoline<F: FnOnce() -> ! + Send + 'static>(closure_ptr: *mut ()) -> ! {
    let closure = unsafe { Box::from_raw(closure_ptr as *mut F) };
    closure()
}

/// Spawn a new preemptible thread with a Rust closure and default stack
pub fn spawn_fn<F>(f: F) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    spawn_fn_with_options(f, false)
}

/// Spawn a cooperative thread with a Rust closure and default stack
pub fn spawn_fn_cooperative<F>(f: F) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    spawn_fn_with_options(f, true)
}

/// Spawn a thread with a Rust closure, options, and default stack
pub fn spawn_fn_with_options<F>(f: F, cooperative: bool) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    spawn_fn_with_stack(f, config::DEFAULT_THREAD_STACK_SIZE, cooperative)
}

/// Spawn a thread with a Rust closure and custom stack size
///
/// # Example
/// ```
/// // Spawn with 256KB stack for async networking
/// spawn_fn_with_stack(|| {
///     run_async_main();
/// }, config::ASYNC_THREAD_STACK_SIZE, false)?;
/// ```
pub fn spawn_fn_with_stack<F>(
    f: F,
    stack_size: usize,
    cooperative: bool,
) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    // Box the closure and get a raw pointer
    let boxed: Box<F> = Box::new(f);
    let closure_ptr = Box::into_raw(boxed) as *mut ();

    // Get the trampoline function for this specific closure type
    let trampoline: fn(*mut ()) -> ! = closure_trampoline::<F>;

    let result = with_irqs_disabled(|| {
        let mut pool = POOL.lock();
        pool.spawn_closure_with_stack_size(trampoline, closure_ptr, stack_size, cooperative)
    });

    // If spawn failed, clean up the boxed closure
    if result.is_err() {
        unsafe {
            let _ = Box::from_raw(closure_ptr as *mut F);
        }
    }

    result
}

/// SGI handler for scheduling
pub fn sgi_scheduler_handler(irq: u32) {
    crate::gic::end_of_interrupt(irq);

    let voluntary = VOLUNTARY_SCHEDULE.swap(false, Ordering::Acquire);

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
    VOLUNTARY_SCHEDULE.store(true, Ordering::Release);
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
    config::MAX_THREADS
}

// ============================================================================
// System Thread API (for SSH sessions, etc.)
// ============================================================================

/// Spawn a thread specifically for system services (SSH sessions, etc.)
///
/// Only spawns in slots 1..RESERVED_THREADS (system thread range).
/// These threads get larger stacks (256KB) and are preemptible.
/// Returns the thread ID or error if no system thread slots are available.
pub fn spawn_system_thread_fn<F>(f: F) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    // Box the closure and get a raw pointer
    let boxed: Box<F> = Box::new(f);
    let closure_ptr = Box::into_raw(boxed) as *mut ();

    // Get the trampoline function for this specific closure type
    let trampoline: fn(*mut ()) -> ! = closure_trampoline::<F>;

    let result = with_irqs_disabled(|| {
        let mut pool = POOL.lock();
        pool.spawn_system_closure(trampoline, closure_ptr)
    });

    // If spawn failed, clean up the boxed closure
    if result.is_err() {
        unsafe {
            let _ = Box::from_raw(closure_ptr as *mut F);
        }
    }

    result
}

/// Count available system thread slots
///
/// Returns the number of free slots in the system thread range (1..RESERVED_THREADS).
pub fn system_threads_available() -> usize {
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        let mut count = 0;
        for i in 1..config::RESERVED_THREADS {
            if pool.slots[i].state == ThreadState::Free {
                count += 1;
            }
        }
        count
    })
}

/// Count active system threads
///
/// Returns the number of non-free slots in the system thread range (1..RESERVED_THREADS).
pub fn system_threads_active() -> usize {
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        let mut count = 0;
        for i in 1..config::RESERVED_THREADS {
            if pool.slots[i].state != ThreadState::Free {
                count += 1;
            }
        }
        count
    })
}

// ============================================================================
// User Process Thread API
// ============================================================================

/// Spawn a thread specifically for user processes
///
/// Only spawns in slots RESERVED_THREADS..MAX_THREADS (user thread range).
/// Returns the thread ID or error if no user thread slots are available.
pub fn spawn_user_thread_fn<F>(f: F) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    // Box the closure and get a raw pointer
    let boxed: Box<F> = Box::new(f);
    let closure_ptr = Box::into_raw(boxed) as *mut ();

    // Get the trampoline function for this specific closure type
    let trampoline: fn(*mut ()) -> ! = closure_trampoline::<F>;

    let result = with_irqs_disabled(|| {
        let mut pool = POOL.lock();
        pool.spawn_user_closure(trampoline, closure_ptr)
    });

    // If spawn failed, clean up the boxed closure
    if result.is_err() {
        unsafe {
            let _ = Box::from_raw(closure_ptr as *mut F);
        }
    }

    result
}

/// Count available user thread slots
///
/// Returns the number of free slots in the user thread range (RESERVED_THREADS..MAX_THREADS).
pub fn user_threads_available() -> usize {
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        let mut count = 0;
        for i in config::RESERVED_THREADS..config::MAX_THREADS {
            if pool.slots[i].state == ThreadState::Free {
                count += 1;
            }
        }
        count
    })
}

/// Count active user threads
///
/// Returns the number of non-free slots in the user thread range.
pub fn user_threads_active() -> usize {
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        let mut count = 0;
        for i in config::RESERVED_THREADS..config::MAX_THREADS {
            if pool.slots[i].state != ThreadState::Free {
                count += 1;
            }
        }
        count
    })
}

/// Check if a thread has terminated
///
/// Returns true if the thread has finished execution (state is Terminated or Free).
/// Also returns true for invalid thread IDs.
pub fn is_thread_terminated(thread_id: usize) -> bool {
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        pool.slots
            .get(thread_id)
            .map(|s| s.state == ThreadState::Terminated || s.state == ThreadState::Free)
            .unwrap_or(true)
    })
}

/// Get the state of a specific thread (for debugging)
pub fn get_thread_state(thread_id: usize) -> Option<ThreadState> {
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        pool.slots.get(thread_id).map(|s| s.state)
    })
}

// ============================================================================
// Stack Protection Functions
// ============================================================================

/// Check all thread stacks for overlap (debug/diagnostic)
/// Returns list of (thread_a, thread_b) pairs that overlap
pub fn check_stack_overlaps() -> Vec<(usize, usize)> {
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        let mut overlaps = Vec::new();

        for i in 0..config::MAX_THREADS {
            for j in (i + 1)..config::MAX_THREADS {
                if pool.stacks[i].overlaps(&pool.stacks[j]) {
                    overlaps.push((i, j));
                }
            }
        }
        overlaps
    })
}

/// Get stack bounds for a thread
pub fn get_stack_bounds(thread_id: usize) -> Option<(usize, usize)> {
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        if thread_id < config::MAX_THREADS && pool.stacks[thread_id].is_allocated() {
            Some((pool.stacks[thread_id].base, pool.stacks[thread_id].top))
        } else {
            None
        }
    })
}

/// Validate current stack pointer is within bounds
pub fn validate_current_sp() -> bool {
    let sp: usize;
    unsafe {
        core::arch::asm!("mov {}, sp", out(reg) sp);
    }

    with_irqs_disabled(|| {
        let pool = POOL.lock();
        let current = pool.current_idx;
        pool.stacks[current].contains(sp)
    })
}

/// Check all thread stack canaries for corruption
/// Returns list of thread IDs with corrupted canaries
pub fn check_all_stack_canaries() -> Vec<usize> {
    if !config::ENABLE_STACK_CANARIES {
        return Vec::new();
    }

    with_irqs_disabled(|| {
        let pool = POOL.lock();
        let mut bad = Vec::new();

        // Skip boot thread (index 0) - it uses fixed boot stack
        for i in 1..config::MAX_THREADS {
            let stack = &pool.stacks[i];
            if stack.is_allocated() && !check_stack_canary(stack.base) {
                bad.push(i);
            }
        }
        bad
    })
}

/// Check if there are any threads ready to run
pub fn has_ready_threads() -> bool {
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        pool.slots.iter().any(|s| s.state == ThreadState::Ready)
    })
}

// ============================================================================
// Kernel Thread Info for kthreads command
// ============================================================================

/// Information about a kernel thread for display
#[derive(Debug, Clone)]
pub struct KernelThreadInfo {
    /// Thread ID (0-31)
    pub tid: usize,
    /// Thread state as string
    pub state: &'static str,
    /// Whether thread is cooperative
    pub cooperative: bool,
    /// Stack base address
    pub stack_base: usize,
    /// Stack size in bytes
    pub stack_size: usize,
    /// Estimated stack usage (based on SP)
    pub stack_used: usize,
    /// Stack canary status (true = intact, false = corrupted)
    pub canary_ok: bool,
    /// Thread description/name
    pub name: &'static str,
}

/// Get list of all kernel threads with their info
pub fn list_kernel_threads() -> Vec<KernelThreadInfo> {
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        let mut threads = Vec::new();

        for i in 0..config::MAX_THREADS {
            let slot = &pool.slots[i];
            let stack = &pool.stacks[i];

            // Skip free slots
            if slot.state == ThreadState::Free {
                continue;
            }

            let state_str = match slot.state {
                ThreadState::Free => "free",
                ThreadState::Ready => "ready",
                ThreadState::Running => "running",
                ThreadState::Terminated => "zombie",
            };

            // Estimate stack usage from saved SP in context
            let stack_used = if stack.is_allocated() && slot.context.sp != 0 {
                // SP points to current stack position (grows down)
                // Usage = top - SP
                let sp = slot.context.sp as usize;
                if sp >= stack.base && sp <= stack.top {
                    stack.top.saturating_sub(sp)
                } else {
                    0 // SP outside stack bounds
                }
            } else {
                0
            };

            // Check canary status
            let canary_ok = if i == 0 || !stack.is_allocated() {
                true // Boot stack or unallocated
            } else if config::ENABLE_STACK_CANARIES {
                check_stack_canary(stack.base)
            } else {
                true // Canaries disabled
            };

            // Thread name based on index range and state
            let name = match i {
                0 => "boot/async",
                1..=7 => "ssh-session", // System threads for SSH sessions
                _ if slot.cooperative => "cooperative",
                _ => "user-process", // User process threads (8+)
            };

            threads.push(KernelThreadInfo {
                tid: i,
                state: state_str,
                cooperative: slot.cooperative,
                stack_base: stack.base,
                stack_size: stack.size,
                stack_used,
                canary_ok,
                name,
            });
        }

        threads
    })
}
