# Context Switch and Threading Bug Analysis

This document summarizes the investigation into kernel crashes related to context switching, thread management, and exception handling.

## Bug 1: SPSR=0x4 (EL1t Mode) Crash

### Symptoms
- Crash with `EC=0x25` (Data Abort), `FAR=0x3fffffa0` (user address)
- `SPSR=0x4` indicating EL1t mode (EL1 with SP_EL0)
- Kernel trying to access user-space stack address
- Occurred after ~1 million context switches

### Root Cause Analysis
SPSR=0x4 means bits[3:0]=4, which is EL1t (EL1 using SP_EL0 as stack pointer). Normal kernel code runs in EL1h (bits[3:0]=5, using SP_EL1).

When the kernel runs in EL1t:
1. The SP register IS SP_EL0 (which contains a user stack address like 0x3fffffa0)
2. Any stack access tries to use that user address
3. User addresses aren't mapped in kernel TTBR0 → crash

The transition to EL1t can only happen via ERET with SPSR bits[0]=0.

### Contributing Factors

1. **New threads initialized with SPSR=0**: All spawn functions set `context.spsr = 0`, which means EL0 (user mode). While `switch_context` loads this into SPSR_EL1, it normally gets overwritten by hardware on next exception. However, any code path that ERETed with this value would crash.

2. **switch_context modifies SPSR_EL1**: The context switch loads SPSR from the saved context into SPSR_EL1. If an unexpected exception occurs and default_exception_handler returns, it ERETed with whatever SPSR_EL1 contained.

3. **Gradual corruption**: After running for ~1M context switches, something corrupted the saved SPSR on a thread's stack, causing ERET to use wrong value.

### Fixes Applied

1. **Initialize SPSR to EL1h (0x00000005)**: All spawn functions now set `context.spsr = 0x00000005` (EL1h) instead of 0.

2. **Safety check in irq_handler**: Before ERET, check if SPSR bits[3:0] == 4 (EL1t) or == 0 (EL0 with kernel ELR), and fix to EL1h.

3. **Disabled SGI debug prints**: The `alloc::format!` calls in IRQ handler could deadlock if allocator lock was held when timer fired.

## Bug 2: Nested IRQ Context Corruption

### Symptoms
- `[SGI CORRUPT]` messages
- System threads getting user-mode SPSR/ELR values

### Root Cause
In `switch_context`, DAIF was restored from saved context. If the saved DAIF had IRQs enabled (bit 7 = 0), IRQs could fire mid-switch. Since `CURRENT_THREAD` was already updated to the new thread, the nested IRQ handler would save state to the wrong thread's context.

### Fix Applied
Removed DAIF restoration from `switch_context`. IRQs stay masked throughout the switch. New threads enable IRQs via `thread_start_closure` checking x21 register.

## Bug 3: Deadlock in Cleanup + Timer IRQ

### Symptoms
- System hang during thread cleanup

### Root Cause
`cleanup_terminated_internal` acquired `POOL.lock()` without disabling IRQs. If a timer fired while holding the lock, the SGI handler would try to acquire the same lock → deadlock on single CPU.

### Fix Applied
Added `IrqGuard` around `POOL.lock()` acquisitions in cleanup and other functions that could be interrupted.

## Bug 4: Allocator Deadlock in IRQ Handler

### Symptoms
- System hang during context switch debug prints

### Root Cause
SGI debug prints used `alloc::format!()` which acquires the allocator lock. If the main code was allocating when the timer fired, the IRQ handler would deadlock trying to allocate.

### Fix Applied
Disabled `ENABLE_SGI_DEBUG_PRINTS`. For production, avoid any allocation in IRQ handlers.

## Architecture Notes

### Exception Levels
- EL0: User mode
- EL1h: Kernel mode with SP_EL1 (normal)
- EL1t: Kernel mode with SP_EL0 (abnormal, causes crashes)

### SPSR Values
- `0x00000000` = EL0 (user mode)
- `0x00000004` = EL1t (EL1 with SP_EL0) - BAD
- `0x00000005` = EL1h (EL1 with SP_EL1) - CORRECT for kernel
- `0x80000345` = EL1h with IRQ masked and various flags

### Context Switch Flow
1. Timer IRQ taken, CPU sets SPSR_EL1 = interrupted PSTATE
2. irq_handler saves SPSR_EL1 to stack
3. sgi_scheduler_handler updates CURRENT_THREAD
4. switch_context saves/loads contexts
5. irq_handler restores SPSR_EL1 from stack
6. ERET uses SPSR_EL1 to return

### Critical Invariants
1. CURRENT_THREAD must match the thread whose stack we're on
2. SPSR on stack must match the mode the thread was in when interrupted
3. IRQs must stay masked during switch_context to prevent nested corruption
4. No allocations in IRQ handlers to prevent deadlock

## Bug 5: NULL Pointer Dereference (FAR=0xfffffffffffffffd)

### Symptoms
```
EC=0x25, ISS=0x4
ELR=0x40090618, FAR=0xfffffffffffffffd
SPSR=0x80000345 (correct EL1h)
Thread=0, only 1 thread exists
```
- Crash very early in boot (37K allocations vs 3M+ in normal runs)
- Test output: `kthreads check: waiting (tid1=8 found=false, tid2=9 found=false)`

### Analysis
- `FAR=0xfffffffffffffffd` = `-3` in signed, suggesting `NULL - 3` or similar
- `ISS=0x4` = Translation fault level 0 (reading from completely unmapped address)
- `SPSR=0x80000345` = Correct EL1h with IRQ masked - **SPSR fix is working**
- Occurred after SPSR/irq_handler fixes were applied

### Root Cause
**Under investigation.** Possible causes:
1. Test waiting for threads 8,9 that were never spawned
2. Corrupted pointer from earlier changes
3. Race condition in thread spawning during tests

### Status
This is a new bug introduced after the SPSR fixes. The context switch SPSR corruption is fixed (SPSR is now correct EL1h), but something else is broken.

## Files Modified
- `src/threading.rs`: SPSR initialization, DAIF handling, deadlock fixes
- `src/exceptions.rs`: SPSR safety check in irq_handler
- `src/config.rs`: Disabled SGI debug prints
- `src/irq.rs`: IrqGuard usage
