# Heap Corruption Investigation (January 2026)

## Summary

The kernel experiences intermittent crashes with `FAR=0x5` (Data Abort) and occasional `EC=0x0E` (Illegal Execution State). These crashes are non-deterministic, suggesting a race condition or memory corruption issue.

## Symptoms

### 1. FAR=0x5 Data Abort (EC=0x25)

**Pattern:** Crash in memcpy with destination address 0x5

```
[Exception] Sync from EL1: EC=0x25, ISS=0x61
  ELR=0x4027acb0, FAR=0x5, SPSR=0x80000345
  Instruction at ELR: 0xf800852e
  Likely: Rn(base)=x9, Rt(dest)=x14
```

**Analysis:**
- The instruction `0xf800852e` is a store in memcpy
- FAR=0x5 means the destination pointer is 0x5
- This value 0x5 corresponds to `thread_state::WAITING` constant
- A Vec or String's internal buffer pointer has been corrupted to value 5
- Happens in various contexts: SSH init, process tests, threading tests

### 2. Garbled Console Output

**Pattern:** Letters systematically dropped from output

```
Expected: "[Test] echo2 test PASSED (process creation succeeded)"
Actual:   "[Test] cho2 tst PASSED (procss cration succdd)"
```

**Analysis:**
- The letter 'e' (ASCII 0x65) is being dropped
- Interestingly: 0x65 with bits 5-6 cleared = 0x05
- May be related to the FAR=0x5 corruption pattern
- Could indicate UART register corruption or string buffer corruption

### 3. System Hang During Heap Allocation

**Pattern:** System hangs during ELF loader heap pre-allocation

```
[ELF] Stack: 0x3fff0000-0x40000000 (16 pages), guard=0x3ffef000, SP=0x3ffffff0
<hangs here - no more output>
```

**Analysis:**
- Hang occurs during `alloc_and_map` calls in `elf_loader.rs` (lines 265-280)
- Each call allocates a physical page from PMM and maps it
- Could be PMM lock deadlock or silent crash from heap corruption

## Fixes Applied

### 1. Banned Heap-Allocating Print Macros

Added Clippy lint to warn on `alloc::format!` usage:
```toml
# Cargo.toml
[lints.clippy]
disallowed_macros = "warn"
```

```toml
# clippy.toml
disallowed-macros = [
    { path = "alloc::format", reason = "Use safe_print! macro instead" },
]
```

Replaced ~249 instances of `console::print(&format!(...))` with `safe_print!`.

### 2. Removed Vec from IRQ Work Queue

Replaced dynamic Vec with static lock-free ring buffer in `executor.rs`:

```rust
// Before
static IRQ_WORK_QUEUE: Spinlock<Vec<IrqWork>> = ...

// After
const IRQ_QUEUE_SIZE: usize = 16;
static IRQ_WORK_QUEUE: [AtomicU8; IRQ_QUEUE_SIZE] = ...
static IRQ_QUEUE_HEAD: AtomicUsize = AtomicUsize::new(0);
static IRQ_QUEUE_TAIL: AtomicUsize = AtomicUsize::new(0);
```

### 3. Removed Vec from SSH Buffer Free Queue

Replaced dynamic Vec with static ring buffer in `ssh/server.rs`:

```rust
// Before
static PENDING_BUFFER_FREE: Spinlock<Vec<usize>> = ...

// After
const PENDING_FREE_QUEUE_SIZE: usize = 32;
static PENDING_FREE_SLOTS: [AtomicUsize; PENDING_FREE_QUEUE_SIZE] = ...
```

### 4. Removed SSH Fallback Connections

Eliminated the entire "fallback connections" mechanism from `ssh/server.rs`:
- Removed `ActiveConnection` struct
- Removed `fallback_connections` Vec
- Simplified connection acceptance to directly spawn threads or reject

### 5. Fixed EC=0x0E (Illegal Execution State) Crashes

Added IL bit clearing before ERET in all exception handlers:

```asm
// Clear IL bit in SPSR before ERET to prevent EC=0xe
mrs     x2, spsr_el1
bic     x2, x2, #0x100000       // Clear IL bit (bit 20)
msr     spsr_el1, x2
```

Applied to:
- `irq_handler`
- `irq_el0_handler`
- `default_exception_handler`
- `sync_el1_handler`
- Syscall return path in `sync_el0_handler`

### 6. Allocator IRQ Protection

Confirmed all allocator functions wrap operations in `with_irqs_disabled()`:
- `talc_alloc`
- `talc_dealloc`
- `talc_realloc`

## Remaining Investigation Areas

### 1. Thread State Corruption Theory

