//! Kernel configuration constants
//!
//! This module contains tunable parameters for the kernel.
//! Modify these values to adjust kernel behavior.
//!
//! # Stack Size Warnings
//!
//! Stack sizes may be insufficient for certain workloads:
//! - Deep async call chains (SSH, HTTP) may need larger stacks
//! - Recursive algorithms can overflow smaller stacks
//! - Complex shell commands may need more stack space
//!
//! See `docs/THREAD_STACK_ANALYSIS.md` for detailed analysis and guidance.

#![allow(dead_code)]

/// Boot/kernel stack size (1MB default)
///
/// Used by thread 0 (boot thread) and exception handlers.
/// This stack is placed at a fixed address (0x42000000) in boot.rs.
pub const KERNEL_STACK_SIZE: usize = 1024 * 1024;

/// Default per-thread stack size (32KB)
///
/// Used for kernel threads spawned without a custom stack size.
/// WARNING: May overflow with deep async polling or recursion.
/// Consider using `ASYNC_THREAD_STACK_SIZE` for network-heavy threads.
pub const DEFAULT_THREAD_STACK_SIZE: usize = 32 * 1024;

/// Stack size for networking/async thread (256KB)
///
/// Larger stack to handle deep SSH/HTTP async call chains.
/// Use this for threads that run the async executor or network services.
pub const ASYNC_THREAD_STACK_SIZE: usize = 256 * 1024;

/// User process stack size (64KB default)
///
/// Stack allocated for user-space ELF processes.
/// WARNING: May overflow with deep recursion in user code.
/// A guard page is placed below the stack to detect overflow.
pub const USER_STACK_SIZE: usize = 64 * 1024;

/// Maximum kernel threads
///
/// Total number of thread slots in the thread pool.
/// Thread 0 is reserved for the boot/idle thread.
/// Actual usable threads = MAX_THREADS - 1
pub const MAX_THREADS: usize = 32;

/// Number of kernel threads reserved for system services
///
/// Threads 0 to RESERVED_THREADS-1 are reserved for:
/// - Thread 0: Boot/async main loop
/// - Threads 1-7: Shell, SSH sessions, internal services
///
/// User processes can only spawn on threads RESERVED_THREADS through MAX_THREADS-1.
pub const RESERVED_THREADS: usize = 8;

/// Stack size for reserved system threads (256KB)
///
/// Used for threads 1 through RESERVED_THREADS-1.
/// Larger stacks to handle deep SSH/HTTP async call chains.
pub const SYSTEM_THREAD_STACK_SIZE: usize = 256 * 1024;

/// Stack size for user process threads (64KB)
///
/// Used for threads RESERVED_THREADS through MAX_THREADS-1.
/// Smaller stacks since user processes have their own user-space stack.
pub const USER_THREAD_STACK_SIZE: usize = 64 * 1024;

/// Enable stack canary checking
///
/// When enabled, canary values are written at the bottom of each thread stack
/// and periodically checked to detect stack overflow.
/// Disable for slightly better performance in production.
pub const ENABLE_STACK_CANARIES: bool = true;

/// Stack canary value
///
/// Magic value written at the bottom of each stack.
/// If this value is corrupted, stack overflow has occurred.
pub const STACK_CANARY: u64 = 0xDEAD_BEEF_CAFE_BABE;

/// Number of canary words at stack bottom
///
/// More canary words = better detection but more wasted stack space.
/// 8 words = 64 bytes of canary.
pub const CANARY_WORDS: usize = 8;

/// Fail tests if test binaries are missing
///
/// When enabled, tests that require binaries (elftest, stdcheck, hello, echo2)
/// will fail if the binary is not found on the filesystem.
/// When disabled, these tests will be skipped with a warning.
///
/// Set to `true` for CI/production builds where all binaries should be present.
/// Set to `false` for development when testing without a fully populated disk.
pub const FAIL_TESTS_IF_TEST_BINARY_MISSING: bool = true;

/// Use cooperative main thread
///
/// When enabled, the main thread will be cooperative and will yield to the scheduler.
/// When disabled, the main thread will be preemptive and will not yield to the scheduler.
pub const COOPERATIVE_MAIN_THREAD: bool = false;
