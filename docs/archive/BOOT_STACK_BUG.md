# Boot Stack Bug (RESOLVED)

## Summary

The kernel boot code **previously** placed the stack at a hardcoded 1MB offset from the kernel base, but the kernel binary is ~3MB. This caused the stack pointer to be placed inside the kernel's code section.

**Status**: ✅ **FIXED** - Stack now placed at `0x42000000` (32MB from kernel base).

## The Original Bug

### Kernel Binary Size

| Build   | File Size | Loaded Size (text+data+bss) |
|---------|-----------|----------------------------|
| Release | 3.8 MB    | ~2.9 MB                    |
| Debug   | 50 MB     | Much larger                |

Section breakdown (release):
- `.text`: 2,942,872 bytes (~2.8 MB)
- `.data`: 520 bytes
- `.bss`: 98,000 bytes (~96 KB)

### Original (Broken) Boot Stack Setup

```asm
.equ STACK_SIZE,        0x100000        // 1MB stack

_boot:
    ldr     x0, =KERNEL_PHYS_BASE       // 0x40000000
    add     x0, x0, #STACK_SIZE         // + 1MB = 0x40100000
    mov     sp, x0
```

### The Collision (OLD)

```
Memory Layout (WAS BROKEN):

0x40000000  ┌─────────────────────┐
            │                     │
            │   Kernel .text      │  ← Code loads here
            │   (~2.8 MB)         │
            │                     │
0x40100000  │─────────────────────│  ← SP WAS set here (1MB offset)
            │                     │     Stack grew DOWN into code!
            │                     │
            │                     │
0x402E6870  └─────────────────────┘  ← Actual kernel end (~2.9 MB)
```

## The Fix

The boot code now places the stack at a fixed address well above the kernel:

```asm
.equ STACK_SIZE,        0x100000        // 1MB stack
.equ STACK_TOP,         0x42000000      // 32MB from kernel base

_boot:
    // Set up early stack (physical address)
    // Place at top of Code+Stack region (32MB from kernel base)
    // This ensures stack is well above the ~3MB kernel binary
    ldr     x0, =STACK_TOP
    mov     sp, x0
```

### Correct Memory Layout (CURRENT)

```
0x40000000  ┌─────────────────────┐
            │   Kernel .text      │
            │   Kernel .data      │
            │   Kernel .bss       │
0x402E6870  ├─────────────────────┤  ← _kernel_phys_end (~3MB)
            │                     │
            │   (free space)      │
            │                     │
0x41F00000  ├─────────────────────┤  ← Stack bottom (grows down from 0x42000000)
            │                     │
            │   Boot Stack        │  ← 1MB stack space
            │   (grows down)      │
            │                     │
0x42000000  ├─────────────────────┤  ← SP set here (STACK_TOP)
            │                     │
            │   Heap starts       │
            │                     │
```

## Remaining Limitations

### No Guard Page

Stack overflow still causes silent corruption (into free space or heap). There is no guard page between the stack and other memory.

### No Code Protection

The kernel still uses **1GB block mappings** with RWX permissions:

```asm
.equ NORMAL_BLOCK, (PT_VALID | PT_BLOCK | PT_AF | PT_SH_INNER | PT_ATTR_NORMAL)
```

Missing protections:
- **No `AP_RO_xxx`** - Kernel code is writable
- **No `PXN`** - All memory is executable
- **No guard pages** - Overflow detection not possible with 1GB blocks

### Future Work

To add proper memory protection:

1. **Use 4KB page mappings** for the kernel region instead of 1GB blocks
2. **Mark sections appropriately**:
   - `.text` → Read-only + Executable
   - `.rodata` → Read-only + No-execute
   - `.data/.bss` → Read-write + No-execute
   - Stack → Read-write + No-execute + Guard page
3. **Add guard page** - Unmap one page at stack bottom

See `docs/THREAD_STACK_ANALYSIS.md` for detailed guard page implementation guidance.

---

## Safeguards Against Recurrence

Two safeguards prevent the kernel from growing too large and overlapping the stack:

### Build-Time Check (Linker Assertion)

The linker script (`linker.ld`) includes an assertion that fails the build if the kernel is too large:

```ld
STACK_BOTTOM = 0x41F00000;
ASSERT(_kernel_phys_end < STACK_BOTTOM, 
    "FATAL: Kernel binary overlaps boot stack!")
```

If the kernel grows beyond 31MB, the build fails with:
```
ld.lld: error: FATAL: Kernel binary overlaps boot stack!
```

### Runtime Check (kernel_main)

Early in `kernel_main()` (`src/main.rs`), we verify the kernel fits:

```rust
const STACK_BOTTOM: usize = 0x41F0_0000;
let kernel_end = unsafe { &_kernel_phys_end as *const u8 as usize };

if kernel_end >= STACK_BOTTOM {
    console::print("!!! FATAL: Kernel binary overlaps with boot stack !!!\n");
    halt();
}

// Warn if getting close (< 4MB margin)
let margin = STACK_BOTTOM - kernel_end;
if margin < 4 * 1024 * 1024 {
    console::print("WARNING: Kernel is within 4MB of stack!\n");
}
```

### Current Status

The kernel is currently ~3.5MB, leaving ~27.5MB margin before the 31MB limit.

| Component | Address | Size |
|-----------|---------|------|
| Kernel base | `0x40000000` | - |
| Kernel end | `~0x40380000` | ~3.5MB |
| Stack bottom | `0x41F00000` | - |
| Stack top | `0x42000000` | 1MB |
| **Margin** | - | **~27.5MB** |

---

## Testing

Verify the fix:

```bash
# Check kernel size
rust-size target/aarch64-unknown-none/release/akuma

# Boot and check SP value (should be 0x42000000)
# In GDB/LLDB at _boot:
(lldb) register read sp
# Should show: sp = 0x0000000042000000
```

## Related Documentation

- `docs/MEMORY_LAYOUT.md` - Overall memory layout and sizing
- `docs/THREAD_STACK_ANALYSIS.md` - Thread stacks and guard page implementation
- `src/boot.rs` - Boot code with stack setup
- `src/mmu.rs` - Page table flags (AP, PXN, UXN)
- `linker.ld` - Linker script with `_kernel_phys_end` symbol