The value 5 (`thread_state::WAITING`) appearing as a Vec pointer is suspicious:

```rust
// src/threading.rs
pub mod thread_state {
    pub const WAITING: u8 = 5;
}
```

**Investigation needed:**
- Check for out-of-bounds writes to `THREAD_STATES` array
- Verify all array index bounds checks
- Look for any place where thread state could be written to wrong address

### 2. Talc Allocator Internals

The talc allocator may have issues under certain conditions:

**Investigation needed:**
- Check talc's behavior when heap is fragmented
- Verify talc's internal metadata isn't being corrupted
- Consider enabling `USE_PAGE_ALLOCATOR` (currently disabled with "DOES NOT ACTUALLY WORK" comment)

### 3. UART Race Conditions

Console output from multiple contexts could cause issues:

**Investigation needed:**
- Add spinlock protection around UART writes
- Check if IRQ handlers print during normal output
- Verify no concurrent access to UART registers

### 4. PMM Lock Ordering

The hang during heap allocation suggests possible deadlock:

**Investigation needed:**
- Check lock ordering between PMM and heap allocator
- Verify no nested lock acquisition
- Add timeout or deadlock detection to spinlocks

### 5. Memory Layout Overlap

Potential overlap between kernel structures:

**Investigation needed:**
- Verify `THREAD_STATES` array doesn't overlap with heap
- Check alignment of static arrays
- Dump memory map at runtime to verify no collisions

## Debugging Tools Added

### Debug Print in ELF Loader

```rust
// src/elf_loader.rs
crate::console::print("[ELF] heap pre-alloc starting\n");
// ... allocations ...
crate::console::print("[ELF] heap pre-alloc done\n");
```

## Reproduction

The crashes are non-deterministic:
- Sometimes crash early (during SSH init)
- Sometimes crash later (during process tests)
- Sometimes run successfully

This pattern strongly suggests a race condition or timing-dependent corruption.

## Debugging Tools Added

### Allocation Registry (January 2026)

Added comprehensive allocation tracking in `allocator.rs`:

```rust
// Enable in allocator.rs
pub const ENABLE_ALLOCATION_REGISTRY: bool = true;
pub const ENABLE_CANARIES: bool = true;
```

**Features:**
- Tracks all live allocations (up to 4096)
- Detects overlapping allocations
- Detects double frees and invalid frees
- Adds 8-byte canary guards before/after each allocation
- Scans for canary corruption at dealloc time

**Usage:**
```rust
// Print registry stats
allocator::print_registry_stats();

// Scan all allocations for canary corruption
let corrupted = allocator::scan_for_corruption();

// Dump all active allocations
allocator::dump_allocations();
```

**Output when corruption detected:**
```
[ALLOC] CANARY CORRUPTION (after) at 0x40300100+64: expected 0xfeedfacedeadc0de, got 0x5
```

### GDB Debugging Strategy

GDB is **highly recommended** for debugging the FAR=0x5 corruption:

1. **Start QEMU with GDB server:**
   ```bash
   ./scripts/run_with_gdb.sh
   ```

2. **Connect GDB and set hardware watchpoint:**
   ```gdb
   target remote :1234
   # If you know the corrupted address (from crash output or allocation registry):
   watch *(uint64_t*)0x40300108  # Watch the after-canary location
   continue
   ```

3. **When watchpoint triggers:**
   - GDB will break when the memory is written
   - Use `bt` to get full backtrace
   - Use `info registers` to see CPU state
   - The corruption source will be in the call stack

4. **For hunting the 0x5 value:**
   ```gdb
   # Watch for any write of value 5 to a specific memory range
   # (requires knowing where the Vec buffer is)
   watch -l *(uint64_t*)0xADDRESS if *(uint64_t*)0xADDRESS == 5
   ```

5. **Useful GDB commands for kernel debugging:**
   ```gdb
   info threads
   info registers
   x/16gx $sp           # Examine stack
   x/8i $pc             # Disassemble at PC
   p/x $ttbr0_el1       # Check page tables
   ```

## Next Steps

1. ~~**Add heap canaries**: Wrap allocations with guard bytes to detect overflows~~ **DONE**
2. **Enable page allocator**: Fix and enable `USE_PAGE_ALLOCATOR` for better isolation
3. **Lock UART output**: Add spinlock to console::print to prevent interleaving
4. **Audit THREAD_STATES usage**: Check all array accesses for bounds safety
5. ~~**Add allocation tracing**: Log all alloc/dealloc with addresses to find corruption source~~ **DONE**
6. **Use GDB watchpoint**: Set hardware watchpoint on the corrupted memory to catch the writer
