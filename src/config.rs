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

/// Stack size for networking/async thread (512KB)
///
/// Larger stack to handle deep SSH/HTTP async call chains.
/// Use this for threads that run the async executor or network services.
/// Note: Increased from 256KB due to stack exhaustion during long-running sessions.
pub const ASYNC_THREAD_STACK_SIZE: usize = 512 * 1024;

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

/// Stack size for reserved system threads (512KB)
///
/// Used for threads 1 through RESERVED_THREADS-1.
/// Larger stacks to handle deep SSH/HTTP async call chains.
/// Note: The async main thread (when COOPERATIVE_MAIN_THREAD=false) runs on
/// a system thread and needs significant stack space for pinned futures.
pub const SYSTEM_THREAD_STACK_SIZE: usize = 512 * 1024;

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
pub const ENABLE_STACK_CANARIES: bool = true; // enabled for debugging stack corruption

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
/// When enabled, the main thread (thread 0) runs the async loop directly on the
/// 1MB boot stack. When disabled, it runs on a system thread with 512KB stack.
///
/// RECOMMENDATION: Set to `true` if experiencing stack exhaustion issues.
/// The async main loop pins 6 complex futures (SSH, HTTP, network) which can
/// require significant stack space for deep async call chains.
///
/// Both modes are now safe due to preemption control around embassy-net polling.
/// The polling loop uses disable_preemption()/enable_preemption() to protect
/// embassy-net's internal RefCells from re-entrant access during timer preemption.
pub const COOPERATIVE_MAIN_THREAD: bool = false;

pub const MAIN_THREAD_PRIORITY_BOOST: bool = false; // boost the priority of the main thread to 100

pub const IGNORE_THREADING_TESTS: bool = false;

/// Disable all tests at boot
///
/// When enabled, skips memory tests, threading tests, filesystem tests,
/// process tests, and shell tests. Use this to debug crashes that might
/// be caused by test-induced heap corruption or thread scheduling issues.
pub const DISABLE_ALL_TESTS: bool = false;

/// Minimal idle loop (for debugging EC=0xe crashes)
///
/// When enabled, the idle loop does nothing but yield. No cleanup, no stats,
/// no prints. Use this to isolate whether the crash is caused by something
/// in the cleanup or stats code vs the core timer/ERET path.
pub const MINIMAL_IDLE_LOOP: bool = false;

/// Skip async network initialization (for debugging crashes)
///
/// When enabled, skips the async network stack and services (SSH, HTTP, etc.).
/// Use this to isolate whether crashes are caused by network code.
pub const SKIP_ASYNC_NETWORK: bool = false;

/// Skip filesystem initialization (for debugging crashes)
///
/// When enabled, skips block device and filesystem init.
/// Use this to isolate whether crashes are caused by fs code.
pub const SKIP_FILESYSTEM_INIT: bool = false;

pub const MEM_MONITOR_PERIOD_SECONDS: u64 = 3;
pub const MEM_MONITOR_ENABLED: bool = false;

/// Enable preemption watchdog
///
/// When enabled, the timer IRQ handler checks if any thread has held
/// preemption disabled for too long and logs a warning.
/// Disable to rule out watchdog as a source of issues.
pub const ENABLE_PREEMPTION_WATCHDOG: bool = true;


/// Enable async process execution with streaming output over SSH
///
/// When enabled, external binaries stream output in real-time to the SSH client
/// instead of buffering all output until command completion. This provides
/// better user experience for long-running commands.
///
/// The streaming implementation uses proper yielding to allow the network runner
/// to transmit packets while the process is running.
pub const ENABLE_SSH_ASYNC_EXEC: bool = true;

// ============================================================================
// Network TX Queue Configuration
// ============================================================================

/// Enable TX packet queueing when virtio lock is contended
///
/// When the main network loop can't acquire the virtio lock (held by an SSH
/// session thread), packets would normally be dropped. With this enabled,
/// packets are copied to a pending queue and sent on the next successful
/// lock acquisition.
///
/// This prevents packet loss during lock contention but uses additional memory.
pub const ENABLE_TX_QUEUE: bool = true;

/// Number of pending TX packet slots
///
/// Maximum number of packets that can be queued when the virtio lock is busy.
/// Each slot uses TX_PACKET_BUFFER_SIZE bytes of static memory.
/// Total memory usage: TX_QUEUE_SLOTS * TX_PACKET_BUFFER_SIZE bytes
pub const TX_QUEUE_SLOTS: usize = 8;

/// Size of each TX packet buffer in bytes
///
/// Must be large enough to hold the largest Ethernet frame (1514 bytes)
/// plus any virtio headers. 2048 is a safe default that matches virtio
/// buffer sizes.
pub const TX_PACKET_BUFFER_SIZE: usize = 2048;

// Debug prints
// WARNING: SGI debug prints use alloc::format! which can deadlock if the
// allocator lock is held when timer fires. Keep disabled unless debugging.
pub const ENABLE_SGI_DEBUG_PRINTS: bool = false;
pub const ENABLE_IRQ_DEBUG_PRINTS: bool = false;

// Timer interval in microseconds
pub const TIMER_INTERVAL_US: u64 = 10_000;

/// Deferred thread cleanup mode
///
/// When enabled, cleanup_terminated() becomes a no-op except when called from
/// thread 0 (main/boot thread). This serializes all cleanup to a single point,
/// avoiding potential races between cleanup and spawn operations.
///
/// Enable this to debug thread slot synchronization issues.
pub const DEFERRED_THREAD_CLEANUP: bool = true;

/// Minimum time (microseconds) a thread must be TERMINATED before cleanup
///
/// This adds a "cooldown" period after termination to ensure exception handlers
/// and context switches have fully completed before the slot is recycled.
/// Only applies when DEFERRED_THREAD_CLEANUP is enabled.
///
/// 10ms is enough for context switches to complete while not blocking tests.
pub const THREAD_CLEANUP_COOLDOWN_US: u64 = 10_000; // 10ms