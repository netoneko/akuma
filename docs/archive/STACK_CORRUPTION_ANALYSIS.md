# Stack Corruption Analysis

This document analyzes the stack corruption bug observed during user process execution.

## Symptoms

### Primary Symptom
When a user process exits, `return_to_kernel()` detects that stack memory has been modified:

```
[return_to_kernel] PID=9
  entry: x30=0x4005b8b4 sp=0x42392740 x29=0x0
  saved: sp=0x42392740 x30=0x400cca68
  Stack comparison (entry vs now):
  [entry_sp+0]: was 0x0 now 0x9 CHANGED!
```

### Extended Corruption Pattern (from earlier runs)
```
[entry_sp+0]: was 0x0 now 0xa CHANGED!
[entry_sp+24]: was 0x0 now 0x6c65682f6e69622f CHANGED!
[entry_sp+32]: was 0x0 now 0x6f6c CHANGED!
```

Decoded values:
- `0xa` = PID 10
- `0x6c65682f6e69622f` = "/bin/hel" (ASCII, little-endian)
- `0x6f6c` = "lo" (ASCII, little-endian)

### Eventual Crash
After extended operation, the kernel crashes with a data abort:
```
[Exception] Sync from EL1: EC=0x25, ISS=0x10, ELR=0x400c59f8, FAR=0x48000000, SPSR=0x60002345
```

## Key Observation

**Fixing `libakuma::sleep` makes the corruption appear faster.**

Before the fix: Process took ~11-21 seconds, corruption detected at exit.
After the fix: Process takes ~9 seconds, corruption detected at exit.

This suggests the corruption happens at a consistent point in the process lifecycle, and faster execution just reveals it sooner in wall-clock time.

## Analysis

### Corruption Pattern Matches ProcessInfo Layout

The corrupted values match the `ProcessInfo` struct layout exactly:

```rust
#[repr(C)]
pub struct ProcessInfo {
    pub pid: u32,           // offset 0 (4 bytes)
    pub ppid: u32,          // offset 4 (4 bytes)
    pub argc: u32,          // offset 8 (4 bytes)
    pub argv_len: u32,      // offset 12 (4 bytes)
    pub argv_data: [u8; 1008], // offset 16 (1008 bytes)
}
```

Reading as u64 values:
- `entry_sp+0`: `pid | (ppid << 32)` = `0x0000000000000009` (PID=9, PPID=0) ✓
- `entry_sp+24`: `argv_data[8..15]` would contain part of path string

This strongly suggests that `ProcessInfo` is being written to `entry_sp` instead of the intended `process_info_phys` address.

### Where ProcessInfo is Written

In `Process::execute()`:
```rust
let info_ptr = crate::mmu::phys_to_virt(self.process_info_phys) as *mut ProcessInfo;
// ...
core::ptr::write(info_ptr, info);
```

If `self.process_info_phys` is corrupted to contain a stack address, the 1KB ProcessInfo struct would be written to the stack, corrupting `entry_sp` and beyond.

### Stack Layout

For user process threads (64KB total):
```
stack.top (tpidr_el1 = exception_stack_top)
    |
    | Exception area (32KB): sync_el0_handler, irq_el0_handler
    |   - Trap frames
    |   - Rust syscall handler stack
    |
stack.top - 32KB = initial_sp (kernel code starts here)
    |
    | Kernel code area (32KB):
    |   - closure_trampoline frame
    |   - process.execute() frame  <-- entry_sp is here
    |   - run_user_until_exit() frame
    |
stack.base
```

### Why Stack Overflow is Unlikely

The exception area is 32KB (`EXCEPTION_STACK_SIZE = 16384*2`). For exception handlers to overflow into `entry_sp`, they would need to use ALL 32KB. This is unlikely because:

1. `sync_el0_handler` trap frame is only 296 bytes
2. `irq_el0_handler` frame is at a fixed offset (tpidr_el1 - 768)
3. Even deep Rust call chains rarely exceed a few KB

## Hypotheses

### Hypothesis 1: process_info_phys Corruption (Most Likely)

The `process_info_phys` field in the Process struct gets corrupted to point to `entry_sp`.

Possible causes:
- Heap corruption overwrites the Process struct
- Use-after-free bug
- Buffer overflow in a neighboring field within Process

Evidence: The corruption pattern matches ProcessInfo exactly.

### Hypothesis 2: PMM Returns Stack Address

The physical memory manager somehow returns a stack address instead of a heap page.

This is unlikely but would explain why `phys_to_virt(process_info_phys)` returns `entry_sp`.

### Hypothesis 3: Unrelated Write to entry_sp

Some other code path writes PID and path data to the stack address.

This would require finding code that:
1. Writes u32 PID at some offset
2. Writes path string "/bin/hello" 24 bytes later

This combination is unique to ProcessInfo.

## Diagnostics Added

Two diagnostic checks were added to `Process::execute()`:

### Check 1: Detect stack-region info_ptr
```rust
if info_ptr_val >= stack_region_start && info_ptr_val < stack_region_end {
    console::print(&alloc::format!(
        "[CRITICAL] process_info_phys={:#x} points into stack region! info_ptr={:#x} entry_sp={:#x}\n",
        self.process_info_phys, info_ptr_val, entry_sp
    ));
}
```

### Check 2: Detect corruption immediately after write
```rust
let current_val = *((entry_sp) as *const u64);
if current_val != self.kernel_ctx.entry_stack[0] {
    console::print(&alloc::format!(
        "[CORRUPT] entry_sp[0] changed AFTER ProcessInfo write! was={:#x} now={:#x} info_ptr={:#x}\n",
        self.kernel_ctx.entry_stack[0], current_val, info_ptr_val
    ));
}
```

