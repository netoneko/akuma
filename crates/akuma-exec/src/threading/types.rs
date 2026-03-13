//! Pure data types for the threading subsystem.
//!
//! These types have no architecture-specific dependencies and can be
//! compiled and tested on the host.

#![allow(dead_code)]

use alloc::string::String;

/// Compile-time constant for static array sizes (must match ExecConfig::max_threads).
pub const MAX_THREADS: usize = 32;

/// Default timeout for cooperative threads in microseconds (100ms)
pub const COOPERATIVE_TIMEOUT_US: u64 = 100_000;

/// Thread 0 is the boot/idle thread - always protected, never terminated
pub const IDLE_THREAD_IDX: usize = 0;

/// Magic value for Context integrity check
pub const CONTEXT_MAGIC: u64 = 0xDEAD_BEEF_1234_5678;

/// Size of per-thread exception stack area (reserved at top of kernel stack)
///
/// This area holds:
/// - sync_el0_handler trap frame (296 bytes) at top
/// - irq_el0_handler frame (272 bytes) at top-768
/// - Function call stack for syscall handlers (console, FS, etc.)
///
/// Syscall handlers can call deep functions. 16KB*2 provides enough
/// headroom to prevent overlap with kernel code below.
pub const EXCEPTION_STACK_SIZE: usize = 16384 * 2;

/// Size of the UNIFIED IRQ frame saved on stack (832 bytes)
///
/// Both EL0 and EL1 IRQ handlers now use this same layout.
/// Layout: 288 bytes GPR (x0-x30, ELR, SPSR, SP_EL0, TPIDR, x10/x11)
///       + 16 bytes padding/alignment (total GPR block = 304)
///       + 528 bytes NEON/FP (Q0-Q31 + FPCR + FPSR)
/// The NEON block sits between the TPIDR push and x10/x11 in the frame.
pub const IRQ_FRAME_SIZE: usize = 304 + 528;

/// Maximum time preemption can be disabled before watchdog warning (100ms)
pub const PREEMPTION_WATCHDOG_WARN_US: u64 = 100_000;

/// Maximum time preemption can be disabled before watchdog panic (5 seconds)
pub const PREEMPTION_WATCHDOG_PANIC_US: u64 = 5_000_000;

/// Maximum expected gap between watchdog checks (100ms).
/// If we see a gap larger than this, the host likely slept.
pub const MAX_EXPECTED_CHECK_GAP_US: u64 = 100_000;

/// Thread state values for lock-free atomic operations
pub mod thread_state {
    pub const FREE: u8 = 0;
    pub const READY: u8 = 1;
    pub const RUNNING: u8 = 2;
    pub const TERMINATED: u8 = 3;
    pub const INITIALIZING: u8 = 4;
    pub const WAITING: u8 = 5;
}

/// User trap frame saved by the EL0 sync/IRQ handler.
#[repr(C)]
pub struct UserTrapFrame {
    pub x0: u64, pub x1: u64, pub x2: u64, pub x3: u64,
    pub x4: u64, pub x5: u64, pub x6: u64, pub x7: u64,
    pub x8: u64, pub x9: u64, pub x10: u64, pub x11: u64,
    pub x12: u64, pub x13: u64, pub x14: u64, pub x15: u64,
    pub x16: u64, pub x17: u64, pub x18: u64, pub x19: u64,
    pub x20: u64, pub x21: u64, pub x22: u64, pub x23: u64,
    pub x24: u64, pub x25: u64, pub x26: u64, pub x27: u64,
    pub x28: u64, pub x29: u64, pub x30: u64,
    pub sp_el0: u64,
    pub elr_el1: u64,
    pub spsr_el1: u64,
    pub tpidr_el0: u64,
    pub _padding: u64,
}

/// CPU context saved during context switch
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Context {
    pub magic: u64,
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
    pub x29: u64,
    pub x30: u64,
    pub sp: u64,
    pub daif: u64,
    pub elr: u64,
    pub spsr: u64,
    pub ttbr0: u64,
    pub user_entry: u64,
    pub user_sp: u64,
    pub user_tls: u64,
    pub is_user_process: u64,
}

