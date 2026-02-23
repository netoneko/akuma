// Preemptive threading with fixed-size thread pool
// Supports per-thread stack sizes and stack overflow detection

#![allow(dead_code)]

use alloc::boxed::Box;
use alloc::vec::Vec;
use alloc::sync::Arc; // Added
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use spinning_top::Spinlock;

// Use the shared IRQ guard from the irq module
use crate::config;
use crate::irq::with_irqs_disabled;

// ============================================================================
// Lock-Free Thread State Management
// ============================================================================

/// Thread state values for lock-free atomic operations
pub mod thread_state {
    pub const FREE: u8 = 0;
    pub const READY: u8 = 1;
    pub const RUNNING: u8 = 2;
    pub const TERMINATED: u8 = 3;
    pub const INITIALIZING: u8 = 4; // Thread claimed but context not yet set up
    pub const WAITING: u8 = 5; // Thread blocked on wait queue (e.g., nanosleep)
}

/// Atomic thread states - lock-free access
/// Each thread's state can be read/modified without holding any lock
static THREAD_STATES: [AtomicU8; config::MAX_THREADS] = {
    const INIT: AtomicU8 = AtomicU8::new(thread_state::FREE);
    [INIT; config::MAX_THREADS]
};

/// Atomic wake times for WAITING threads - scheduler checks these
/// Value is 0 for threads that are not waiting, otherwise it's the wake deadline in microseconds
static WAKE_TIMES: [AtomicU64; config::MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; config::MAX_THREADS]
};

/// Atomic total CPU time in microseconds for each thread
static TOTAL_CPU_TIMES: [AtomicU64; config::MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; config::MAX_THREADS]
};

/// Atomic "sticky wake" flags - set when wake() is called, cleared when thread resumes
static WOKEN_STATES: [AtomicBool; config::MAX_THREADS] = {
    const INIT: AtomicBool = AtomicBool::new(false);
    [INIT; config::MAX_THREADS]
};

/// Current running thread - stored in TPIDRRO_EL0 register
/// Using a CPU register avoids race conditions with global atomics.
/// TPIDRRO_EL0 is accessible from EL1 and provides per-CPU thread tracking.
/// It is read-only from EL0 (user mode), which is fine as userspace shouldn't
/// need to modify its own thread ID directly.

/// Set the current thread ID in TPIDRRO_EL0
#[inline]
fn set_current_thread_register(tid: usize) {
    unsafe {
        core::arch::asm!("msr tpidrro_el0, {}", in(reg) tid as u64);
    }
}

/// Get the current thread ID from TPIDRRO_EL0
/// Halts the system if the register contains an invalid value (>= MAX_THREADS)
/// since this indicates serious corruption that cannot be recovered from.
#[inline]
fn get_current_thread_register() -> usize {
    let val: u64;
    unsafe {
        core::arch::asm!("mrs {}, tpidrro_el0", out(reg) val);
    }
    let tid = val as usize;
    // Bounds check - if corrupted, halt immediately
    if tid >= config::MAX_THREADS {
        // Log corruption and halt - we cannot safely continue
        safe_print!(256, "[FATAL] TPIDRRO_EL0 CORRUPT: tid=0x{:x} >= MAX_THREADS ({})\nSystem halted - cannot determine current thread\n", 
            val, config::MAX_THREADS);
        loop {
            unsafe { core::arch::asm!("wfi"); }
        }
    }
    tid
}

/// Atomically claim a free slot in the given range
/// Returns the slot index if successful, None if no free slots
/// NOTE: Sets state to INITIALIZING, not READY - caller must set to READY after context setup!
fn claim_free_slot(start: usize, end: usize) -> Option<usize> {
    for i in start..end {
        // Try to atomically change FREE -> INITIALIZING
        // We use INITIALIZING (not READY) to prevent scheduler from picking up
        // the thread before its context is fully set up
        if THREAD_STATES[i]
            .compare_exchange(
                thread_state::FREE,
                thread_state::INITIALIZING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            return Some(i);
        }
    }
    None
}

/// Per-thread termination timestamp (for cooldown tracking)
static TERMINATION_TIME: [AtomicU64; config::MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; config::MAX_THREADS]
};

/// Mark a thread as terminated (lock-free)
pub fn mark_thread_terminated(idx: usize) {
    if idx != IDLE_THREAD_IDX && idx < config::MAX_THREADS {
        // Record termination time for cooldown tracking
        TERMINATION_TIME[idx].store(crate::timer::uptime_us(), Ordering::SeqCst);
        THREAD_STATES[idx].store(thread_state::TERMINATED, Ordering::SeqCst);
    }
}

/// Mark a thread as ready (lock-free)
fn mark_thread_ready(idx: usize) {
    if idx < config::MAX_THREADS {
        THREAD_STATES[idx].store(thread_state::READY, Ordering::SeqCst);
    }
}

/// Mark a thread as running (lock-free)
fn mark_thread_running(idx: usize) {
    if idx < config::MAX_THREADS {
        THREAD_STATES[idx].store(thread_state::RUNNING, Ordering::SeqCst);
    }
}

/// Mark a thread as waiting with a wake time (lock-free)
fn mark_thread_waiting(idx: usize, wake_time_us: u64) {
    if idx < config::MAX_THREADS {
        WAKE_TIMES[idx].store(wake_time_us, Ordering::SeqCst);
        THREAD_STATES[idx].store(thread_state::WAITING, Ordering::SeqCst);
    }
}

