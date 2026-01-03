# Thread Stack Analysis

This document analyzes stack usage in the kernel's threading system and identifies potential stack overflow risks.

## Thread Types and Stack Sizes

| Thread | Stack Size | Location | Notes |
|--------|-----------|----------|-------|
| Thread 0 (boot) | 1 MB | `0x40000000-0x40100000` | **BROKEN** - See `docs/BOOT_STACK_BUG.md` |
| Threads 1-31 | 32 KB each | Heap-allocated | Pre-allocated at init |

### Boot Thread (Thread 0)

```rust
// In src/threading.rs
self.stacks[IDLE_THREAD_IDX] = 0; // Boot stack, don't allocate
```

Thread 0 uses the boot stack set up in `boot.rs`. This stack is currently **placed inside the kernel code** due to the hardcoded 1MB offset bug.

### Spawned Threads (1-31)

```rust
// In src/threading.rs
const STACK_SIZE: usize = 32 * 1024;  // 32KB
const MAX_THREADS: usize = 32;

// Pre-allocate stacks for all other slots using Vec
for i in 1..MAX_THREADS {
    let stack_vec: Vec<u8> = alloc::vec![0u8; STACK_SIZE];
    let stack_ptr = Box::into_raw(stack_vec.into_boxed_slice()) as *mut u8;
    self.stacks[i] = stack_ptr as usize;
}
```

Spawned threads get 32KB stacks from the heap. These are safe from the kernel code corruption issue but could overflow with deep call stacks.

## Async Execution Analysis

The kernel uses async/await extensively. Understanding where async code runs is critical for stack analysis.

### Where Async Code Runs

The main async polling loop runs on **thread 0** (boot thread):

```rust
// In src/main.rs run_async_main()
loop {
    // Poll the main network runner
    let _ = runner_pinned.as_mut().poll(&mut cx);
    
    // Poll the SSH server
    let _ = ssh_pinned.as_mut().poll(&mut cx);
    
    // Poll the HTTP web servers
    let _ = web_pinned.as_mut().poll(&mut cx);
    
    // ...more futures...
}
```

### Stack Usage in Async

**Future state machines** are stored on the heap (via `Box::pin()` or `pin!` macro):
- The async state (local variables across await points) lives in heap memory
- This is good - it doesn't consume stack per-task

**Poll call chains** use the current thread's stack:
- Each `poll()` call creates a call chain
- Deeply nested async functions = deep call chains during poll
- This DOES consume stack

### Async Complexity Analysis

| Component | async fn + .await count | Risk |
|-----------|------------------------|------|
| SSH Protocol (`ssh/protocol.rs`) | 101 | High |
| Shell Commands (`shell/`) | 214 | High |
| Web Server (`web_server.rs`) | ~20 | Medium |
| Async Net (`async_net.rs`) | ~40 | Medium |

The SSH and shell code have very high async complexity, meaning deep call chains during polling.

## Stack Overflow Scenarios

### Scenario 1: Boot Thread Async Polling (HIGH RISK)

```
run_async_main()
  └─ ssh_pinned.poll()
       └─ ssh::run().poll()
            └─ connection.future.poll()
                 └─ handle_session().poll()
                      └─ process_channel_request().poll()
                           └─ run_shell_command().poll()
                                └─ shell::execute().poll()
                                     └─ command_handler().poll()
                                          └─ fs::async_read().poll()
                                               └─ ... more nesting ...
```

Each level adds a stack frame. With 101+ async points in SSH alone, this can easily exceed available stack.

**Current status**: Thread 0 uses the broken boot stack placed inside kernel code. Any significant stack usage corrupts kernel code.

### Scenario 2: Spawned Thread Deep Calls (MEDIUM RISK)

Spawned threads have 32KB stacks. Risk factors:
- Large local variables (arrays, buffers)
- Deep recursion
- Many nested function calls
- Closure captures

32KB is generally sufficient for typical operations but could overflow with:
- Deeply nested shell command execution
- Large stack-allocated buffers
- Complex async operations running on spawned threads

### Scenario 3: Exception Handling (LOW-MEDIUM RISK)

Exception handlers push context to the stack:

```asm
// In src/exceptions.rs
sync_el0_handler:
    sub     sp, sp, #280            // 35 * 8 bytes for user context
```

Each syscall or exception uses 280+ bytes of stack. Nested exceptions or syscalls during interrupt handling compound this.

## No Guard Pages

Currently, there are no guard pages between:
- Stack and kernel code (boot stack)
- Stack and heap (spawned thread stacks)
- Adjacent thread stacks

Stack overflow results in **silent memory corruption**, not a fault.

## Recommendations

### Immediate Fixes

1. **Fix boot stack placement** (see `docs/BOOT_STACK_BUG.md`)
   - Move stack to after `_kernel_phys_end`
   - Or use a fixed high address like `0x42000000`

2. **Consider increasing thread stack size**
   ```rust
   const STACK_SIZE: usize = 64 * 1024;  // 64KB instead of 32KB
   ```
   Trade-off: Fewer threads or more heap usage

### Future Improvements

1. **Add stack guard pages**
   - Unmap one page at bottom of each stack
   - Overflow triggers page fault instead of corruption

2. **Stack usage monitoring**
   - Fill stacks with canary pattern at allocation
   - Periodically check canary integrity
   - Add stack high-water-mark tracking

3. **Move async polling to dedicated thread**
   - Don't run complex async code on boot thread
   - Spawn a thread specifically for async execution with known-good stack

4. **Reduce stack pressure in hot paths**
   - Use heap allocation for large temporary buffers
   - Avoid deep recursion
   - Box large futures to move state to heap

## Testing Stack Usage

To estimate actual stack usage, you could add instrumentation:

```rust
// At thread start
fn fill_stack_with_canary(stack_base: usize, size: usize) {
    unsafe {
        let ptr = stack_base as *mut u64;
        for i in 0..(size / 8) {
            ptr.add(i).write_volatile(0xDEADBEEF_CAFEBABE);
        }
    }
}

// To check high-water mark
fn check_stack_usage(stack_base: usize, size: usize) -> usize {
    unsafe {
        let ptr = stack_base as *const u64;
        for i in 0..(size / 8) {
            if ptr.add(i).read_volatile() != 0xDEADBEEF_CAFEBABE {
                return size - (i * 8);  // Used bytes
            }
        }
        0
    }
}
```

## Related Documentation

- `docs/BOOT_STACK_BUG.md` - Critical bug with boot stack placement
- `docs/MEMORY_LAYOUT.md` - Overall memory layout
- `src/threading.rs` - Thread pool implementation
- `src/executor.rs` - Embassy async executor