impl Context {
    pub const fn zero() -> Self {
        Self {
            magic: CONTEXT_MAGIC,
            x19: 0, x20: 0, x21: 0, x22: 0, x23: 0, x24: 0,
            x25: 0, x26: 0, x27: 0, x28: 0, x29: 0, x30: 0,
            sp: 0, daif: 0, elr: 0, spsr: 0,
            ttbr0: 0,
            user_entry: 0, user_sp: 0, user_tls: 0, is_user_process: 0,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.magic == CONTEXT_MAGIC
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    Free,
    Ready,
    Running,
    Terminated,
}

/// Stack information for a thread
#[derive(Debug, Clone, Copy)]
pub struct StackInfo {
    pub base: usize,
    pub size: usize,
    pub top: usize,
}

impl StackInfo {
    pub const fn empty() -> Self {
        Self { base: 0, size: 0, top: 0 }
    }

    pub fn new(base: usize, size: usize) -> Self {
        Self { base, size, top: base + size }
    }

    pub fn overlaps(&self, other: &StackInfo) -> bool {
        if self.base == 0 || other.base == 0 {
            return false;
        }
        self.base < other.top && other.base < self.top
    }

    pub fn contains(&self, addr: usize) -> bool {
        self.base != 0 && addr >= self.base && addr < self.top
    }

    pub fn is_allocated(&self) -> bool {
        self.base != 0
    }
}

/// Thread slot in the pool
///
/// Thread state is stored in the global THREAD_STATES atomic array,
/// NOT in this struct, for lock-free state checks during scheduling.
#[repr(C)]
pub struct ThreadSlot {
    pub cooperative: bool,
    pub start_time_us: u64,
    pub timeout_us: u64,
    pub exception_stack_top: u64,
    /// Address of a fault handler for user memory access (copy_from/to_user).
    /// If non-zero, an EL1 Data Abort will redirect here instead of killing the process.
    pub user_copy_fault_handler: u64,
}

impl ThreadSlot {
    pub const fn empty() -> Self {
        Self {
            cooperative: false,
            start_time_us: 0,
            timeout_us: 0,
            exception_stack_top: 0,
            user_copy_fault_handler: 0,
        }
    }
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

/// Information about a kernel thread for display
#[derive(Debug, Clone)]
pub struct KernelThreadInfo {
    pub tid: usize,
    pub state: &'static str,
    pub cooperative: bool,
    pub stack_base: usize,
    pub stack_size: usize,
    pub stack_used: usize,
    pub canary_ok: bool,
    pub name: &'static str,
}

/// Snapshot data copied from thread pool (to minimize IRQ-disabled time)
pub(crate) struct ThreadPoolSnapshot {
    pub states: [ThreadState; MAX_THREADS],
    pub cooperative: [bool; MAX_THREADS],
    pub sps: [u64; MAX_THREADS],
    pub stacks: [StackInfo; MAX_THREADS],
}

/// Stack allocation summary for verification
#[derive(Debug, Clone)]
pub struct StackAllocationSummary {
    pub total_bytes: usize,
    pub boot_stack: usize,
    pub system_thread_count: usize,
    pub system_stack_size: usize,
    pub system_total: usize,
    pub user_thread_count: usize,
    pub user_stack_size: usize,
    pub user_total: usize,
    pub exception_stack_size: usize,
    pub usable_kernel_stack: usize,
}

/// Calculate the total stack memory required based on kernel config
pub fn calculate_stack_requirements(
    reserved_threads: usize,
    kernel_stack_size: usize,
    system_thread_stack_size: usize,
    user_thread_stack_size: usize,
) -> StackAllocationSummary {
    let system_thread_count = reserved_threads - 1;
    let user_thread_count = MAX_THREADS - reserved_threads;

    let system_total = system_thread_count * system_thread_stack_size;
    let user_total = user_thread_count * user_thread_stack_size;

    let heap_allocated = system_total + user_total;

    let min_stack = system_thread_stack_size.min(user_thread_stack_size);
    let usable_kernel_stack = min_stack.saturating_sub(EXCEPTION_STACK_SIZE);

    StackAllocationSummary {
        total_bytes: kernel_stack_size + heap_allocated,
        boot_stack: kernel_stack_size,
        system_thread_count,
        system_stack_size: system_thread_stack_size,
        system_total,
        user_thread_count,
        user_stack_size: user_thread_stack_size,
        user_total,
        exception_stack_size: EXCEPTION_STACK_SIZE,
        usable_kernel_stack,
    }
}

/// Verify that stack allocations fit within available heap memory
pub fn verify_stack_memory_params(
    available_heap: usize,
    reserved_threads: usize,
    kernel_stack_size: usize,
    system_thread_stack_size: usize,
    user_thread_stack_size: usize,
) -> Result<StackAllocationSummary, String> {
    let summary = calculate_stack_requirements(
        reserved_threads,
        kernel_stack_size,
        system_thread_stack_size,
        user_thread_stack_size,
    );

    const MIN_USABLE_KERNEL_STACK: usize = 8 * 1024;
    if summary.usable_kernel_stack < MIN_USABLE_KERNEL_STACK {
        return Err(alloc::format!(
            "Exception stack too large! Usable kernel stack: {} KB < {} KB minimum",
            summary.usable_kernel_stack / 1024,
            MIN_USABLE_KERNEL_STACK / 1024,
        ));
    }

    let heap_required = summary.system_total + summary.user_total;

    if heap_required > available_heap {
        return Err(alloc::format!(
            "Stack memory exceeds heap! Required: {} KB, Available: {} KB",
            heap_required / 1024,
            available_heap / 1024,
        ));
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Constants sanity checks ---

    #[test]
    fn constants_sanity() {
        assert_eq!(CONTEXT_MAGIC, 0xDEAD_BEEF_1234_5678, "CONTEXT_MAGIC must match expected value");
        assert_eq!(MAX_THREADS, 32, "MAX_THREADS should be 32");
        assert_eq!(IRQ_FRAME_SIZE, 304 + 528, "IRQ_FRAME_SIZE = 304 + 528");
        assert_eq!(IRQ_FRAME_SIZE, 832, "IRQ_FRAME_SIZE should be 832 bytes");
        assert!(EXCEPTION_STACK_SIZE > 0, "EXCEPTION_STACK_SIZE must be positive");
        assert!(EXCEPTION_STACK_SIZE >= 32 * 1024, "EXCEPTION_STACK_SIZE should be at least 32KB");
    }

    // --- Context::zero() and Context::is_valid() ---

    #[test]
    fn context_zero_sets_magic() {
        let ctx = Context::zero();
        assert_eq!(ctx.magic, CONTEXT_MAGIC, "zero() must set magic to CONTEXT_MAGIC");
    }

    #[test]
    fn context_zero_all_registers_zero() {
        let ctx = Context::zero();
        assert_eq!(ctx.x19, 0);
        assert_eq!(ctx.x20, 0);
        assert_eq!(ctx.x30, 0);
        assert_eq!(ctx.sp, 0);
        assert_eq!(ctx.daif, 0);
        assert_eq!(ctx.elr, 0);
        assert_eq!(ctx.spsr, 0);
        assert_eq!(ctx.ttbr0, 0);
        assert_eq!(ctx.user_entry, 0);
        assert_eq!(ctx.user_sp, 0);
        assert_eq!(ctx.user_tls, 0);
        assert_eq!(ctx.is_user_process, 0);
    }

    #[test]
    fn context_is_valid_with_valid_context() {
        let ctx = Context::zero();
        assert!(ctx.is_valid(), "zero() context should be valid");
    }

    #[test]
    fn context_is_valid_rejects_corrupt_magic() {
        let mut ctx = Context::zero();
        ctx.magic = 0;
        assert!(!ctx.is_valid(), "context with wrong magic should be invalid");
    }

    #[test]
    fn context_is_valid_rejects_partial_write() {
        let mut ctx = Context::zero();
        ctx.magic = CONTEXT_MAGIC.wrapping_add(1);
        assert!(!ctx.is_valid());
    }

    // --- StackInfo ---

    #[test]
    fn stack_info_empty() {
        let s = StackInfo::empty();
        assert_eq!(s.base, 0);
        assert_eq!(s.size, 0);
        assert_eq!(s.top, 0);
        assert!(!s.is_allocated());
    }

    #[test]
    fn stack_info_new() {
        let s = StackInfo::new(0x1000, 0x4000);
        assert_eq!(s.base, 0x1000);
        assert_eq!(s.size, 0x4000);
        assert_eq!(s.top, 0x5000);
        assert!(s.is_allocated());
    }

    #[test]
    fn stack_info_contains() {
        let s = StackInfo::new(0x1000, 0x4000);
        assert!(!s.contains(0x0FFF), "below base");
        assert!(s.contains(0x1000), "at base");
        assert!(s.contains(0x2000), "in middle");
        assert!(s.contains(0x4FFF), "just below top");
        assert!(!s.contains(0x5000), "at top (exclusive)");
        assert!(!s.contains(0x5001), "above top");
    }

    #[test]
    fn stack_info_empty_contains_nothing() {
        let s = StackInfo::empty();
        assert!(!s.contains(0));
        assert!(!s.contains(0x1000));
    }

    #[test]
    fn stack_info_overlaps_disjoint() {
        let a = StackInfo::new(0x1000, 0x1000);
        let b = StackInfo::new(0x3000, 0x1000);
        assert!(!a.overlaps(&b));
        assert!(!b.overlaps(&a));
    }

    #[test]
    fn stack_info_overlaps_adjacent() {
        let a = StackInfo::new(0x1000, 0x1000);
        let b = StackInfo::new(0x2000, 0x1000);
        assert!(!a.overlaps(&b), "adjacent stacks should not overlap");
    }

    #[test]
    fn stack_info_overlaps_intersecting() {
        let a = StackInfo::new(0x1000, 0x2000);
        let b = StackInfo::new(0x2000, 0x2000);
        assert!(a.overlaps(&b));
        assert!(b.overlaps(&a));
    }

    #[test]
    fn stack_info_overlaps_one_inside_other() {
        let outer = StackInfo::new(0x1000, 0x4000);
        let inner = StackInfo::new(0x2000, 0x1000);
        assert!(outer.overlaps(&inner));
        assert!(inner.overlaps(&outer));
    }

    #[test]
    fn stack_info_overlaps_with_empty_always_false() {
        let a = StackInfo::new(0x1000, 0x1000);
        let empty = StackInfo::empty();
        assert!(!a.overlaps(&empty));
        assert!(!empty.overlaps(&a));
    }

    #[test]
    fn stack_info_is_allocated() {
        assert!(!StackInfo::empty().is_allocated());
        assert!(StackInfo::new(1, 0).is_allocated());
        assert!(StackInfo::new(0x1000, 0x1000).is_allocated());
    }

    // --- calculate_stack_requirements ---

    #[test]
    fn calculate_stack_requirements_math() {
        let reserved = 4_usize;
        let kernel_stack = 64 * 1024;
        let system_stack = 32 * 1024;
        let user_stack = 64 * 1024;

        let summary = calculate_stack_requirements(
            reserved,
            kernel_stack,
            system_stack,
            user_stack,
        );

        assert_eq!(summary.system_thread_count, 3, "reserved - 1");
        assert_eq!(summary.user_thread_count, 28, "MAX_THREADS - reserved");
        assert_eq!(summary.system_total, 3 * 32 * 1024);
        assert_eq!(summary.user_total, 28 * 64 * 1024);
        assert_eq!(
            summary.total_bytes,
            kernel_stack + summary.system_total + summary.user_total
        );
        assert_eq!(summary.boot_stack, kernel_stack);
        assert_eq!(summary.exception_stack_size, EXCEPTION_STACK_SIZE);
    }

    #[test]
    fn calculate_stack_requirements_usable_kernel_stack() {
        let system_stack: usize = 64 * 1024;
        let user_stack: usize = 64 * 1024;
        let min_stack = system_stack.min(user_stack);
        let expected_usable = min_stack.saturating_sub(EXCEPTION_STACK_SIZE);

        let summary = calculate_stack_requirements(4, 64 * 1024, system_stack, user_stack);
        assert_eq!(summary.usable_kernel_stack, expected_usable);
    }

    #[test]
    fn calculate_stack_requirements_single_system_thread() {
        let summary = calculate_stack_requirements(2, 32 * 1024, 16 * 1024, 32 * 1024);
        assert_eq!(summary.system_thread_count, 1);
        assert_eq!(summary.user_thread_count, 30);
    }

    // --- verify_stack_memory_params ---

    #[test]
    fn verify_stack_memory_params_ok_when_enough_heap() {
        let heap = 128 * 1024 * 1024;
        let stack = EXCEPTION_STACK_SIZE + 16 * 1024;
        let result = verify_stack_memory_params(heap, 4, 64 * 1024, stack, stack);
        assert!(result.is_ok(), "should succeed with 128MB heap");
    }

    #[test]
    fn verify_stack_memory_params_err_when_heap_too_small() {
        let stack = EXCEPTION_STACK_SIZE + 16 * 1024;
        let summary = calculate_stack_requirements(4, 64 * 1024, stack, stack);
        let heap_required = summary.system_total + summary.user_total;
        let tiny_heap = heap_required / 2;

        let result = verify_stack_memory_params(tiny_heap, 4, 64 * 1024, stack, stack);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("exceeds heap") || err.contains("Stack memory"));
    }

    #[test]
    fn verify_stack_memory_params_err_when_usable_stack_too_small() {
        let system_stack = 16 * 1024;
        let user_stack = 16 * 1024;
        if EXCEPTION_STACK_SIZE >= system_stack {
            let result = verify_stack_memory_params(
                256 * 1024 * 1024,
                4,
                64 * 1024,
                system_stack,
                user_stack,
            );
            assert!(result.is_err(), "usable stack < 8KB should fail");
            let err = result.unwrap_err();
            assert!(err.contains("Exception stack") || err.contains("Usable"));
        }
    }
}