/// Get thread wake time (lock-free read)
fn get_wake_time(idx: usize) -> u64 {
    if idx < config::MAX_THREADS {
        WAKE_TIMES[idx].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// Get thread state (lock-free read)
pub fn get_thread_state(idx: usize) -> u8 {
    if idx < config::MAX_THREADS {
        THREAD_STATES[idx].load(Ordering::SeqCst)
    } else {
        thread_state::FREE
    }
}

/// Check if a thread is terminated (lock-free)
pub fn is_thread_terminated(thread_id: usize) -> bool {
    get_thread_state(thread_id) == thread_state::TERMINATED
}

/// Get total CPU time for a thread in microseconds
pub fn get_thread_cpu_time(idx: usize) -> u64 {
    if idx < config::MAX_THREADS {
        let mut total = TOTAL_CPU_TIMES[idx].load(Ordering::Relaxed);
        
        // If the thread is currently running, add the time since it started
        if get_thread_state(idx) == thread_state::RUNNING {
            let start_time = with_irqs_disabled(|| {
                let pool = POOL.lock();
                pool.slots[idx].start_time_us
            });
            if start_time > 0 {
                let now = crate::timer::uptime_us();
                total += now.saturating_sub(start_time);
            }
        }
        total
    } else {
        0
    }
}

/// Count free slots in range (lock-free)
fn count_free_slots(start: usize, end: usize) -> usize {
    (start..end)
        .filter(|&i| THREAD_STATES[i].load(Ordering::Relaxed) == thread_state::FREE)
        .count()
}

/// Cleanup terminated threads - atomically mark as free (lock-free)
/// Returns number of threads cleaned up
///
/// When DEFERRED_THREAD_CLEANUP is enabled:
/// - Only cleans up if called from thread 0 (main thread)
/// - Respects THREAD_CLEANUP_COOLDOWN_US before recycling slots
pub fn cleanup_terminated_lockfree() -> usize {
    cleanup_terminated_internal(false)
}

/// Force cleanup of terminated threads - bypasses thread check and cooldown
/// Use for tests or when you know it's safe to recycle immediately
pub fn cleanup_terminated_force() -> usize {
    cleanup_terminated_internal(true)
}

/// Internal cleanup implementation
fn cleanup_terminated_internal(force: bool) -> usize {
    // In deferred mode (unless forced), only allow cleanup from thread 0
    if !force && config::DEFERRED_THREAD_CLEANUP {
        let current = get_current_thread_register();
        if current != IDLE_THREAD_IDX {
            // Not main thread - skip cleanup
            return 0;
        }
    }
    
    let now = crate::timer::uptime_us();
    let mut count = 0;
    
    for i in 1..config::MAX_THREADS {
        // Check if thread is terminated
        if THREAD_STATES[i].load(Ordering::SeqCst) != thread_state::TERMINATED {
            continue;
        }
        
        // In deferred mode (unless forced), check cooldown period
        if !force && config::DEFERRED_THREAD_CLEANUP {
            let term_time = TERMINATION_TIME[i].load(Ordering::SeqCst);
            if term_time > 0 && now.saturating_sub(term_time) < config::THREAD_CLEANUP_COOLDOWN_US {
                // Thread hasn't been terminated long enough - skip
                continue;
            }
        }
        
        // CRITICAL: Use INITIALIZING as intermediate state to prevent race with spawn!
        // 
        // Race condition without this:
        // 1. Cleanup: TERMINATED -> FREE
        // 2. Spawn: claim_free_slot sees FREE, changes to INITIALIZING
        // 3. Spawn: sets up context in THREAD_CONTEXTS[i]
        // 4. Cleanup: still running, zeros THREAD_CONTEXTS[i] -> OVERWRITES spawn's context!
        // 5. Spawn: sets state to READY
        // 6. Scheduler: switches to thread with zeroed context -> CRASH
        //
        // Solution: Use INITIALIZING so spawn's claim_free_slot fails while cleanup runs.
        if THREAD_STATES[i]
            .compare_exchange(
                thread_state::TERMINATED,
                thread_state::INITIALIZING,  // Block spawns from claiming this slot
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            // Calculate cooldown before clearing
            let term_time = TERMINATION_TIME[i].load(Ordering::SeqCst);
            let cooldown = now.saturating_sub(term_time);
            
            // Clear termination time
            TERMINATION_TIME[i].store(0, Ordering::SeqCst);
            
            // CRITICAL: Zero the context to prevent stale ELR/SPSR/TTBR0 from leaking
            // This prevents a newly spawned thread from accidentally getting user-mode
            // ELR/SPSR from a previous process execution.
            //
            // We must disable IRQs while holding the pool lock to prevent deadlock:
            // if a timer fires while we hold the lock, the SGI handler will try to
            // acquire the same lock and spin forever (single CPU = deadlock).
            {
                let _guard = crate::irq::IrqGuard::new();
                
                // Zero the context in THREAD_CONTEXTS
                unsafe {
                    *get_context_mut(i) = Context::zero();
                }
                
                // Clear slot state
                let mut pool = POOL.lock();
                pool.slots[i].cooperative = false;
                pool.slots[i].start_time_us = 0;
                pool.slots[i].timeout_us = 0;
            }
            
            // Re-initialize canary for reuse
            if config::ENABLE_STACK_CANARIES {
                // Must disable IRQs when acquiring POOL lock to prevent deadlock
                // if timer fires - SGI handler would try to acquire the same lock
                let stack_base = {
                    let _guard = crate::irq::IrqGuard::new();
                    POOL.lock().stacks[i].base
                };
                if stack_base != 0 {
                    init_stack_canary(stack_base);
                }
            }
            
            // NOW set to FREE - cleanup is complete, spawn can safely claim this slot
            THREAD_STATES[i].store(thread_state::FREE, Ordering::SeqCst);
            
            // Safe print without heap allocation
            safe_print!(128, "[Cleanup] Thread {} recycled after {}us cooldown\n", i, cooldown);
            
            count += 1;
        }
    }
    count
}

// ============================================================================
// Preemption Control (Per-Thread)
// ============================================================================

/// Per-thread preemption disable counters.
/// Each thread has its own counter to track nested disable_preemption() calls.
/// This prevents one thread's preemption state from affecting another thread.
static PREEMPTION_DISABLED: [AtomicUsize; config::MAX_THREADS] = {
    const INIT: AtomicUsize = AtomicUsize::new(0);
    [INIT; config::MAX_THREADS]
};

/// Per-thread timestamp (in microseconds) when preemption was last disabled.
/// Used by the watchdog to detect stuck threads.
static PREEMPTION_DISABLED_SINCE: [AtomicU64; config::MAX_THREADS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; config::MAX_THREADS]
};

/// Track last time we ran the watchdog check (for detecting time jumps)
static LAST_WATCHDOG_CHECK_US: AtomicU64 = AtomicU64::new(0);

/// Maximum time preemption can be disabled before watchdog warning (100ms)
const PREEMPTION_WATCHDOG_WARN_US: u64 = 100_000;

/// Maximum time preemption can be disabled before watchdog panic (5 seconds)
const PREEMPTION_WATCHDOG_PANIC_US: u64 = 5_000_000;

/// Maximum expected gap between watchdog checks (100ms).
/// If we see a gap larger than this, the host likely slept.
const MAX_EXPECTED_CHECK_GAP_US: u64 = 100_000;

/// Disable preemption for the current thread.
///
/// Can be nested - must call `enable_preemption()` the same number of times.
/// While preemption is disabled, timer interrupts will not cause a context switch
/// for THIS thread, but IRQs are still enabled and yield_now() still works.
///
/// Use this to protect code that uses RefCell or other non-thread-safe structures.
#[inline]
pub fn disable_preemption() {
    let tid = get_current_thread_register();
    let prev = PREEMPTION_DISABLED[tid].fetch_add(1, Ordering::SeqCst);
    // Record timestamp on first disable (nesting level 0 -> 1)
    if prev == 0 {
        PREEMPTION_DISABLED_SINCE[tid].store(crate::timer::uptime_us(), Ordering::Release);
    }
}

/// Re-enable preemption for the current thread.
///
/// Must be called once for each call to `disable_preemption()`.
#[inline]
pub fn enable_preemption() {
    let tid = get_current_thread_register();
    let prev = PREEMPTION_DISABLED[tid].fetch_sub(1, Ordering::SeqCst);
    debug_assert!(prev > 0, "enable_preemption called without matching disable");
    // Clear timestamp when fully re-enabled (nesting level 1 -> 0)
    if prev == 1 {
        PREEMPTION_DISABLED_SINCE[tid].store(0, Ordering::Release);
    }
}

/// Check if preemption is currently disabled for the current thread.
#[inline]
pub fn is_preemption_disabled() -> bool {
    let tid = get_current_thread_register();
    PREEMPTION_DISABLED[tid].load(Ordering::SeqCst) > 0
}

/// Check if preemption has been disabled for too long (watchdog).
/// Called from timer interrupt handler.
/// Returns:
/// - None: preemption not disabled or within normal time
/// - Some(duration_us): preemption disabled for this many microseconds
pub fn check_preemption_watchdog() -> Option<u64> {
    let tid = get_current_thread_register();
    let now = crate::timer::uptime_us();
    
    // Detect time jumps (host sleep/wake)
    let last_check = LAST_WATCHDOG_CHECK_US.swap(now, Ordering::SeqCst);
    if last_check > 0 {
        let gap = now.saturating_sub(last_check);
        if gap > MAX_EXPECTED_CHECK_GAP_US {
            // Time jumped - host probably slept. Log and reset timestamps.
            // Safe print without heap allocation
            safe_print!(128, "[WATCHDOG] Time jump detected: {}ms (host sleep/wake)\n", gap / 1000);
            
            // Reset timestamp for this thread to avoid false alarm
            let disabled_since = PREEMPTION_DISABLED_SINCE[tid].load(Ordering::Acquire);
            if disabled_since != 0 {
                PREEMPTION_DISABLED_SINCE[tid].store(now, Ordering::Release);
            }
            return None;
        }
    }
    
    let disabled_since = PREEMPTION_DISABLED_SINCE[tid].load(Ordering::Acquire);
    if disabled_since == 0 {
        return None;
    }
    
    let duration = now.saturating_sub(disabled_since);
    
        if duration >= PREEMPTION_WATCHDOG_PANIC_US {
            // Critical: been disabled way too long - just log and continue
            // DO NOT use panic! here - we're in IRQ context
            safe_print!(128, "[WATCHDOG] Thread {} preemption disabled {}ms (critical)\n", tid, duration / 1000);
            return Some(duration);
        }
     else if duration >= PREEMPTION_WATCHDOG_WARN_US {
        // Warning: something is slow
        return Some(duration);
    }
    
    None
}

// ============================================================================
// Thread Constants
// ============================================================================

/// Default timeout for cooperative threads in microseconds (100ms)
/// Reduced from 5 seconds to ensure network loop runs frequently
pub const COOPERATIVE_TIMEOUT_US: u64 = 100_000;

/// Thread 0 is the boot/idle thread - always protected, never terminated
const IDLE_THREAD_IDX: usize = 0;

// Assembly context switch implementation
global_asm!(
    r#"
.section .text
.global switch_context
.global thread_start
.global thread_start_closure

// void switch_context(Context* old, const Context* new)
// x0 = pointer to old context (save here)
// x1 = pointer to new context (load from here)
switch_context:
    // Save old context
    // NOTE: magic field is at offset 0, registers start at offset 8
    stp x19, x20, [x0, #8]
    stp x21, x22, [x0, #24]
    stp x23, x24, [x0, #40]
    stp x25, x26, [x0, #56]
    stp x27, x28, [x0, #72]
    stp x29, x30, [x0, #88]
    
    // Save stack pointer
    mov x9, sp
    str x9, [x0, #104]
    
    // Save DAIF (interrupt mask)
    mrs x9, daif
    str x9, [x0, #112]
    
    // Save ELR_EL1 and SPSR_EL1 to context
    // CRITICAL: We MUST save/restore these here because:
    // - irq_handler pushes ELR/SPSR to the OLD thread's stack
    // - switch_context switches to the NEW thread's stack  
    // - irq_handler would pop from the WRONG stack without this!
    // Each thread needs its own saved ELR/SPSR in its context.
    mrs x9, elr_el1
    str x9, [x0, #120]
    mrs x9, spsr_el1
    str x9, [x0, #128]
    
    // Save TTBR0_EL1 (user address space)
    // This is critical for thread safety: each thread may have different TTBR0
    // (kernel threads use boot TTBR0, user processes use their own)
    mrs x9, ttbr0_el1
    str x9, [x0, #136]

    // Save TPIDR_EL0 (user TLS pointer)
    mrs x9, tpidr_el0
    str x9, [x0, #160]
    
    // Ensure all writes to new context memory are visible before loading
    dsb ish
    
    // Load new context
    ldp x19, x20, [x1, #8]
    ldp x21, x22, [x1, #24]
    ldp x23, x24, [x1, #40]
    ldp x25, x26, [x1, #56]
    ldp x27, x28, [x1, #72]
    ldp x29, x30, [x1, #88]
    
    // CRITICAL: Catch corrupt x30 before ret
    // If x30 == 0, we'd jump to address 0 and crash with EC=0x0
    cbnz x30, 10f
    // x30 is 0! Halt the system with marker
    mov x0, #0xBAD
    movk x0, #0x0030, lsl #16   // 0x00300BAD = "bad x30"
11: wfi
    b 11b
10:
    
    // Load stack pointer
    ldr x9, [x1, #104]
    mov sp, x9
    
    // CRITICAL: Do NOT restore DAIF here!
    // 
    // Restoring DAIF could unmask IRQs mid-switch, allowing a nested timer
    // interrupt. Since TPIDRRO_EL0 was already updated, the nested handler
    // would save the wrong thread's context, causing corruption.
    //
    // IRQs must stay MASKED throughout switch_context.
    //
    // For RETURNING threads: sgi_scheduler_handler's epilog will unmask IRQs,
    // then ERET restores SPSR (which includes original IRQ state).
    //
    // For NEW threads: they ret to thread_start_closure which handles IRQ
    // enabling based on thread type (process threads stay masked until
    // activate(), others enable immediately).
    
    // Load ELR_EL1 and SPSR_EL1 from new context
    // This ensures irq_handler's ERET uses this thread's saved values,
    // not garbage from the stack (which belongs to a different thread after switch).
    ldr x9, [x1, #120]
    
    // CRITICAL: Catch corrupt ELR before writing to system register
    // ELR=0 is ALWAYS a bug for any thread - there's no valid code at address 0
    cbnz x9, 12f                // ELR != 0, OK
    // ELR is 0! Halt with thread ID in x1
    mrs x1, tpidrro_el0         // Get thread ID for debugging
    mov x0, #0xBAD
    movk x0, #0x00E1, lsl #16   // 0x00E10BAD = "bad ELR"
13: wfi
    b 13b
12:
    msr elr_el1, x9
    ldr x9, [x1, #128]
    msr spsr_el1, x9
    isb                       // Ensure ELR/SPSR changes visible before continuing
    
    // Load TTBR0_EL1 (user address space)
    // Must restore before returning so the thread sees the correct address space
    //
    // CRITICAL: Must flush TLB after TTBR0 switch!
    // When switching between threads with different TTBR0 (kernel vs user),
    // stale TLB entries from the old address space could cause:
    // - Wrong physical addresses being accessed
    // - External aborts during translation table walk (DFSC=0x21)
    // 
    // Sequence: DSB -> switch TTBR0 -> ISB -> flush TLB -> DSB -> ISB
    ldr x9, [x1, #136]
    dsb ish                   // Complete pending memory accesses
    msr ttbr0_el1, x9         // Switch TTBR0

    // Restore TPIDR_EL0 (user TLS pointer)
    ldr x9, [x1, #160]
    msr tpidr_el0, x9
    isb
    
    isb                       // Ensure TTBR0 change visible
    tlbi vmalle1              // Flush all EL1 TLB entries
    dsb ish                   // Wait for TLB flush to complete
    isb                       // Ensure clean state before execution continues
    
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
// x21 holds IRQ enable flag: 0 = enable IRQs now, non-zero = keep disabled
thread_start_closure:
    // CRITICAL: Verify x19 (trampoline) is valid before calling
    // If x19 == 0, we'd jump to address 0 and crash with EC=0x0
    cbnz x19, 2f
    // x19 is 0! Halt with marker
    mov x0, #0xBAD
    movk x0, #0x0019, lsl #16   // 0x00190BAD = "bad x19"
3:  wfi
    b 3b
2:
    // Check if we should enable IRQs (x21 == 0 means enable)
    // For process threads: x21 != 0, keep IRQs disabled until activate()
    // For system/test threads: x21 == 0, enable IRQs now
    cbnz x21, 1f           // Skip IRQ enable if x21 != 0
    msr daifclr, #2        // Enable IRQs
1:
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

/// Magic value for Context integrity check
pub const CONTEXT_MAGIC: u64 = 0xDEAD_BEEF_1234_5678;

/// Size of the UNIFIED IRQ frame saved on stack (304 bytes)
/// Both EL0 and EL1 IRQ handlers now use this same layout.
pub const IRQ_FRAME_SIZE: usize = 304;

/// Set up a fake IRQ frame on a new thread's stack
/// 
/// This allows the simplified stack-based context switch to work for new threads.
/// When the IRQ handler restores from this stack, it will load these values.
/// 
/// UNIFIED frame layout (288 bytes) - used by both EL0 and EL1 handlers:
///   [sp+0]:   x30 + padding
///   [sp+16]:  x28, x29
///   [sp+32]:  x26, x27
///   [sp+48]:  x24, x25
///   [sp+64]:  x22, x23
///   [sp+80]:  x20, x21
///   [sp+96]:  x18, x19
///   [sp+112]: x16, x17
///   [sp+128]: x14, x15
///   [sp+144]: x12, x13
///   [sp+160]: x8, x9
///   [sp+176]: x6, x7
///   [sp+192]: x4, x5
///   [sp+208]: x2, x3
///   [sp+224]: x0, x1
///   [sp+240]: ELR, SPSR
///   [sp+256]: SP_EL0 + padding
///   [sp+272]: TPIDR_EL0 + padding
///   [sp+288]: x10, x11
/// 
/// Returns the SP value pointing to the fake IRQ frame
pub fn setup_fake_irq_frame(
    stack_top: u64,
    entry_point: u64,
    x19: u64,  // Trampoline function pointer
    x20: u64,  // Closure data pointer
    x21: u64,  // IRQ enable flag (0 = enable)
) -> u64 {
    let frame_base = stack_top - IRQ_FRAME_SIZE as u64;
    let frame = frame_base as *mut u64;
    
    unsafe {
        // Zero the frame first
        core::ptr::write_bytes(frame as *mut u8, 0, IRQ_FRAME_SIZE);
        
        // Write registers at their offsets (offset / 8 = index)
        // [sp+0]: x30 - return address after thread_start_closure returns
        frame.add(0).write_volatile(thread_exit_stub as *const () as u64);
        
        // [sp+16]: x28, x29 - frame pointer and x28
        frame.add(2).write_volatile(0);  // x28
        frame.add(3).write_volatile(0);  // x29 (frame pointer)
        
        // [sp+80]: x20, x21
        frame.add(10).write_volatile(x20);  // x20 - closure data
        frame.add(11).write_volatile(x21);  // x21 - IRQ enable flag
        
        // [sp+96]: x18, x19
        frame.add(12).write_volatile(0);    // x18
        frame.add(13).write_volatile(x19);  // x19 - trampoline
        
        // [sp+240]: ELR, SPSR
        frame.add(30).write_volatile(entry_point);  // ELR - where to jump
        // SPSR bits: [9:7]=DAI masks, [6]=F mask, [3:0]=M (EL1h=5)
        // 0x345 = F=1, I=0 (IRQ enabled), A=1, D=1
        // 0x3C5 = F=1, I=1 (IRQ disabled), A=1, D=1
        let spsr = if x21 != 0 {
            0x000003C5  // EL1h, IRQs DISABLED
        } else {
            0x00000345  // EL1h, IRQs ENABLED
        };
        frame.add(31).write_volatile(spsr);
        
        // [sp+256]: SP_EL0 + padding (user stack pointer, 0 for new threads)
        frame.add(32).write_volatile(0);  // SP_EL0

        // [sp+272]: TPIDR_EL0 + padding (user thread pointer, 0 for new threads)
        frame.add(34).write_volatile(0);  // TPIDR_EL0
        
        // [sp+288]: x10, x11
        frame.add(36).write_volatile(0);  // x10
        frame.add(37).write_volatile(0);  // x11
    }
    
    frame_base
}

/// Stub for thread exit - threads should never return here
#[unsafe(no_mangle)]
extern "C" fn thread_exit_stub() -> ! {
    safe_print!(128, "[THREAD] Exit stub reached - marking terminated\n");
    mark_current_terminated();
    loop {
        yield_now();
    }
}

/// CPU context saved during context switch
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Context {
    // Magic value FIRST for easy detection of corruption
    pub magic: u64,
    // Callee-saved registers
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
    // User process fields (unified context architecture)
    pub user_entry: u64,     // User PC for first ERET (0 for kernel threads)
    pub user_sp: u64,        // User SP for first ERET (0 for kernel threads)
    pub user_tls: u64,       // User TPIDR_EL0 for TLS
    pub is_user_process: u64, // 1 if this is a user process thread, 0 for kernel threads
}

impl Context {
    pub const fn zero() -> Self {
        Self {
            magic: CONTEXT_MAGIC,
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
            user_entry: 0,
            user_sp: 0,
            user_tls: 0,
            is_user_process: 0,
        }
    }
    
    /// Check if the context magic is intact
    pub fn is_valid(&self) -> bool {
        self.magic == CONTEXT_MAGIC
    }
}

// ============================================================================
// Thread Contexts - Separate Static Array for Lock-Free Access
// ============================================================================
//
// Thread contexts are stored in a separate static array, NOT behind the POOL
// spinlock. This allows the scheduler to access contexts without holding the
// lock across switch_context, which would cause deadlock.
//
// Safety invariants:
// 1. Only the scheduler (with IRQs masked) modifies contexts during switch
// 2. A thread's context is only accessed when that thread is NOT running
// 3. Context must be fully initialized before state becomes READY
// 4. Context is zeroed when state becomes FREE
// ============================================================================

use core::cell::UnsafeCell;

/// Wrapper to make UnsafeCell<Context> Sync
/// SAFETY: THREAD_CONTEXTS is safe to share because:
/// 1. Each context is only modified by the scheduler with IRQs masked
/// 2. A context is only accessed when its thread is not running on any CPU
/// 3. We're single-CPU, so no concurrent access is possible
#[repr(transparent)]
struct SyncContext(UnsafeCell<Context>);

// SAFETY: See above - single CPU with IRQs masked during access
unsafe impl Sync for SyncContext {}

impl SyncContext {
    const fn new() -> Self {
        Self(UnsafeCell::new(Context::zero()))
    }
    
    #[inline]
    fn get(&self) -> *mut Context {
        self.0.get()
    }
}

/// Per-thread CPU contexts - accessed without POOL lock
/// Safety: Access only when IRQs are masked and thread state is valid
static THREAD_CONTEXTS: [SyncContext; config::MAX_THREADS] = {
    const INIT: SyncContext = SyncContext::new();
    [INIT; config::MAX_THREADS]
};

/// Get a mutable pointer to a thread's context
/// SAFETY: Caller must ensure IRQs are masked and thread is not running
#[inline]
fn get_context_mut(idx: usize) -> *mut Context {
    THREAD_CONTEXTS[idx].get()
}

/// Get an immutable pointer to a thread's context  
/// SAFETY: Caller must ensure thread is not running
#[inline]
fn get_context(idx: usize) -> *const Context {
    THREAD_CONTEXTS[idx].get()
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

/// Size of per-thread exception stack area (reserved at top of kernel stack)
/// This area holds:
/// - sync_el0_handler trap frame (296 bytes) at top
/// - irq_el0_handler frame (272 bytes) at top-768
/// - Function call stack for syscall handlers (console, FS, etc.)
/// 
/// IMPORTANT: Syscall handlers can call deep functions. 16KB provides enough
/// headroom to prevent overlap with kernel code (like execute()) below.
pub const EXCEPTION_STACK_SIZE: usize = 16384*2;

/// Thread slot in the pool
/// 
/// Note: Thread state is stored in the global THREAD_STATES atomic array,
/// NOT in this struct. This allows lock-free state checks during scheduling.
#[repr(C)]
pub struct ThreadSlot {
    // NOTE: state removed - use THREAD_STATES[idx] instead for lock-free access
    // NOTE: context removed - use THREAD_CONTEXTS[idx] instead for lock-free access
    pub cooperative: bool,
    pub start_time_us: u64,
    pub timeout_us: u64,
    /// Per-thread exception stack top (for syscall trap frames)
    /// This is the top of the reserved 1KB area at the top of the kernel stack.
    /// To move exception stacks elsewhere, allocate separate memory and update this pointer.
    pub exception_stack_top: u64,
}

impl ThreadSlot {
    pub const fn empty() -> Self {
        Self {
            cooperative: false,
            start_time_us: 0,
            timeout_us: 0,
            exception_stack_top: 0,
        }
    }
}

/// Fixed-size thread pool with per-thread stack sizes
pub struct ThreadPool {
    slots: [ThreadSlot; config::MAX_THREADS],
    stacks: [StackInfo; config::MAX_THREADS],
    current_idx: usize,
    initialized: bool,
    /// Counter for proportional scheduling of thread 0
    /// Thread 0 gets boosted when this reaches NETWORK_THREAD_RATIO
    network_boost_counter: u32,
    /// Global round-robin index for fair thread rotation
    /// This ensures all threads get scheduled, not just the first ready one
    /// after the current thread.
    round_robin_idx: usize,
}

impl ThreadPool {
    pub const fn new() -> Self {
        Self {
            slots: [const { ThreadSlot::empty() }; config::MAX_THREADS],
            stacks: [const { StackInfo::empty() }; config::MAX_THREADS],
            current_idx: 0,
            initialized: false,
            network_boost_counter: 0,
            round_robin_idx: 0,
        }
    }

    /// Initialize the pool - allocate stacks with sizes based on thread role
    ///
    /// Thread 0: Boot stack (1MB, fixed location) - cooperative for I/O protection
    /// Threads 1 to RESERVED_THREADS-1: System threads (256KB each) - preemptible
    /// Threads RESERVED_THREADS to MAX_THREADS-1: User process threads (128KB each) - preemptible
    pub fn init(&mut self) {
        // Get the STORED boot TTBR0 value - all kernel threads will use this
        // CRITICAL: Must use stored value, not current TTBR0 which could be a user process's!
        let boot_ttbr0: u64 = crate::mmu::get_boot_ttbr0();

        // Slot 0 is the idle/boot thread (uses boot stack, never terminated)
        // It runs the async executor and network runner, so mark it cooperative
        // to avoid preemption during critical I/O operations. It still gets
        // preempted after the timeout to allow other threads to run.
        THREAD_STATES[IDLE_THREAD_IDX].store(thread_state::RUNNING, Ordering::SeqCst);
        self.slots[IDLE_THREAD_IDX].cooperative = true;
        self.slots[IDLE_THREAD_IDX].timeout_us = COOPERATIVE_TIMEOUT_US;
        self.slots[IDLE_THREAD_IDX].start_time_us = crate::timer::uptime_us();
        
        // Initialize boot thread context in THREAD_CONTEXTS (not in slot)
        unsafe {
            let boot_ctx = &mut *get_context_mut(IDLE_THREAD_IDX);
            boot_ctx.magic = CONTEXT_MAGIC;
            boot_ctx.ttbr0 = boot_ttbr0;
            // Boot thread starts with kernel mode SPSR
            boot_ctx.spsr = 0x00000005; // EL1h
            // Other fields stay zero (callee-saved regs saved on first context switch)
            boot_ctx.user_entry = 0;
            boot_ctx.user_sp = 0;
            boot_ctx.is_user_process = 0;
        }

        // Boot stack info (fixed location from boot.rs)
        // The boot stack was already in use before threading init, starting at
        // 0x42000000 and growing down. We CANNOT reserve space at the top.
        let boot_stack_top = 0x42000000u64; // STACK_TOP from boot.rs
        let boot_stack_base = 0x41F00000usize; // STACK_TOP - STACK_SIZE = 0x42000000 - 0x100000
        self.stacks[IDLE_THREAD_IDX] = StackInfo::new(
            boot_stack_base,
            config::KERNEL_STACK_SIZE,
        );
        
        // Allocate a SEPARATE exception stack for thread 0 (boot thread).
        // Unlike spawned threads which reserve space at the top of their stack,
        // the boot stack was already in use before we could reserve space.
        // We allocate from heap to get a clean, separate area.
        let boot_exception_stack: Vec<u8> = alloc::vec![0u8; EXCEPTION_STACK_SIZE];
        let boot_exception_stack_box = boot_exception_stack.into_boxed_slice();
        let boot_exception_stack_ptr = Box::into_raw(boot_exception_stack_box);
        let boot_exception_stack_top = unsafe { 
            (boot_exception_stack_ptr as *const u8).add(EXCEPTION_STACK_SIZE) as u64 
        };
        // Align to 16 bytes
        self.slots[IDLE_THREAD_IDX].exception_stack_top = boot_exception_stack_top & !0xF;
        
        // CRITICAL: Update TPIDR_EL1 to point to Thread 0's new exception stack!
        // exceptions::init() set it to 0x42000000 (boot stack top) initially,
        // but we've now allocated a proper exception stack from the heap.
        // Without this, the first IRQ would use the wrong exception stack pointer.
        crate::exceptions::set_current_exception_stack(self.slots[IDLE_THREAD_IDX].exception_stack_top);
        
        // Initialize canary for boot stack
        if config::ENABLE_STACK_CANARIES {
            init_stack_canary(boot_stack_base);
        }

        // Threads 1 to RESERVED_THREADS-1: System threads with large stacks (256KB)
        // Used for shell, SSH sessions, async executor, etc.
        for i in 1..config::RESERVED_THREADS {
            self.allocate_stack_for_slot(i, config::SYSTEM_THREAD_STACK_SIZE);
        }

        // Threads RESERVED_THREADS to MAX_THREADS-1: User process threads with smaller stacks (128KB)
        // Used for running user processes
        for i in config::RESERVED_THREADS..config::MAX_THREADS {
            self.allocate_stack_for_slot(i, config::USER_THREAD_STACK_SIZE);
        }

        self.initialized = true;
    }

    /// Allocate a stack for a specific slot
    /// 
    /// Stack layout (stack grows downward):
    /// ```text
    /// |------------------| <- stack_top (highest address)
    /// | Exception area   |  EXCEPTION_STACK_SIZE (1KB) for trap frames
    /// |------------------|
    /// | Kernel stack     |  Rest of stack for normal kernel code
    /// |------------------| <- stack_base (lowest address)
    /// ```
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
        
        // Set exception stack top (top of the reserved 1KB area)
        // The exception stack is at the very top of the kernel stack
        self.slots[slot_idx].exception_stack_top = (stack_info.top & !0xF) as u64;
    }

    /// Reallocate stack for a slot with new size (only if slot is Free)
    fn reallocate_stack(&mut self, slot_idx: usize, new_size: usize) -> Result<(), &'static str> {
        if slot_idx == 0 {
            return Err("Cannot reallocate boot stack");
        }
        if slot_idx >= config::MAX_THREADS {
            return Err("Invalid slot index");
        }
        if THREAD_STATES[slot_idx].load(Ordering::SeqCst) != thread_state::FREE {
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
            if THREAD_STATES[i].load(Ordering::SeqCst) == thread_state::FREE {
                // Reallocate stack if size differs
                if self.stacks[i].size != stack_size {
                    self.reallocate_stack(i, stack_size)?;
                }

                // Setup the thread
                let stack = &self.stacks[i];
                let sp = (stack.top & !0xF) as u64;
                let entry_addr = entry as *const () as u64;

                // Write context fields to THREAD_CONTEXTS (not in slot)
                // Get STORED boot TTBR0 (not current, which could be user process's!)
                let boot_ttbr0 = crate::mmu::get_boot_ttbr0();

                unsafe {
                    let ctx = &mut *get_context_mut(i);
                    // Magic value for integrity checking
                    ctx.magic = CONTEXT_MAGIC;
                    ctx.x19 = entry_addr;
                    ctx.x20 = 0;
                    ctx.x21 = 0;
                    ctx.x22 = 0;
                    ctx.x23 = 0;
                    ctx.x24 = 0;
                    ctx.x25 = 0;
                    ctx.x26 = 0;
                    ctx.x27 = 0;
                    ctx.x28 = 0;
                    ctx.x29 = 0;
                    ctx.x30 = thread_start as *const () as u64;
                    ctx.sp = sp;
                    ctx.daif = 0;
                    // Set ELR to thread entry point - if any code path accidentally ERETs,
                    // we'll jump to a valid address instead of crashing with ELR=0
                    ctx.elr = thread_start as *const () as u64;
                    // SPSR for kernel threads: EL1h (bits[3:0]=5)
                    ctx.spsr = 0x00000005; // EL1h
                    ctx.ttbr0 = boot_ttbr0;
                    // User process fields - 0 for kernel threads
                    ctx.user_entry = 0;
                    ctx.user_sp = 0;
                    ctx.is_user_process = 0;
                }

                // Write slot metadata
                self.slots[i].cooperative = cooperative;
                self.slots[i].start_time_us = 0;
                self.slots[i].timeout_us = if cooperative {
                    COOPERATIVE_TIMEOUT_US
                } else {
                    0
                };

                // Set state last (makes thread visible to scheduler)
                THREAD_STATES[i].store(thread_state::READY, Ordering::SeqCst);

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
            if THREAD_STATES[i].load(Ordering::SeqCst) == thread_state::FREE {
                // System thread stacks should be pre-allocated at correct size
                // If not, something corrupted them (bug in spawn_closure_with_stack_size)
                debug_assert!(
                    self.stacks[i].size == config::SYSTEM_THREAD_STACK_SIZE,
                    "System thread {} has wrong stack size: {} (expected {})",
                    i, self.stacks[i].size, config::SYSTEM_THREAD_STACK_SIZE
                );
                
                let stack = &self.stacks[i];
                // Initial SP is BELOW the exception area (which is at the top)
                // Exception handlers use [stack_top - EXCEPTION_STACK_SIZE, stack_top]
                // Kernel code uses [stack_base, stack_top - EXCEPTION_STACK_SIZE]
                let stack_top = ((stack.top - EXCEPTION_STACK_SIZE) & !0xF) as u64;
                
                // Get STORED boot TTBR0 (not current, which could be user process's!)
                let boot_ttbr0 = crate::mmu::get_boot_ttbr0();

                // Set up fake IRQ frame for stack-based context switching
                // When the IRQ handler restores from this stack, it will "return" to
                // thread_start_closure with x19/x20/x21 set up correctly
                let sp = setup_fake_irq_frame(
                    stack_top,
                    thread_start_closure as *const () as u64,  // ELR - where to jump
                    trampoline_fn as *const () as u64,          // x19 - trampoline
                    closure_ptr as u64,                         // x20 - closure data
                    0,                                          // x21 - enable IRQs
                );
                
                // Safe print without heap allocation
                safe_print!(256, "[spawn_system] tid={} stack_top=0x{:x} irq_frame_sp=0x{:x}\n",
                    i, stack_top, sp);

                // Write minimal context - only SP and TTBR0 are needed now
                // All other registers are on the stack in the fake IRQ frame
                unsafe {
                    let ctx = &mut *get_context_mut(i);
                    ctx.magic = CONTEXT_MAGIC;
                    ctx.sp = sp;
                    ctx.ttbr0 = boot_ttbr0;
                    // Legacy fields - kept for compatibility but not used in simple path
                    ctx.x19 = trampoline_fn as *const () as u64;
                    ctx.x20 = closure_ptr as u64;
                    ctx.x21 = 0;
                    ctx.x30 = thread_start_closure as *const () as u64;
                    ctx.elr = thread_start_closure as *const () as u64;
                    ctx.spsr = 0x00000345; // EL1h, IRQs enabled
                }

                // System threads are preemptible (not cooperative)
                self.slots[i].cooperative = false;
                self.slots[i].start_time_us = 0;
                self.slots[i].timeout_us = 0;

                THREAD_STATES[i].store(thread_state::READY, Ordering::SeqCst);

                return Ok(i);
            }
        }

        Err("No free system thread slots")
    }

    /// Spawn a thread for user processes (only in user thread range)
    ///
    /// Only searches slots RESERVED_THREADS..MAX_THREADS.
    /// Uses USER_THREAD_STACK_SIZE (128KB).
    pub fn spawn_user_closure(
        &mut self,
        trampoline_fn: fn(*mut ()) -> !,
        closure_ptr: *mut (),
        cooperative: bool,
    ) -> Result<usize, &'static str> {
        if !self.initialized {
            return Err("Thread pool not initialized");
        }

        // Only search in user thread range
        for i in config::RESERVED_THREADS..config::MAX_THREADS {
            if THREAD_STATES[i].load(Ordering::SeqCst) == thread_state::FREE {
                // User thread stacks should be pre-allocated at correct size
                debug_assert!(
                    self.stacks[i].size == config::USER_THREAD_STACK_SIZE,
                    "User thread {} has wrong stack size: {} (expected {})",
                    i, self.stacks[i].size, config::USER_THREAD_STACK_SIZE
                );
                
                let stack = &self.stacks[i];
                let stack_top = ((stack.top - EXCEPTION_STACK_SIZE) & !0xF) as u64;
                
                // Get STORED boot TTBR0
                let boot_ttbr0 = crate::mmu::get_boot_ttbr0();

                // Set up fake IRQ frame for stack-based context switching
                let sp = setup_fake_irq_frame(
                    stack_top,
                    thread_start_closure as *const () as u64,  // ELR
                    trampoline_fn as *const () as u64,          // x19 - trampoline
                    closure_ptr as u64,                         // x20 - closure data
                    0,                                          // x21 - enable IRQs
                );
                
                crate::safe_print!(96, "[spawn_user] tid={} stack_top={:#x} irq_sp={:#x}\n",
                    i, stack_top, sp);

                // Write minimal context
                unsafe {
                    let ctx = &mut *get_context_mut(i);
                    ctx.magic = CONTEXT_MAGIC;
                    ctx.sp = sp;
                    ctx.ttbr0 = boot_ttbr0;
                    // Legacy fields
                    ctx.x19 = trampoline_fn as *const () as u64;
                    ctx.x20 = closure_ptr as u64;
                    ctx.x30 = thread_start_closure as *const () as u64;
                    ctx.elr = thread_start_closure as *const () as u64;
                    ctx.spsr = 0x00000345;
                }

                self.slots[i].cooperative = cooperative;
                self.slots[i].start_time_us = 0;
                self.slots[i].timeout_us = if cooperative {
                    COOPERATIVE_TIMEOUT_US
                } else {
                    0
                };

                THREAD_STATES[i].store(thread_state::READY, Ordering::SeqCst);

                return Ok(i);
            }
        }

        Err("No free user thread slots")
    }

    /// Reclaim a terminated thread slot (just mark as Free)
    pub fn reclaim(&mut self, idx: usize) {
        if idx > 0 && idx < config::MAX_THREADS && 
           THREAD_STATES[idx].load(Ordering::SeqCst) == thread_state::TERMINATED
        {
            THREAD_STATES[idx].store(thread_state::FREE, Ordering::SeqCst);
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
            if THREAD_STATES[i].load(Ordering::SeqCst) == thread_state::TERMINATED {
                THREAD_STATES[i].store(thread_state::FREE, Ordering::SeqCst);
                // Re-initialize canary for reuse
                if config::ENABLE_STACK_CANARIES && self.stacks[i].is_allocated() {
                    init_stack_canary(self.stacks[i].base);
                }
                count += 1;
            }
        }
        count
    }

    /// Select next ready thread (round-robin) - LOCK-FREE for state transitions
    ///
    /// # Preemption rules:
    /// - `voluntary=true`: Thread yielded voluntarily (yield_now) - always switch
    /// - `voluntary=false`: Timer-triggered preemption
    ///   - If preemption is explicitly disabled: Don't switch
    ///   - Cooperative threads (thread 0): Only switch after timeout elapses
    ///   - Non-cooperative threads (sessions, user processes): Always preemptible
    pub fn schedule_indices(&mut self, voluntary: bool) -> Option<(usize, usize)> {
        // Use TPIDRRO_EL0 register for current thread ID - more reliable than atomic
        let current_idx = get_current_thread_register();
        let current = &self.slots[current_idx];

        // For timer-triggered preemption, first check if preemption is explicitly disabled.
        if !voluntary && is_preemption_disabled() {
            return None;
        }

        // For timer-triggered preemption, check if the current thread is cooperative.
        // Use atomic state check
        let current_state = THREAD_STATES[current_idx].load(Ordering::SeqCst);
        if !voluntary && current.cooperative && current_state == thread_state::RUNNING {
            let timeout = current.timeout_us;
            if timeout > 0 && current.start_time_us > 0 {
                let now = crate::timer::uptime_us();
                let elapsed = now.saturating_sub(current.start_time_us);
                if elapsed < timeout {
                    return None;
                }
            } else {
                return None;
            }
        }

        // Proportional scheduling for thread 0 (network loop)
        // Thread 0 gets boosted every NETWORK_THREAD_RATIO scheduler ticks.
        // This gives thread 0 a 1/N share of CPU time (e.g., 25% with ratio=4).
        if current_idx != 0 {
            self.network_boost_counter += 1;
            if self.network_boost_counter >= config::NETWORK_THREAD_RATIO {
                self.network_boost_counter = 0;
                let thread0_state = THREAD_STATES[0].load(Ordering::SeqCst);
                if thread0_state == thread0_state { // Always true, just to keep structure
                    let thread0_state_val = THREAD_STATES[0].load(Ordering::SeqCst);
                    if thread0_state_val == thread_state::READY {
                        if current_state != thread_state::TERMINATED && current_state != thread_state::WAITING {
                            THREAD_STATES[current_idx].store(thread_state::READY, Ordering::SeqCst);
                        }
                        THREAD_STATES[0].store(thread_state::RUNNING, Ordering::SeqCst);
                        let now = crate::timer::uptime_us();
                        self.slots[0].start_time_us = now;
                        set_current_thread_register(0);
                        self.current_idx = 0;
                        return Some((current_idx, 0));
                    }
                }
            }
        }
        // If current_idx == 0 or counter hasn't reached ratio, use round-robin below

        // First pass: Wake any WAITING threads whose wake time has passed
        let now = crate::timer::uptime_us();
        let mut woke_any = false;
        for i in 0..config::MAX_THREADS {
            if THREAD_STATES[i].load(Ordering::SeqCst) == thread_state::WAITING {
                let wake_time = WAKE_TIMES[i].load(Ordering::SeqCst);
                if wake_time > 0 && now >= wake_time {
                    // Wake this thread - mark as READY and clear wake time
                    WAKE_TIMES[i].store(0, Ordering::SeqCst);
                    THREAD_STATES[i].store(thread_state::READY, Ordering::SeqCst);
                    woke_any = true;
                }
            }
        }
        // Send event to wake any threads in WFI
        if woke_any {
            unsafe { core::arch::asm!("sev"); }
        }

        // Find next ready thread using GLOBAL round-robin index
        // This ensures fair rotation through ALL threads, not just starting from current.
        // Without this, threads 10, 11 would never run if 8, 9 are always ready and
        // the scheduler always runs from a low-numbered system thread.
        let mut next_idx = (self.round_robin_idx + 1) % config::MAX_THREADS;
        let start_idx = next_idx;

        loop {
            let state = THREAD_STATES[next_idx].load(Ordering::SeqCst);
            
            if state == thread_state::READY {
                // Found a ready thread - but skip if it's the current one
                // (we want to switch TO a different thread, not stay on current)
                if next_idx != current_idx {
                    break;
                }
            }

            next_idx = (next_idx + 1) % config::MAX_THREADS;

            if next_idx == start_idx {
                // Wrapped around without finding a different ready thread
                return None;
            }
        }
        
        // Update global round-robin index to where we found the next thread
        // This ensures the NEXT scheduling decision continues from here
        self.round_robin_idx = next_idx;

        // Update states atomically (lock-free)
        // Don't change state if thread is TERMINATED or WAITING
        // WAITING threads keep their state - scheduler handles wake time
        let current_state = THREAD_STATES[current_idx].load(Ordering::SeqCst);
        let now = crate::timer::uptime_us();

        // Accumulate CPU time for the thread being scheduled out
        if current.start_time_us > 0 {
            let elapsed = now.saturating_sub(current.start_time_us);
            TOTAL_CPU_TIMES[current_idx].fetch_add(elapsed, Ordering::Relaxed);
        }

        if current_state != thread_state::TERMINATED && current_state != thread_state::WAITING {
            THREAD_STATES[current_idx].store(thread_state::READY, Ordering::SeqCst);
        }
        THREAD_STATES[next_idx].store(thread_state::RUNNING, Ordering::SeqCst);
        
        // Update timing (still in slot, but we own it)
        self.slots[next_idx].start_time_us = now;

        // Update current thread in CPU register (authoritative source of truth)
        set_current_thread_register(next_idx);
        self.current_idx = next_idx; // Keep in sync for context access
        
        Some((current_idx, next_idx))
    }

    pub fn thread_stats(&self) -> (usize, usize, usize) {
        let mut ready = 0;
        let mut running = 0;
        let mut terminated = 0;
        // Use atomic THREAD_STATES array (source of truth)
        for i in 0..config::MAX_THREADS {
            match THREAD_STATES[i].load(Ordering::Relaxed) {
                thread_state::READY => ready += 1,
                thread_state::RUNNING => running += 1,
                thread_state::TERMINATED => terminated += 1,
                _ => {}
            }
        }
        (ready, running, terminated)
    }

    pub fn thread_count(&self) -> usize {
        // Use atomic THREAD_STATES array (source of truth)
        (0..config::MAX_THREADS)
            .filter(|&i| THREAD_STATES[i].load(Ordering::Relaxed) != thread_state::FREE)
            .count()
    }

    pub unsafe fn get_context_ptrs(
        &mut self,
        old_idx: usize,
        new_idx: usize,
    ) -> (*mut Context, *const Context) {
        // Contexts are now in THREAD_CONTEXTS static array, not in slots
        let old_ptr = get_context_mut(old_idx);
        let new_ptr = get_context(new_idx);
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
    // Print stack requirements before initialization
    print_stack_requirements();
    
    // Verify stack memory fits in available heap
    let heap_size = crate::allocator::stats().heap_size;
    if let Err(msg) = verify_stack_memory(heap_size) {
        panic!("Stack allocation failed: {}", msg);
    }
    
    // Initialize ThreadPool (allocates stacks, sets up boot thread)
    {
        let mut pool = POOL.lock();
        pool.init();
    }
    
    // Initialize atomic thread states to match ThreadPool state
    // Thread 0 is RUNNING (boot thread), all others are FREE
    THREAD_STATES[0].store(thread_state::RUNNING, Ordering::SeqCst);
    for i in 1..config::MAX_THREADS {
        THREAD_STATES[i].store(thread_state::FREE, Ordering::SeqCst);
    }
    set_current_thread_register(0);  // Initialize CPU register for boot thread
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

/// Spawn a thread with a Rust closure and options
///
/// Uses user thread slots (RESERVED_THREADS..MAX_THREADS) with fixed 128KB stacks.
pub fn spawn_fn_with_options<F>(f: F, cooperative: bool) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    spawn_user_thread_fn_with_options(f, cooperative)
}


/// DEPRECATED: Old SGI handler using switch_context
/// 
/// This handler used the old context switching mechanism with `switch_context`.
/// It has been replaced by `sgi_scheduler_handler_with_sp` which uses a unified
/// stack-based approach for both EL0 and EL1 IRQs.
/// 
/// Kept for reference and potential fallback.
#[allow(dead_code)]
pub fn sgi_scheduler_handler(irq: u32) {
    crate::gic::end_of_interrupt(irq);

    let voluntary = VOLUNTARY_SCHEDULE.swap(false, Ordering::Acquire);

    // CRITICAL: Copy all needed metadata while holding lock, then release lock BEFORE switch_context.
    // We cannot hold the lock across switch_context because:
    // 1. The new thread might get a timer IRQ and try to acquire the same lock → deadlock
    // 2. The lock guard is on the old thread's stack which we're switching away from
    //
    // Solution: Contexts are stored in THREAD_CONTEXTS static array (not behind lock).
    // We copy stack bases and exception stack pointer while locked, then release.
    let switch_info = {
        let mut pool = POOL.lock();
        pool.schedule_indices(voluntary).map(|(old_idx, new_idx)| {
            // Copy all metadata we need - lock will be released after this block
            let old_stack_base = pool.stacks[old_idx].base;
            let new_stack_base = pool.stacks[new_idx].base;
            let new_tpidr = pool.slots[new_idx].exception_stack_top;
            (old_idx, new_idx, old_stack_base, new_stack_base, new_tpidr)
        })
    };  // Lock released here - safe because contexts are in separate static array

    if let Some((old_idx, new_idx, old_stack_base, new_stack_base, new_tpidr)) = switch_info {
        if config::ENABLE_SGI_DEBUG_PRINTS {
            // Safe print without heap allocation (critical in IRQ context!)
            safe_print!(128, "[SGI] switching {} -> {}\n", old_idx, new_idx);
        }
        
        unsafe {
            // Verify stack canaries before switching (only if enabled)
            if config::ENABLE_STACK_CANARIES {
                if !check_stack_canary(old_stack_base) {
                    // Don't allocate in IRQ context!
                    safe_print!(128, "[CANARY] old thread stack corrupt\n");
                }
                if !check_stack_canary(new_stack_base) {
                    safe_print!(128, "[CANARY] new thread stack corrupt\n");
                }
            }
            
            // Access contexts from THREAD_CONTEXTS - no lock needed!
            let old_ptr = get_context_mut(old_idx);
            let new_ptr = get_context(new_idx);
            
            // Read context values for corruption checks
            let new_ctx = &*new_ptr;
            
            // PHASE 3: Check context magic values before switching
            // If magic is corrupted, the context has been overwritten
            
            // Check old context - if corrupted, we're already in trouble
            let old_ctx = &*old_ptr;
            if !old_ctx.is_valid() {
                safe_print!(256, "[SGI CORRUPT] OLD context magic invalid for thread {} - magic=0x{:x}\n", 
                    old_idx, old_ctx.magic);
            }
            
            // Check new context - if corrupted, try to recover
            // CRITICAL: We CANNOT return early here! schedule_indices already updated
            // TPIDR_EL0 to new_idx. If we return, the next timer interrupt will save
            // old_idx's CPU state into new_idx's context slot, causing corruption.
            // We MUST always call switch_context after schedule_indices returns Some.
            if !new_ctx.is_valid() {
                safe_print!(256, "[SGI CORRUPT] NEW context magic invalid for thread {} - recovering\n", new_idx);
                // Try to recover: reinitialize the context with safe values
                // Try to recover: reinitialize the context with safe values
                let ctx = &mut *get_context_mut(new_idx);
                ctx.magic = CONTEXT_MAGIC;
                ctx.spsr = 0x00000005; // EL1h, IRQs will be enabled by trampoline
                // Mark for termination AFTER we switch to it - it will terminate itself
                // on the next yield/preemption. Setting x30=0 will cause it to crash
                // safely if it tries to return.
                ctx.x30 = 0; 
                ctx.elr = 0;
                // Thread will run with corrupted state but at least won't corrupt others
                THREAD_STATES[new_idx].store(thread_state::TERMINATED, Ordering::SeqCst);
            }
            
            let new_saved_ttbr0 = new_ctx.ttbr0;
            let new_saved_spsr = new_ctx.spsr;
            let new_saved_elr = new_ctx.elr;
            let new_saved_x30 = new_ctx.x30;
            
            // CRITICAL: Detect context bugs that would cause jumps to address 0
            // This check runs ALWAYS (not behind debug flag) since it's critical
            let new_x19 = new_ctx.x19;
            if new_saved_elr == 0 && new_idx != 0 {
                crate::safe_print!(64, "[CTX BUG] tid={} ELR=0 x30={:#x}\n", new_idx, new_saved_x30);
            }
            if new_x19 == 0 && new_idx != 0 {
                crate::safe_print!(64, "[CTX BUG] tid={} x19=0 (trampoline)\n", new_idx);
            }
            if new_saved_x30 == 0 && new_idx != 0 {
                crate::safe_print!(64, "[CTX BUG] tid={} x30=0 (link reg)\n", new_idx);
            }
            
            // Check for context corruption
            let is_user_spsr = (new_saved_spsr & 0xF) == 0;
            let is_new_thread = new_saved_elr == 0 && new_saved_spsr == 0;
            
            // CRITICAL: System threads (0-7) should NEVER have user-mode SPSR!
            if new_idx < 8 && !is_new_thread && is_user_spsr {
                // Don't allocate in IRQ context!
                crate::console::print("[SGI CORRUPT] system thread has user SPSR - recovering\n");
                // Try to recover by forcing kernel mode in SPSR
                (*get_context_mut(new_idx)).spsr = 0x00000345; // EL1h, IRQs enabled
            }
            
            // For user process threads (8+), check for boot TTBR0 with user ELR
            if new_idx >= 8 && !is_new_thread {
                let is_boot_ttbr0 = new_saved_ttbr0 >= 0x4020_0000 && new_saved_ttbr0 < 0x4040_0000;
                let is_user_elr = new_saved_elr > 0 && new_saved_elr < 0x4000_0000;
                
                if is_user_elr && is_user_spsr && is_boot_ttbr0 {
                    // Don't allocate in IRQ context!
                    crate::console::print("[SGI CORRUPT] user thread has boot TTBR0\n");
                }
            }
            
            // DEBUG: Log context info for user threads (always, to diagnose hang)
            if new_idx >= 8 {
                crate::safe_print!(128, "[CTX] tid={} elr={:#x} spsr={:#x} ttbr0={:#x} sp={:#x} x19={:#x}\n",
                    new_idx, new_saved_elr, new_saved_spsr, new_saved_ttbr0, new_ctx.sp, new_x19);
            }
            
            // Update exception stack BEFORE switching
            crate::exceptions::set_current_exception_stack(new_tpidr);
            
            // Note: TPIDRRO_EL0 (thread ID) is already updated by schedule_indices
            
            // Debug: Print context SPs before switch AND actual SP register
            if config::ENABLE_SGI_DEBUG_PRINTS {
                let actual_sp: u64;
                core::arch::asm!("mov {}, sp", out(reg) actual_sp);
                let new_sp = (*new_ptr).sp;
                let new_elr = (*new_ptr).elr;
                safe_print!(256, "  SP_now=0x{:x} new_ctx.sp=0x{:x} new_ctx.elr=0x{:x}{}\n",
                    actual_sp, new_sp, new_elr,
                    if new_elr == 0 && new_idx != 0 { " *** ELR=0 BUG! ***" } else { "" });
            }
            
            switch_context(old_ptr, new_ptr);
            
            // We return here after being switched BACK to this thread
            // CRITICAL: Mask IRQs immediately to prevent nested timer interrupts
            core::arch::asm!("msr daifset, #2", options(nomem, nostack));
            
            // Debug: Print SP after return
            if config::ENABLE_SGI_DEBUG_PRINTS {
                let current_sp: u64;
                core::arch::asm!("mov {}, sp", out(reg) current_sp);
                crate::console::print("[SGI] back, SP=0x");
                crate::console::print_hex(current_sp);
                crate::console::print("\n");
            }
            
            // Check for TTBR0/SPSR mismatch - detect context corruption early
            let current_ttbr0: u64;
            let current_spsr: u64;
            core::arch::asm!("mrs {}, ttbr0_el1", out(reg) current_ttbr0);
            core::arch::asm!("mrs {}, spsr_el1", out(reg) current_spsr);
            
            // SPSR bits [3:0] = 0 means EL0 (user mode)
            let returning_to_user = (current_spsr & 0xF) == 0;
            // Boot TTBR0 is typically in 0x402xxxxx range
            let has_boot_ttbr0 = current_ttbr0 >= 0x4020_0000 && current_ttbr0 < 0x4040_0000;
            
            if returning_to_user && has_boot_ttbr0 {
                // Don't allocate in IRQ context!
                crate::console::print("[SGI DANGER] returning to EL0 with boot TTBR0\n");
            }
            
            if config::ENABLE_SGI_DEBUG_PRINTS {
                // Add sequence number, SP, and x30 to help debug double-return issue
                // Safe print without heap allocation (critical in IRQ context!)
                static RETURN_SEQ: core::sync::atomic::AtomicU64 = 
                    core::sync::atomic::AtomicU64::new(0);
                let seq = RETURN_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                let current_sp: u64;
                let current_x30: u64;
                let current_elr: u64;
                unsafe {
                    core::arch::asm!("mov {}, sp", out(reg) current_sp);
                    core::arch::asm!("mov {}, x30", out(reg) current_x30);
                    core::arch::asm!("mrs {}, elr_el1", out(reg) current_elr);
                }
                safe_print!(512, "[SGI] returned to tid={} seq={} SP=0x{:x} x30=0x{:x} ELR=0x{:x}\n",
                    old_idx, seq, current_sp, current_x30, current_elr);
            }
            
            // Re-enable IRQs before returning from handler
            // This is safe now - we've finished all critical handler work
            unsafe {
                core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
            }
        }
    }
}

/// Yield to another thread
pub fn yield_now() {
    VOLUNTARY_SCHEDULE.store(true, Ordering::Release);
    crate::gic::trigger_sgi(crate::gic::SGI_SCHEDULER);
}

/// SIMPLIFIED SGI handler for stack-based context switching
/// 
/// Takes current SP from assembly, returns new SP if switch needed (or 0).
/// The assembly does the actual SP switch AFTER this function returns.
/// This avoids the problem of switching SP in the middle of Rust code.
pub fn sgi_scheduler_handler_with_sp(irq: u32, current_sp: u64) -> u64 {
    crate::gic::end_of_interrupt(irq);
    
    let voluntary = VOLUNTARY_SCHEDULE.swap(false, Ordering::Acquire);
    
    // Get scheduling decision
    let switch_info = {
        let mut pool = POOL.lock();
        pool.schedule_indices(voluntary).map(|(old_idx, new_idx)| {
            let new_tpidr = pool.slots[new_idx].exception_stack_top;
            (old_idx, new_idx, new_tpidr)
        })
    };
    
    if let Some((old_idx, new_idx, new_tpidr)) = switch_info {
        if config::ENABLE_SGI_DEBUG_PRINTS {
            crate::safe_print!(64, "[SGI-S] {} -> {}\n", old_idx, new_idx);
        }
        
        unsafe {
            // Get context pointers
            let old_ctx = get_context_mut(old_idx);
            let new_ctx = get_context(new_idx);
            
            // Save current SP (from IRQ frame) to old context
            (*old_ctx).sp = current_sp;
            
            // Save current TTBR0 to old context
            // CRITICAL: Processes set their own TTBR0 via activate(), 
            // so we must save it here to restore correctly later
            let current_ttbr0: u64;
            core::arch::asm!("mrs {}, ttbr0_el1", out(reg) current_ttbr0);
            (*old_ctx).ttbr0 = current_ttbr0;
            
            // Load new SP from new context
            let new_sp = (*new_ctx).sp;
            
            // Verify new SP is valid
            if new_sp == 0 || new_sp < 0x4000_0000 {
                crate::safe_print!(64, "[SGI-S FATAL] new_sp={:#x} invalid!\n", new_sp);
                loop { core::arch::asm!("wfi"); }
            }
            
            // Update exception stack for new thread
            crate::exceptions::set_current_exception_stack(new_tpidr);
            
            // Load TTBR0 for new thread
            let new_ttbr0 = (*new_ctx).ttbr0;
            core::arch::asm!(
                "dsb ish",
                "msr ttbr0_el1, {ttbr0}",
                "isb",
                "tlbi vmalle1",
                "dsb ish",
                "isb",
                ttbr0 = in(reg) new_ttbr0,
            );
            
            if config::ENABLE_SGI_DEBUG_PRINTS {
                crate::safe_print!(64, "[SGI-S] returning new_sp={:#x}\n", new_sp);
            }
            
            // Return new SP - assembly will do the switch
            return new_sp;
        }
    }
    
    0  // No switch needed
}

/// Update a thread's context for a new execution (e.g., after execve or fork)
pub fn update_thread_context(thread_id: usize, user_context: &crate::process::UserContext) {
    // Disable IRQs to safely access context
    crate::irq::with_irqs_disabled(|| {
        unsafe {
            let ctx = &mut *get_context_mut(thread_id);
            
            // Update context fields that are directly in Context struct
            ctx.elr = user_context.pc;
            // ctx.sp points to the kernel stack top (where the trap frame is).
            // We generally don't change ctx.sp for fork(), we want to keep the stack frame.
            // But for execve(), we might want to reset it?
            // For fork(), the thread is NEW, so ctx.sp points to the fake frame we just built.
            // We should NOT change ctx.sp to user_context.sp (which is a user stack pointer!).
            
            ctx.spsr = user_context.spsr;
            ctx.ttbr0 = user_context.ttbr0;
            
            ctx.user_entry = user_context.pc;
            ctx.user_sp = user_context.sp;
            ctx.user_tls = user_context.tpidr;
            ctx.is_user_process = 1;
            
            // Update registers in the trap frame on the stack
            // The trap frame is at ctx.sp
            let frame_ptr = ctx.sp as *mut u64;
            
            // Frame layout from setup_fake_irq_frame / IRQ handler:
            // [sp+224]: x0, x1
            frame_ptr.add(224/8).write_volatile(user_context.x0);
            frame_ptr.add(232/8).write_volatile(user_context.x1);
            
            // We can update other registers if needed, but for fork() x0=0 is the main one.
            // The trap frame has 0 for others by default (from setup_fake_irq_frame).
            // If we want to copy all registers from parent (for full fork), we should do it here.
            // But UserContext only has x0-x30 if we added them.
            // For now, updating x0 is sufficient for vfork return value.
        }
    });
}

// ============================================================================
// Waker Integration
// ============================================================================

use core::task::{RawWaker, RawWakerVTable, Waker};

/// Waker implementation for thread-based waking
pub struct ThreadWaker {
    thread_id: usize,
}

impl ThreadWaker {
    pub fn new(thread_id: usize) -> Self {
        Self { thread_id }
    }

    /// Wake the thread associated with this waker
    pub fn wake(&self) {
        let tid = self.thread_id;
        if tid < config::MAX_THREADS {
            // Set sticky wake flag so schedule_blocking knows we were woken
            WOKEN_STATES[tid].store(true, Ordering::SeqCst);

            // Only wake if thread is actually WAITING
            if THREAD_STATES[tid].load(Ordering::SeqCst) == thread_state::WAITING {
                WAKE_TIMES[tid].store(0, Ordering::SeqCst);
                THREAD_STATES[tid].store(thread_state::READY, Ordering::SeqCst);
                // Trigger SGI to ensure scheduler runs and picks up the thread
                crate::gic::trigger_sgi(crate::gic::SGI_SCHEDULER);
            }
        }
    }
}

/// Marks the thread with the given ID as READY.
fn mark_thread_ready_from_waker(thread_id: usize) {
    let waker = ThreadWaker::new(thread_id);
    waker.wake();
}

/// Creates a RawWaker that, when woken, marks the specified thread as READY.
fn waker_from_thread_id(thread_id: usize) -> RawWaker {
    let ptr = thread_id as *const ();
    RawWaker::new(ptr, &THREAD_WAKER_VTABLE)
}

/// Creates a waker for the current thread.
pub fn current_thread_waker() -> Waker {
    let tid = get_current_thread_register();
    unsafe { Waker::from_raw(waker_from_thread_id(tid)) }
}

const THREAD_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    thread_waker_clone,
    thread_waker_wake,
    thread_waker_wake_by_ref,
    thread_waker_drop,
);

unsafe fn thread_waker_clone(data: *const ()) -> RawWaker {
    waker_from_thread_id(data as usize)
}

unsafe fn thread_waker_wake(data: *const ()) {
    mark_thread_ready_from_waker(data as usize);
}

unsafe fn thread_waker_wake_by_ref(data: *const ()) {
    mark_thread_ready_from_waker(data as usize);
}

unsafe fn thread_waker_drop(_data: *const ()) {
    // No-op, waker doesn't own any resources
}

/// Returns a Waker for the specified thread ID.
/// When this waker is invoked (wake() is called), the target thread will be marked READY.
pub fn get_waker_for_thread(thread_id: usize) -> Waker {
    unsafe { Waker::from_raw(waker_from_thread_id(thread_id)) }
}

/// Block the current thread until the specified wake time, then yield
/// 
/// This is safe to call from syscall handlers. The thread will be marked as
/// WAITING and will not be scheduled until:
/// 1. The wake_time_us deadline has passed, OR
/// 2. An external event wakes the thread (not yet implemented)
///
/// When the thread is woken, it resumes execution right after this function returns.
/// 
/// # TTBR0 Handling
/// 
/// When called from a syscall context, TTBR0 contains user page tables.
/// We must switch to kernel (boot) TTBR0 before yielding so that:
/// 1. switch_context saves kernel TTBR0, not user TTBR0
/// 2. When resumed, kernel code can access all kernel memory
/// 
/// After resuming, we restore the user TTBR0 before returning to syscall handler.
pub fn schedule_blocking(wake_time_us: u64) {
    let tid = current_thread_id();
    
    // Check if we were already woken (sticky wake)
    if WOKEN_STATES[tid].swap(false, Ordering::SeqCst) {
        return;
    }

    let now = crate::timer::uptime_us();
    
    // Check if already past deadline - don't bother blocking
    if now >= wake_time_us {
        return;
    }

    // Save current preemption state and ensure it's enabled for the block
    let was_disabled = is_preemption_disabled();
    if was_disabled {
        // Log this as it might be a sign of a bug (blocking while holding a lock)
        // but we'll allow it by temporarily enabling preemption.
        // crate::safe_print!(64, "[threading] schedule_blocking called with preemption disabled (tid={})\n", tid);
        
        // We MUST enable preemption here, otherwise the timer IRQ will acknowledge
        // but will NOT schedule another thread, leading to a hang in the wfi loop.
        enable_preemption();
    }
    
    // Mark thread as WAITING with wake time
    mark_thread_waiting(tid, wake_time_us);
    
    // Wait for timer to preempt us and for scheduler to wake us
    loop {
        // Double check sticky wake flag in loop
        if WOKEN_STATES[tid].swap(false, Ordering::SeqCst) {
            WAKE_TIMES[tid].store(0, Ordering::SeqCst);
            THREAD_STATES[tid].store(thread_state::RUNNING, Ordering::SeqCst);
            break;
        }

        let state = THREAD_STATES[tid].load(Ordering::SeqCst);
        if state != thread_state::WAITING {
            break;
        }
        
        if crate::process::is_current_interrupted() {
            WAKE_TIMES[tid].store(0, Ordering::SeqCst);
            THREAD_STATES[tid].store(thread_state::RUNNING, Ordering::SeqCst);
            break;
        }
        
        // Wait for interrupt - timer IRQ will fire within 10ms
        unsafe { core::arch::asm!("wfi"); }
    }

    // Restore preemption state
    if was_disabled {
        disable_preemption();
    }
}

/// Get thread stats (ready, running, terminated) - LOCK-FREE
pub fn thread_stats() -> (usize, usize, usize) {
    let mut ready = 0;
    let mut running = 0;
    let mut terminated = 0;
    for i in 0..config::MAX_THREADS {
        match THREAD_STATES[i].load(Ordering::Relaxed) {
            thread_state::READY => ready += 1,
            thread_state::RUNNING => running += 1,
            thread_state::TERMINATED => terminated += 1,
            _ => {}
        }
    }
    (ready, running, terminated)
}

/// Thread state counts for all states
pub struct ThreadStatsFull {
    pub free: usize,
    pub ready: usize,
    pub running: usize,
    pub terminated: usize,
    pub initializing: usize,
    pub waiting: usize,
}

/// Get counts for all thread states (lock-free)
pub fn thread_stats_full() -> ThreadStatsFull {
    let mut stats = ThreadStatsFull {
        free: 0,
        ready: 0,
        running: 0,
        terminated: 0,
        initializing: 0,
        waiting: 0,
    };
    for i in 0..config::MAX_THREADS {
        match THREAD_STATES[i].load(Ordering::Relaxed) {
            thread_state::FREE => stats.free += 1,
            thread_state::READY => stats.ready += 1,
            thread_state::RUNNING => stats.running += 1,
            thread_state::TERMINATED => stats.terminated += 1,
            thread_state::INITIALIZING => stats.initializing += 1,
            thread_state::WAITING => stats.waiting += 1,
            _ => {}
        }
    }
    stats
}

/// Clean up terminated threads (mark slots as free) - LOCK-FREE
pub fn cleanup_terminated() -> usize {
    cleanup_terminated_lockfree()
}

/// Get active thread count
pub fn thread_count() -> usize {
    // Lock-free: count non-free threads
    (0..config::MAX_THREADS)
        .filter(|&i| THREAD_STATES[i].load(Ordering::Relaxed) != thread_state::FREE)
        .count()
}

/// Get stack info for a specific thread (base, top)
/// Returns None if thread index is invalid
pub fn get_thread_stack_info(tid: usize) -> Option<(usize, usize)> {
    if tid >= config::MAX_THREADS {
        return None;
    }
    // Disable IRQs to prevent deadlock if timer fires while we hold the lock
    with_irqs_disabled(|| {
        let pool = POOL.lock();
        let stack = &pool.stacks[tid];
        if stack.base == 0 {
            None
        } else {
            Some((stack.base, stack.top))
        }
    })
}

/// Mark current thread as terminated (thread 0 cannot be terminated) - LOCK-FREE
pub fn mark_current_terminated() {
    let idx = get_current_thread_register();
    if idx != IDLE_THREAD_IDX {
        mark_thread_terminated(idx);
    }
}

/// Get current thread ID from TPIDR_EL0 register
/// This is more reliable than a global atomic as it's per-CPU
#[inline]
pub fn current_thread_id() -> usize {
    get_current_thread_register()
}

/// Get max thread count
pub fn max_threads() -> usize {
    config::MAX_THREADS
}

// ============================================================================
// System Thread API (for SSH sessions, etc.)
// ============================================================================

/// Spawn a thread specifically for system services (SSH sessions, etc.) - LOCK-FREE
///
/// Only spawns in slots 1..RESERVED_THREADS (system thread range).
/// These threads get larger stacks (256KB) and are preemptible.
/// Returns the thread ID or error if no system thread slots are available.
pub fn spawn_system_thread_fn<F>(f: F) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    // Step 1: Atomically claim a free slot (lock-free)
    let slot_idx = match claim_free_slot(1, config::RESERVED_THREADS) {
        Some(idx) => idx,
        None => return Err("No free system thread slots"),
    };
    
    // Step 2: Box the closure (heap allocation - no lock held!)
    let boxed: Box<F> = Box::new(f);
    let closure_ptr = Box::into_raw(boxed) as *mut ();
    let trampoline: fn(*mut ()) -> ! = closure_trampoline::<F>;

    // Step 3: Set up fake IRQ frame and context
    // This enables stack-based context switching
    with_irqs_disabled(|| {
        // Get stack info from POOL (brief lock)
        let stack_top = {
            let pool = POOL.lock();
            let stack = &pool.stacks[slot_idx];
            // Initial stack top is BELOW the exception area
            ((stack.top - EXCEPTION_STACK_SIZE) & !0xF) as u64
        };
        
        // Get STORED boot TTBR0 (not current, which could be user process's!)
        let boot_ttbr0 = crate::mmu::get_boot_ttbr0();

        // Set up fake IRQ frame for stack-based context switching
        let sp = setup_fake_irq_frame(
            stack_top,
            thread_start_closure as *const () as u64,  // ELR - where to jump
            trampoline as *const () as u64,            // x19 - trampoline
            closure_ptr as u64,                        // x20 - closure data
            0,                                         // x21 - enable IRQs
        );
        
        // Debug output
        crate::safe_print!(128, "[spawn_system_fn SIMPLE] tid={} stack_top={:#x} irq_sp={:#x}\n",
            slot_idx, stack_top, sp);

        // Write minimal context - only SP and TTBR0 needed for simple path
        unsafe {
            let ctx = &mut *get_context_mut(slot_idx);
            ctx.magic = CONTEXT_MAGIC;
            ctx.sp = sp;
            ctx.ttbr0 = boot_ttbr0;
            // Legacy fields for compatibility
            ctx.x19 = trampoline as *const () as u64;
            ctx.x20 = closure_ptr as u64;
            ctx.x30 = thread_start_closure as *const () as u64;
            ctx.elr = thread_start_closure as *const () as u64;
            ctx.spsr = 0x00000345;
        }

        // Write slot metadata (needs POOL lock)
        {
            let mut pool = POOL.lock();
            pool.slots[slot_idx].cooperative = false; // System threads are preemptible
            pool.slots[slot_idx].start_time_us = 0;
            pool.slots[slot_idx].timeout_us = 0;
        }
        
        // NOW set atomic state to READY - context is fully set up, scheduler can run it
        THREAD_STATES[slot_idx].store(thread_state::READY, Ordering::SeqCst);
    });
    
    Ok(slot_idx)
}

/// Count available system thread slots
///
/// Returns the number of free slots in the system thread range (1..RESERVED_THREADS).
pub fn system_threads_available() -> usize {
    // Lock-free: count free system thread slots
    count_free_slots(1, config::RESERVED_THREADS)
}

/// Count active system threads
///
/// Returns the number of non-free slots in the system thread range (1..RESERVED_THREADS).
pub fn system_threads_active() -> usize {
    // Lock-free: count non-free system thread slots
    (1..config::RESERVED_THREADS)
        .filter(|&i| THREAD_STATES[i].load(Ordering::Relaxed) != thread_state::FREE)
        .count()
}

// ============================================================================
// User Process Thread API
// ============================================================================

/// Spawn a thread specifically for user processes
///
/// Only spawns in slots RESERVED_THREADS..MAX_THREADS (user thread range).
/// Returns the thread ID or error if no user thread slots are available.
/// User threads are preemptive by default.
pub fn spawn_user_thread_fn<F>(f: F) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    spawn_user_thread_fn_internal(f, false, false)
}

/// Spawn a user thread for running a user PROCESS
///
/// This variant starts with IRQs DISABLED to prevent the race condition where
/// timer fires before activate() sets the user TTBR0. The closure MUST call
/// enable_irqs() after setting up the user address space.
pub fn spawn_user_thread_fn_for_process<F>(f: F) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    spawn_user_thread_fn_internal(f, false, true)
}

/// Spawn a user thread with cooperative option - LOCK-FREE (legacy wrapper)
pub fn spawn_user_thread_fn_with_options<F>(f: F, cooperative: bool) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    spawn_user_thread_fn_internal(f, cooperative, false)
}

/// Internal implementation for spawning user threads
/// 
/// - cooperative: if true, thread runs cooperatively (longer time slice, no forced preemption)
/// - start_irqs_disabled: if true, thread starts with IRQs disabled (for process threads)
fn spawn_user_thread_fn_internal<F>(f: F, cooperative: bool, start_irqs_disabled: bool) -> Result<usize, &'static str>
where
    F: FnOnce() -> ! + Send + 'static,
{
    // Step 1: Atomically claim a free slot (lock-free)
    let slot_idx = match claim_free_slot(config::RESERVED_THREADS, config::MAX_THREADS) {
        Some(idx) => idx,
        None => return Err("No free user thread slots"),
    };
    
    // Step 2: Box the closure (heap allocation - no lock held!)
    let boxed: Box<F> = Box::new(f);
    let closure_ptr = Box::into_raw(boxed) as *mut ();
    let trampoline: fn(*mut ()) -> ! = closure_trampoline::<F>;

    // Step 3: Set up fake IRQ frame and minimal context
    // This enables stack-based context switching
    with_irqs_disabled(|| {
        // Get stack info from POOL (brief lock)
        let stack_top = {
            let pool = POOL.lock();
            let stack = &pool.stacks[slot_idx];
            // Initial stack top is BELOW the exception area
            ((stack.top - EXCEPTION_STACK_SIZE) & !0xF) as u64
        };
        
        // Get STORED boot TTBR0 (not current, which could be user process's!)
        let boot_ttbr0 = crate::mmu::get_boot_ttbr0();

        // x21 = IRQ enable flag: 0 = enable, non-zero = keep disabled
        let x21 = if start_irqs_disabled { 1u64 } else { 0u64 };

        // Set up fake IRQ frame for stack-based context switching
        let sp = setup_fake_irq_frame(
            stack_top,
            thread_start_closure as *const () as u64,  // ELR - where to jump
            trampoline as *const () as u64,            // x19 - trampoline
            closure_ptr as u64,                        // x20 - closure data
            x21,                                       // x21 - IRQ enable flag
        );

        // Write minimal context - only SP and TTBR0 needed for stack-based switching
        unsafe {
            let ctx = &mut *get_context_mut(slot_idx);
            ctx.magic = CONTEXT_MAGIC;
            ctx.sp = sp;
            ctx.ttbr0 = boot_ttbr0;
            // Legacy fields for compatibility with old scheduler path
            ctx.x19 = trampoline as *const () as u64;
            ctx.x20 = closure_ptr as u64;
            ctx.x21 = x21;
            ctx.x30 = thread_start_closure as *const () as u64;
            ctx.elr = thread_start_closure as *const () as u64;
            ctx.spsr = 0x00000345; // EL1h, IRQs enabled
        }

        // Write slot metadata (needs POOL lock)
        {
            let mut pool = POOL.lock();
            pool.slots[slot_idx].cooperative = cooperative;
            pool.slots[slot_idx].start_time_us = 0;
            pool.slots[slot_idx].timeout_us = if cooperative { COOPERATIVE_TIMEOUT_US } else { 0 };
        }
        
        // NOW set atomic state to READY - context is fully set up, scheduler can run it
        THREAD_STATES[slot_idx].store(thread_state::READY, Ordering::SeqCst);
    });
    
    Ok(slot_idx)
}

/// Count available user thread slots

/// Count available user thread slots
///
/// Returns the number of free slots in the user thread range (RESERVED_THREADS..MAX_THREADS).
pub fn user_threads_available() -> usize {
    // Lock-free: count free user thread slots
    count_free_slots(config::RESERVED_THREADS, config::MAX_THREADS)
}

/// Count active user threads
///
/// Returns the number of non-free slots in the user thread range.
pub fn user_threads_active() -> usize {
    // Lock-free: count non-free user thread slots
    (config::RESERVED_THREADS..config::MAX_THREADS)
        .filter(|&i| THREAD_STATES[i].load(Ordering::Relaxed) != thread_state::FREE)
        .count()
}

// Note: is_thread_terminated is defined above using lock-free atomics

/// Get the state of a specific thread (for debugging) - LOCK-FREE
pub fn get_thread_state_enum(thread_id: usize) -> Option<ThreadState> {
    if thread_id >= config::MAX_THREADS {
        return None;
    }
    let state = THREAD_STATES[thread_id].load(Ordering::Relaxed);
    Some(match state {
        thread_state::FREE => ThreadState::Free,
        thread_state::READY => ThreadState::Ready,
        thread_state::RUNNING => ThreadState::Running,
        thread_state::TERMINATED => ThreadState::Terminated,
        thread_state::INITIALIZING => ThreadState::Ready, // Treat as ready for display
        _ => ThreadState::Free,
    })
}

/// Get the saved user context for a thread
/// Used by fork() to duplicate the parent's state
pub fn get_saved_user_context(thread_id: usize) -> Option<crate::process::UserContext> {
    if thread_id >= config::MAX_THREADS {
        return None;
    }
    
    crate::irq::with_irqs_disabled(|| {
        let ctx_ptr = get_context(thread_id);
        let ctx = unsafe { &*ctx_ptr };
        
        // Only return if it looks like a valid user context
        if ctx.is_user_process != 0 {
            Some(crate::process::UserContext {
                pc: ctx.user_entry,
                sp: ctx.user_sp,
                tpidr: ctx.user_tls,
                spsr: ctx.spsr,
                ttbr0: ctx.ttbr0,
                // General purpose registers are not fully tracked in UserContext struct yet
                // but for fork() we primarily need PC/SP/SPSR/TTBR0.
                // The trap handler saves GP regs to kernel stack, which we can't easily access here.
                // However, for the child process returning 0 from fork(), we set x0=0 explicitly.
                // The other registers will be zeroed/undefined in the new thread unless we copy them.
                // TODO: For full fork support, we need to copy all GP registers from the trap frame.
                x0: 0, x1: 0, x2: 0, x3: 0, x4: 0, x5: 0, x6: 0, x7: 0,
                x8: 0, x9: 0, x10: 0, x11: 0, x12: 0, x13: 0, x14: 0, x15: 0,
                x16: 0, x17: 0, x18: 0, x19: 0, x20: 0, x21: 0, x22: 0, x23: 0,
                x24: 0, x25: 0, x26: 0, x27: 0, x28: 0, x29: 0, x30: 0,
            })
        } else {
            None
        }
    })
}

/// Spawn a user thread with a specific trampoline and data pointer
/// Used by fork_process to spawn the child thread
pub fn spawn_user_thread(
    trampoline_fn: extern "C" fn() -> !,
    data_ptr: *mut (), // Passed as x0 to trampoline? No, entry_point_trampoline takes no args
    cooperative: bool
) -> Result<usize, &'static str> {
    // We reuse spawn_user_closure but cast our function
    // entry_point_trampoline doesn't take args, so data_ptr is ignored by it
    // but spawn_user_closure expects a function taking *mut ()
    
    let trampoline_casted = unsafe {
        core::mem::transmute::<
            extern "C" fn() -> !,
            fn(*mut ()) -> !
        >(trampoline_fn)
    };
    
    with_irqs_disabled(|| {
        let mut pool = POOL.lock();
        pool.spawn_user_closure(trampoline_casted, data_ptr, cooperative)
    })
}

// ============================================================================
// Stack Protection Functions
// ============================================================================

/// Check all thread stacks for overlap (debug/diagnostic)
/// Returns list of (thread_a, thread_b) pairs that overlap
pub fn check_stack_overlaps() -> Vec<(usize, usize)> {
    // Copy stack info while holding lock (quick), process outside
    let stacks: [StackInfo; config::MAX_THREADS] = with_irqs_disabled(|| {
        let pool = POOL.lock();
        pool.stacks
    });

    // O(n²) check done outside critical section
    let mut overlaps = Vec::new();
    for i in 0..config::MAX_THREADS {
        for j in (i + 1)..config::MAX_THREADS {
            if stacks[i].overlaps(&stacks[j]) {
                overlaps.push((i, j));
            }
        }
    }
    overlaps
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

    // Copy stack info quickly while holding lock
    let stacks: [StackInfo; config::MAX_THREADS] = with_irqs_disabled(|| {
        let pool = POOL.lock();
        pool.stacks
    });

    // Check canaries outside critical section (memory reads can be slow)
    let mut bad = Vec::new();
    for i in 1..config::MAX_THREADS {
        if stacks[i].is_allocated() && !check_stack_canary(stacks[i].base) {
            bad.push(i);
        }
    }
    bad
}

/// Check if there are any threads ready to run
pub fn has_ready_threads() -> bool {
    // Use atomic THREAD_STATES array (lock-free)
    (0..config::MAX_THREADS)
        .any(|i| THREAD_STATES[i].load(Ordering::Relaxed) == thread_state::READY)
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

/// Snapshot data copied from thread pool (to minimize IRQ-disabled time)
struct ThreadPoolSnapshot {
    states: [ThreadState; config::MAX_THREADS],
    cooperative: [bool; config::MAX_THREADS],
    sps: [u64; config::MAX_THREADS],
    stacks: [StackInfo; config::MAX_THREADS],
}

/// Get list of all kernel threads with their info
pub fn list_kernel_threads() -> Vec<KernelThreadInfo> {
    // Take a quick snapshot - read atomic states (lock-free) and pool data (brief lock)
    let snapshot: ThreadPoolSnapshot = with_irqs_disabled(|| {
        let pool = POOL.lock();
        
        let mut states = [ThreadState::Free; config::MAX_THREADS];
        let mut cooperative = [false; config::MAX_THREADS];
        let mut sps = [0u64; config::MAX_THREADS];
        
        for i in 0..config::MAX_THREADS {
            // Read state from atomic array (lock-free source of truth)
            states[i] = match THREAD_STATES[i].load(Ordering::Relaxed) {
                thread_state::FREE => ThreadState::Free,
                thread_state::READY => ThreadState::Ready,
                thread_state::RUNNING => ThreadState::Running,
                thread_state::TERMINATED => ThreadState::Terminated,
                thread_state::INITIALIZING => ThreadState::Ready, // Show as ready (being set up)
                thread_state::WAITING => ThreadState::Ready, // Show waiting threads
                _ => ThreadState::Free,
            };
            cooperative[i] = pool.slots[i].cooperative;
            // Read SP from THREAD_CONTEXTS (not from slot)
            sps[i] = unsafe { (*get_context(i)).sp };
        }
        
        ThreadPoolSnapshot {
            states,
            cooperative,
            sps,
            stacks: pool.stacks,
        }
    });

    // Process snapshot outside critical section (Vec allocation, canary checks, etc.)
    let mut threads = Vec::new();

    for i in 0..config::MAX_THREADS {
        // Skip free slots
        if snapshot.states[i] == ThreadState::Free {
            continue;
        }

        let state_str = match snapshot.states[i] {
            ThreadState::Free => "free",
            ThreadState::Ready => "ready",
            ThreadState::Running => "running",
            ThreadState::Terminated => "zombie",
        };

        let stack = &snapshot.stacks[i];
        let sp = snapshot.sps[i];

        // Estimate stack usage from saved SP in context
        let stack_used = if stack.is_allocated() && sp != 0 {
            let sp_usize = sp as usize;
            if sp_usize >= stack.base && sp_usize <= stack.top {
                stack.top.saturating_sub(sp_usize)
            } else {
                0
            }
        } else {
            0
        };

        // Check canary status (memory read, done outside lock)
        let canary_ok = if i == 0 || !stack.is_allocated() {
            true
        } else if config::ENABLE_STACK_CANARIES {
            check_stack_canary(stack.base)
        } else {
            true
        };

        // Thread name based on index range and state
        let name = match i {
            0 => "bootstrap",
            1 => if crate::config::COOPERATIVE_MAIN_THREAD { "system-thread" } else { "network" },
            2..=7 => "system-thread",
            _ if snapshot.cooperative[i] => "cooperative",
            _ => "user-process",
        };

        threads.push(KernelThreadInfo {
            tid: i,
            state: state_str,
            cooperative: snapshot.cooperative[i],
            stack_base: stack.base,
            stack_size: stack.size,
            stack_used,
            canary_ok,
            name,
        });
    }

    threads
}

pub fn dump_stack_info() {
    use crate::threading;
    let threads = threading::list_kernel_threads();

    for t in threads {
        let size_kb = t.stack_size / 1024;
        let used_kb = t.stack_used / 1024;
        crate::safe_print!(192, "Thread ID: {} State: {} Cooperative: {} Stack Size: {} KB Used: {} KB\n", t.tid, t.state, t.cooperative, size_kb, used_kb);
    }
}

// ============================================================================
// Stack Memory Verification
// ============================================================================

/// Stack allocation summary for verification
#[derive(Debug, Clone)]
pub struct StackAllocationSummary {
    /// Total stack memory required (bytes)
    pub total_bytes: usize,
    /// Boot stack size (thread 0)
    pub boot_stack: usize,
    /// Number of system threads (1..RESERVED_THREADS)
    pub system_thread_count: usize,
    /// Size per system thread stack
    pub system_stack_size: usize,
    /// Total system thread stack memory
    pub system_total: usize,
    /// Number of user threads (RESERVED_THREADS..MAX_THREADS)  
    pub user_thread_count: usize,
    /// Size per user thread stack
    pub user_stack_size: usize,
    /// Total user thread stack memory
    pub user_total: usize,
    /// Exception stack size per thread (reserved at top of each stack)
    pub exception_stack_size: usize,
    /// Usable kernel stack per thread (total - exception area)
    pub usable_kernel_stack: usize,
}

/// Calculate the total stack memory required based on kernel config
/// 
/// This function computes the expected stack allocation based on:
/// - Thread 0: KERNEL_STACK_SIZE (boot stack, fixed location)
/// - Threads 1 to RESERVED_THREADS-1: SYSTEM_THREAD_STACK_SIZE each
/// - Threads RESERVED_THREADS to MAX_THREADS-1: USER_THREAD_STACK_SIZE each
///
/// Each stack is divided into:
/// - Exception area (EXCEPTION_STACK_SIZE) at top: for IRQ/syscall handlers
/// - Usable kernel stack below: for normal kernel code (execute(), etc.)
///
/// Note: Thread 0's boot stack is at a fixed address and doesn't count
/// against heap memory, but is included for completeness.
pub fn calculate_stack_requirements() -> StackAllocationSummary {
    let system_thread_count = config::RESERVED_THREADS - 1; // Threads 1 to RESERVED_THREADS-1
    let user_thread_count = config::MAX_THREADS - config::RESERVED_THREADS;
    
    let system_total = system_thread_count * config::SYSTEM_THREAD_STACK_SIZE;
    let user_total = user_thread_count * config::USER_THREAD_STACK_SIZE;
    
    // Boot stack is at fixed location, not from heap
    let heap_allocated = system_total + user_total;
    
    // Calculate usable kernel stack (smallest of the stack types minus exception area)
    let min_stack = config::SYSTEM_THREAD_STACK_SIZE.min(config::USER_THREAD_STACK_SIZE);
    let usable_kernel_stack = min_stack.saturating_sub(EXCEPTION_STACK_SIZE);
    
    StackAllocationSummary {
        total_bytes: config::KERNEL_STACK_SIZE + heap_allocated,
        boot_stack: config::KERNEL_STACK_SIZE,
        system_thread_count,
        system_stack_size: config::SYSTEM_THREAD_STACK_SIZE,
        system_total,
        user_thread_count,
        user_stack_size: config::USER_THREAD_STACK_SIZE,
        user_total,
        exception_stack_size: EXCEPTION_STACK_SIZE,
        usable_kernel_stack,
    }
}

/// Verify that stack allocations fit within available heap memory
///
/// Returns Ok(summary) if stacks fit, Err with message if not.
///
/// # Arguments
/// * `available_heap` - Total heap memory available (bytes)
pub fn verify_stack_memory(available_heap: usize) -> Result<StackAllocationSummary, alloc::string::String> {
    let summary = calculate_stack_requirements();
    
    // Check that exception stack doesn't consume the entire stack
    // We need at least 8KB of usable kernel stack for execute() and other kernel code
    const MIN_USABLE_KERNEL_STACK: usize = 8 * 1024;
    if summary.usable_kernel_stack < MIN_USABLE_KERNEL_STACK {
        return Err(alloc::format!(
            "Exception stack too large! Usable kernel stack: {} KB < {} KB minimum\n\
             Stack layout per thread:\n\
             - Total stack: {} KB\n\
             - Exception area (at top): {} KB\n\
             - Usable kernel stack: {} KB\n\
             Reduce EXCEPTION_STACK_SIZE or increase thread stack sizes.",
            summary.usable_kernel_stack / 1024,
            MIN_USABLE_KERNEL_STACK / 1024,
            summary.system_stack_size.min(summary.user_stack_size) / 1024,
            summary.exception_stack_size / 1024,
            summary.usable_kernel_stack / 1024,
        ));
    }
    
    // Only system and user thread stacks come from heap
    // Boot stack is at fixed location
    let heap_required = summary.system_total + summary.user_total;
    
    if heap_required > available_heap {
        return Err(alloc::format!(
            "Stack memory exceeds heap! Required: {} KB, Available: {} KB\n\
             Breakdown:\n\
             - {} system threads × {} KB = {} KB\n\
             - {} user threads × {} KB = {} KB",
            heap_required / 1024,
            available_heap / 1024,
            summary.system_thread_count,
            summary.system_stack_size / 1024,
            summary.system_total / 1024,
            summary.user_thread_count,
            summary.user_stack_size / 1024,
            summary.user_total / 1024,
        ));
    }
    
    Ok(summary)
}

/// Print stack allocation summary to console
pub fn print_stack_requirements() {
    use crate::console;
    
    let summary = calculate_stack_requirements();
    let heap_required = summary.system_total + summary.user_total;
    
    console::print("=== Stack Memory Requirements ===\n");
    crate::safe_print!(64, "Boot stack (fixed):     {} KB\n", summary.boot_stack / 1024);
    crate::safe_print!(128, "System threads:         {} × {} KB = {} KB\n",
        summary.system_thread_count,
        summary.system_stack_size / 1024,
        summary.system_total / 1024);
    crate::safe_print!(128, "User threads:           {} × {} KB = {} KB\n",
        summary.user_thread_count,
        summary.user_stack_size / 1024,
        summary.user_total / 1024);
    crate::safe_print!(96, "Exception area/thread:  {} KB (for IRQ/syscall handlers)\n",
        summary.exception_stack_size / 1024);
    crate::safe_print!(96, "Usable kernel stack:    {} KB (per thread, for execute() etc.)\n",
        summary.usable_kernel_stack / 1024);
    crate::safe_print!(96, "Total from heap:        {} KB ({} MB)\n",
        heap_required / 1024,
        heap_required / (1024 * 1024));
    crate::safe_print!(96, "Grand total:            {} KB ({} MB)\n",
        summary.total_bytes / 1024,
        summary.total_bytes / (1024 * 1024));
}