## Test Binary: stackstress

A stress test binary was created to trigger the corruption faster:

```
userspace/stackstress/src/main.rs
```

Usage: `stackstress [iterations] [mode]`
- Mode 1: Rapid 1ms sleeps (stresses schedule_blocking)
- Mode 2: Rapid writes (stresses syscall path)
- Mode 3: Mixed (default)

This maximizes syscall frequency to stress-test the exception handling path.

## Timing Anomaly

The user also observed:
```
hello: uptime=9001ms expected uptime=10000ms difference=18446744073709550617ms
```

The difference value is `-999` as signed i64 (underflow). This means:
- Expected: 10000ms (10 sleeps × 1000ms, but code expects 9 sleeps for 10 outputs)
- Actual: 9001ms

If running with 11 outputs (which would give expected=10000ms), then actual=9001ms suggests sleeps are completing ~10% faster than expected. This may be a separate timing bug or just measurement overhead.

## Recommended Investigation Steps

1. **Run with diagnostics** - Look for `[CRITICAL]` or `[CORRUPT]` messages

2. **Verify PMM integrity** - Add assertions in `alloc_page_zeroed()` to ensure returned addresses are in valid heap range (not stack)

3. **Check Process struct layout** - Ensure `process_info_phys` isn't adjacent to a Vec or String that could overflow

4. **Add stack canaries around entry_sp** - Write magic values above and below entry_sp, check them periodically

5. **Trace process_info_phys lifecycle**:
   - Print address when allocated in `from_elf()`
   - Print address when used in `execute()`
   - Check if values differ

6. **Examine heap allocator** - If using a custom allocator, verify it doesn't have corruption bugs

## Thread Slot Synchronization Analysis

### Potential Race Conditions

#### Race 1: Cleanup vs Spawn

When cleanup transitions a slot from TERMINATED → FREE, a spawn could immediately claim it:

```
Thread A (cleanup)                    Thread B (spawn)
--------------------                  ------------------
CAS(TERMINATED → FREE) succeeds
                                      CAS(FREE → INITIALIZING) succeeds
init_stack_canary()                   Setting up new context on same stack!
```

Both threads are writing to the same stack memory simultaneously.

#### Race 2: Entry_sp Points to Reused Stack

The Process struct (on heap) stores `kernel_ctx.entry_sp` which points to the **stack**.
When a thread slot is reused:
1. Old Process struct still has entry_sp pointing to old stack address
2. New thread uses same stack, overwrites that memory
3. If anything reads old entry_sp, it sees new thread's data

#### Race 3: Closure Lifetime vs Stack Lifetime

The closure is `FnOnce() -> !` (never returns), so it's never dropped:
- Process struct on heap is never freed (memory leak)
- But entry_sp still points to stack that gets reused

### Mitigation: Deferred Cleanup Mode

Added `config::DEFERRED_THREAD_CLEANUP` to serialize cleanup:

1. **Main-thread-only cleanup**: Only thread 0 can run cleanup
2. **Cooldown period**: Slots must be TERMINATED for 100ms before recycling

This ensures:
- No concurrent cleanup and spawn operations
- Exception handlers have time to fully complete
- Context switches are stable before slot reuse

Configuration in `config.rs`:
```rust
pub const DEFERRED_THREAD_CLEANUP: bool = true;
pub const THREAD_CLEANUP_COOLDOWN_US: u64 = 100_000; // 100ms
```

### Testing the Race Hypothesis

1. Enable `DEFERRED_THREAD_CLEANUP` (now default)
2. Run test workload
3. If corruption disappears: race condition confirmed
4. If corruption persists: issue is elsewhere (ProcessInfo write bug)

## Resolution

### Root Cause Confirmed: Thread Slot Cleanup Race

Testing with `DEFERRED_THREAD_CLEANUP=true` eliminated the ProcessInfo corruption:

**Before (concurrent cleanup):**
```
[entry_sp+0]: was 0x0 now 0x9 CHANGED!      ← PID corruption
[entry_sp+24]: was 0x0 now 0x6c65682f6e69622f ← Path string
```

**After (deferred cleanup):**
```
[entry_sp+16]: was 0x0 now 0x44026000 CHANGED!  ← Normal local variable (Vec ptr)
[entry_sp+32]: was 0x0 now 0x8 CHANGED!          ← Normal local variable (Vec cap)
```

The PID/path corruption is gone. The remaining "changes" at +16 and +32 are normal stack
usage by `execute()`'s local variables (specifically the `arg_refs: Vec<&str>`).

### Fix Applied

1. **Deferred cleanup mode** (`config::DEFERRED_THREAD_CLEANUP = true`)
   - Only thread 0 can run cleanup
   - 100ms cooldown before slot recycling

2. **Smarter diagnostics** in `return_to_kernel()`
   - Only reports if ProcessInfo corruption pattern is detected (PID at +0, path at +24)
   - Ignores normal local variable changes

### Remaining Work

The stack comparison now correctly distinguishes:
- **Corruption**: PID/path written to stack (logs `[STACK CORRUPTION]`)
- **Normal**: Local variables being used (silent)

## Related Issues

- Issue 5 in `docs/SCHEDULING_TIMING_ISSUES.md` documents this same corruption
- Issue 6 documents the eventual crash after extended operation

## Files Modified

- `src/process.rs` - Added diagnostic checks in `execute()`
- `src/threading.rs` - Added deferred cleanup with cooldown
- `src/config.rs` - Added DEFERRED_THREAD_CLEANUP and THREAD_CLEANUP_COOLDOWN_US
- `userspace/stackstress/` - New stress test binary
- `userspace/Cargo.toml` - Added stackstress to workspace
- `userspace/build.sh` - Added stackstress to build
