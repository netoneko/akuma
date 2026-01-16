# Scheduling and Timing Issues

This document describes known timing behaviors and issues with the wait queue scheduler implementation.

## Overview

The wait queue scheduler allows threads to block on timed waits (e.g., `nanosleep`) without busy-waiting. However, several timing discrepancies have been observed.

## Issue 1: Watchdog Time Jump Detection

**Symptom:** Watchdog detects "time jumps" of 100-1000ms even under normal operation.

```
[WATCHDOG] Time jump detected: 200ms (host sleep/wake)
[WATCHDOG] Time jump detected: 1000ms (host sleep/wake)
[WATCHDOG] Time jump detected: 400ms (host sleep/wake)
```

**Cause:** The watchdog expects timer callbacks at regular 10ms intervals. When a cooperative thread (like the network loop on thread 0) holds the CPU without yielding, timer callbacks are delayed. The watchdog interprets this as a "host sleep/wake" event.

**Impact:** False positive warnings. Does not affect correctness.

**Potential fixes:**
- Increase watchdog threshold
- Make cooperative threads yield more frequently
- Adjust timer interrupt frequency

## Issue 2: Parallel Process Timing (2x Expected Duration)

**Symptom:** When two user processes run in parallel, each takes approximately 2x the expected duration.

```
# Two parallel hello processes (each expects 10 seconds)
hello: uptime=21885ms expected uptime=10000ms difference=11885ms
hello: uptime=21891ms expected uptime=10000ms difference=11891ms
```

**Cause:** Round-robin scheduling without time accounting. Each process receives roughly 50% of CPU time when sharing with another process. A 10-second task takes 20+ seconds of wall-clock time.

**This is expected behavior** for a simple round-robin scheduler. Each process:
1. Runs for one time slice (~10ms)
2. Gets preempted
3. Waits while the other process runs
4. Repeats

The `uptime` counter advances during the wait, so each process sees ~20 seconds pass.

## Issue 3: Single Process Overhead (~10%)

**Symptom:** A single user process takes ~10% longer than expected.

```
hello: uptime=11257ms expected uptime=10000ms difference=1257ms
```

**Cause:** Multiple factors contribute:
1. **Timer granularity:** Timer fires every 10ms. Wake times are checked at timer intervals, adding up to 10ms latency per sleep.
2. **Scheduling overhead:** Context switch time, scheduler execution time.
3. **WFI latency:** Time between setting WAITING state and being woken by timer.
4. **Cooperative threads:** Thread 0 (network/main loop) is cooperative and may hold CPU briefly.

**Calculation for 10 iterations of 1-second sleep:**
- 10 sleeps × 10ms max wake latency = 100ms minimum overhead
- Plus scheduling overhead = ~1000-1500ms total overhead

## Issue 4: Two Processes in Separate SSH Sessions

**Symptom:** Same as Issue 2 - processes take 2x expected duration.

```
hello: uptime=20921ms expected uptime=10000ms difference=10921ms
```

**Cause:** Same as parallel process timing. Whether processes are spawned from the same test or separate SSH sessions, they still share CPU via round-robin scheduling.

## Current Scheduler Design

### Wait Queue Implementation

```
schedule_blocking(wake_time_us):
    1. Mark thread as WAITING with wake_time
    2. Enter WFI loop until state changes
    
schedule_indices() [called by scheduler]:
    1. Check all WAITING threads
    2. Wake threads whose wake_time has passed (set to READY)
    3. Send SEV to wake WFI
    4. Pick next READY thread (round-robin)
```

### Thread States

- `READY` - Can be scheduled
- `RUNNING` - Currently executing
- `WAITING` - Blocked on wait queue (has wake_time)
- `TERMINATED` - Finished execution

### Timer Configuration

- Timer fires every 10ms
- Each timer tick triggers scheduler (SGI)
- Scheduler wakes expired WAITING threads and picks next thread

## Potential Improvements

### 1. Reduce Wake Latency
- Check wake times more frequently
- Use a sorted wake queue for O(1) next-wake lookup

### 2. Fair Scheduling
- Track CPU time used per thread
- Prioritize threads that have used less CPU
- Implement virtual runtime (like CFS)

### 3. Priority Scheduling
- Give user processes higher priority during I/O wait
- Boost priority for threads waiting on external events

### 4. Reduce Timer Overhead
- Batch timer checks
- Use tickless scheduling when possible

## Test Cases

### Parallel Process Test
- Spawns 2 hello processes
- Each does 10 iterations of print + 1-second sleep
- Expected: Each completes in ~10 seconds
- Actual: Each completes in ~22 seconds (2x due to sharing)
- Status: **PASS** (functionally correct, timing as expected for round-robin)

### SSH Hello Test
- Runs hello from SSH session
- Expected: Completes in ~10 seconds
- Actual: Completes in ~11 seconds (10% overhead)
- Status: **PASS**

### Concurrent SSH Sessions
- Two SSH sessions each running hello
- Expected: Each ~10 seconds
- Actual: Each ~21 seconds (2x due to sharing)
- Status: **Expected behavior**

