# Preemptive Multitasking in Akuma

This document describes the thread-per-session architecture that enables true preemptive multitasking for SSH sessions and user processes.

## Overview

Akuma implements preemptive multitasking using a fixed-size thread pool with timer-based context switching. The architecture separates threads into three categories:

| Thread Range | Purpose | Stack Size | Preemptible |
|-------------|---------|------------|-------------|
| 0 | Boot/Async executor | 1MB | Cooperative (with timeout) |
| 1-7 | SSH sessions | 256KB | Yes |
| 8-31 | User processes | 64KB | Yes |

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Thread 0                                 │
│                    (Boot/Async Loop)                            │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
│  │   Accept    │  │   Network   │  │   Embassy Time          │  │
│  │ Connections │  │   Runner    │  │   Driver                │  │
│  └──────┬──────┘  └─────────────┘  └─────────────────────────┘  │
│         │                                                        │
└─────────┼────────────────────────────────────────────────────────┘
          │ spawn_system_thread_fn()
          ▼
┌─────────────────────────────────────────────────────────────────┐
│              Session Threads (1-7)                               │
│  ┌──────────────┐ ┌──────────────┐ ┌──────────────┐             │
│  │  Thread 1    │ │  Thread 2    │ │  Thread 3    │  ...        │
│  │  SSH Sess A  │ │  SSH Sess B  │ │  SSH Sess C  │             │
│  │  block_on()  │ │  block_on()  │ │  block_on()  │             │
│  └──────┬───────┘ └──────────────┘ └──────────────┘             │
│         │                                                        │
└─────────┼────────────────────────────────────────────────────────┘
          │ spawn_user_thread_fn()
          ▼
┌─────────────────────────────────────────────────────────────────┐
│              User Process Threads (8-31)                         │
│  ┌──────────────┐ ┌──────────────┐ ┌──────────────┐             │
│  │  Thread 8    │ │  Thread 9    │ │  Thread 10   │  ...        │
│  │  /bin/hello  │ │  /bin/echo2  │ │  (free)      │             │
│  └──────────────┘ └──────────────┘ └──────────────┘             │
└─────────────────────────────────────────────────────────────────┘
          ▲
          │ Timer IRQ (10ms) triggers SGI_SCHEDULER
          │