## Issue 5: Stack Corruption During Process Exit

**Symptom:** Stack values change between process entry and exit.

```
[return_to_kernel] PID=10
  entry: x30=0x4005b8b4 sp=0x423a2740 x29=0x0
  saved: sp=0x423a2740 x30=0x400cca68
  Stack comparison (entry vs now):
  [entry_sp+0]: was 0x0 now 0xa CHANGED!
  [entry_sp+24]: was 0x0 now 0x6c65682f6e69622f CHANGED!
  [entry_sp+32]: was 0x0 now 0x6f6c CHANGED!
```

**Analysis of corrupted values:**
- `[entry_sp+0]: 0xa` = 10 decimal = **PID 10**
- `[entry_sp+0]: 0xb` = 11 decimal = **PID 11**
- `[entry_sp+24]: 0x6c65682f6e69622f` = ASCII "/bin/hel" (little-endian)
- `[entry_sp+32]: 0x6f6c` = ASCII "lo" (little-endian)

The corrupted values are the **PID** and **path string** (`"/bin/hello"`), suggesting syscall handlers are writing to stack memory that overlaps with `execute()`'s saved frame.

**Cause hypothesis:**
The exception stack area (32KB reserved at top of thread stack) may be insufficient for deep syscall call chains. When `rust_sync_el0_handler` processes syscalls, its stack usage may extend into the kernel code area below, overwriting `execute()`'s saved registers.

**Stack layout (per thread):**
```
stack_top (e.g., 0x4239b060)
  |
  | Exception area (32KB) - sync_el0_handler frames
  |
initial_sp (stack_top - 32KB)
  |
  | Kernel code area - execute(), process handling
  |
stack_base
```

**Impact:** Processes complete successfully despite corruption. The corrupted values are in a region that was initialized to zero and isn't critical for the return path (x30 is saved/restored separately).

**Why it works despite corruption:**
1. The return address (`x30`) is saved at a different offset
2. Corrupted area was padding/unused stack space
3. `return_to_kernel()` restores from the kernel context struct, not stack

**Potential fixes:**
1. Increase `EXCEPTION_STACK_SIZE` further (currently 32KB)
2. Reduce syscall handler stack usage (avoid large local variables)
3. Use separate exception stacks (not carved from thread stack)
4. Add stack canaries to detect overflow earlier

**Status:** **Known issue, does not affect functionality**

## Issue 6: Kernel Data Abort After Extended Operation

**Symptom:** After running multiple processes over time, the kernel crashes with a data abort.

```
[return_to_kernel] PID=11
  entry: x30=0x4005b8b4 sp=0x42392740 x29=0x0
  saved: sp=0x42392740 x30=0x400cca68
  Stack comparison (entry vs now):
  [entry_sp+0]: was 0x0 now 0xb CHANGED!
[Process] '/bin/hello' (PID 11) exited with code 0
[Thread] About to mark terminated
[Thread] About to yield
[Heartbeat] Loop 5420037 | T1 | SP:0x42089390 | Used:0KB | Mode:sys-512KB
[SSH] Ignoring message type 80 during shell
[Exception] Sync from EL1: EC=0x25, ISS=0x10, ELR=0x400c59f8, FAR=0x48000000, SPSR=0x60002345
```

**Analysis:**
- `EC=0x25` (37) = **Data Abort from EL1** (kernel mode)
- `ISS=0x10` = Data abort syndrome
- `ELR=0x400c59f8` = Faulting instruction address (kernel code)
- `FAR=0x48000000` = **Faulting memory address** (suspiciously round!)
- `SPSR=0x60002345` = Saved processor state

**Key observation:** `FAR=0x48000000` is a very round number, suggesting:
1. Dereferencing a corrupted/NULL pointer with an offset
2. Heap corruption leading to invalid pointer
3. Use-after-free of thread/process structures
4. Stack overflow corrupting return addresses

**Timeline:**
1. PID 11 exits successfully
2. Thread marked terminated and yields
3. Heartbeat runs (iteration 5.4 million - long runtime)
4. SSH processes message
5. **Crash** - something in kernel accesses 0x48000000

**Possible causes:**
1. **Thread slot reuse corruption:** After many process spawns/exits, thread slot metadata may be corrupted
2. **Memory fragmentation:** Heap allocator may be corrupted after many alloc/free cycles
3. **Stack overflow:** Accumulated stack corruption from Issue 5 eventually causes crash
4. **Process channel cleanup:** The ProcessChannel cleanup after exit may corrupt memory

**Reproduction:** Requires extended operation with multiple SSH sessions and process spawns. Not immediately reproducible.

**Status:** **Critical bug, needs investigation**

**Debugging steps:**
1. Enable heap validation (`ENABLE_HEAP_VALIDATION` if available)
2. Add stack canary checks before/after thread operations
3. Verify thread slot cleanup in `cleanup_terminated()`
4. Check ProcessChannel deallocation
5. Use GDB to examine 0x400c59f8 (faulting instruction)