┌─────────────────────────────────────────────────────────────────┐
│                    Preemptive Scheduler                          │
│  - Round-robin scheduling                                        │
│  - Timer-based preemption for non-cooperative threads           │
│  - Cooperative threads get time-slice protection                │
└─────────────────────────────────────────────────────────────────┘
```

## Thread Categories

### Thread 0: Boot/Async Executor

Thread 0 runs the main async executor that:
- Polls the network runner (handles virtio-net TX/RX)
- Accepts new SSH connections
- Manages embassy timers

This thread is marked as **cooperative** with a 5-second timeout. This means:
- It won't be preempted during short I/O operations
- After 5 seconds, it will be preempted to allow other threads to run
- It voluntarily yields via `yield_now()` in the async executor

### Threads 1-7: SSH Session Threads

Each SSH connection gets its own dedicated thread from this pool. Benefits:
- Sessions run independently and concurrently
- One session blocking doesn't affect others
- Commands execute without blocking the accept loop

Session threads are **non-cooperative** (always preemptible). They use `block_on()` to run async code:

```rust
fn run_session_on_thread(stream: SendableTcpStream, session_id: usize, buffer_slot: usize) -> ! {
    // Run the async connection handler using blocking executor
    block_on(async {
        protocol::handle_connection(stream).await;
    });
    
    // Mark thread as terminated when done
    crate::threading::mark_current_terminated();
    loop { crate::threading::yield_now(); }
}
```

### Threads 8-31: User Process Threads

User processes (ELF binaries) run on these threads. Each process:
- Has its own address space (TTBR0)
- Runs in EL0 (user mode)
- Is preemptible via timer interrupt

## Preemption Mechanism

### Timer Interrupt Flow

1. **Timer fires** (every 10ms via ARM generic timer)
2. **Timer handler** triggers SGI_SCHEDULER
3. **SGI handler** calls `sgi_scheduler_handler()`
4. **Scheduler** decides whether to switch threads:
   - Cooperative thread (thread 0): Only switch if timeout elapsed
   - Non-cooperative thread: Always switch to next ready thread
5. **Context switch** saves/restores CPU registers

### Cooperative vs Non-Cooperative

```rust
pub fn schedule_indices(&mut self, voluntary: bool) -> Option<(usize, usize)> {
    // For timer-triggered preemption of cooperative threads,
    // check if the time-slice has expired
    if !voluntary && current.cooperative && current.state == ThreadState::Running {
        if elapsed < timeout {
            return None; // Don't preempt yet
        }
    }
    // Non-cooperative threads: always proceed to scheduling
    // ...
}
```

## SSH Session Lifecycle

1. **Accept**: Thread 0 accepts connection via embassy-net
2. **Spawn**: System thread (1-7) is spawned for the session
3. **Handshake**: Session thread runs SSH protocol (key exchange, auth)
4. **Shell**: Interactive shell runs on session thread
5. **Command**: When user runs a command:
   - If it's a builtin: Executes directly on session thread
   - If it's a binary: Spawns on user thread (8-31)
6. **Exit**: Session thread is marked terminated and cleaned up

## Process Execution

When a user runs a binary (e.g., `hello`):

1. **spawn_process_with_channel()** creates a ProcessChannel
2. **spawn_user_thread_fn()** allocates thread 8-31
3. Process executes in EL0 with its own address space
4. Syscalls are handled by the kernel
5. Output is captured via ProcessChannel
6. On exit, thread is marked terminated

## Ctrl+C / Interrupt Handling

The interrupt mechanism allows processes to be signaled:

1. **ProcessChannel** has an `interrupted` flag
2. **SSH shell** can call `channel.set_interrupted()` when Ctrl+C is detected
3. **Syscall handler** checks `is_current_interrupted()` before each syscall
4. **sys_nanosleep** checks for interrupts every 10ms during sleep
5. Interrupted processes exit with code 130 (128 + SIGINT)

```rust
// In syscall handler
if crate::process::is_current_interrupted() {
    proc.exited = true;
    proc.exit_code = 130;
    return EINTR;
}
```

## Testing Parallel Execution

The `test_parallel_processes()` test verifies concurrent execution:

1. Spawns two `/bin/hello` processes simultaneously
2. Checks both appear in process table concurrently
3. Waits for both to complete
4. Verifies overlapping execution times

Run via the `test` shell command to validate preemptive scheduling.

## Configuration

Key constants in `src/config.rs`:

```rust
pub const MAX_THREADS: usize = 32;           // Total thread slots
pub const RESERVED_THREADS: usize = 8;       // System threads (0-7)
pub const SYSTEM_THREAD_STACK_SIZE: usize = 256 * 1024;  // 256KB
pub const USER_THREAD_STACK_SIZE: usize = 64 * 1024;     // 64KB
pub const COOPERATIVE_TIMEOUT_US: u64 = 5_000_000;       // 5 seconds
```

## API Reference

### Thread Spawning

```rust
// Spawn a system thread (SSH sessions)
threading::spawn_system_thread_fn(|| { ... }) -> Result<usize, &'static str>

// Spawn a user process thread
threading::spawn_user_thread_fn(|| { ... }) -> Result<usize, &'static str>

// Check available slots
threading::system_threads_available() -> usize
threading::user_threads_available() -> usize
```

### Process Management

```rust
// Spawn process with I/O channel
process::spawn_process_with_channel(path, stdin) -> Result<(usize, Arc<ProcessChannel>), String>

// Async execution (for shell)
process::exec_async(path, stdin).await -> Result<(i32, Vec<u8>), String>

// Interrupt a running process
process::interrupt_thread(thread_id)
```

## Limitations

1. **Socket Sharing**: TcpSocket is wrapped in SendableTcpStream for cross-thread use. This relies on embassy-net's internal synchronization.

2. **Fallback Mode**: If no system threads are available, SSH sessions fall back to async polling on thread 0.

3. **Interrupt Delivery**: Ctrl+C interrupts are only checked between syscalls, not during pure computation.

4. **Thread Pool Size**: Fixed at compile time (32 threads max).

