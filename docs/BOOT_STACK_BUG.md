# Boot Stack Corruption Bug

## Summary

The kernel boot code places the stack at a **hardcoded 1MB offset** from the kernel base, but the kernel binary is **~3MB**. This causes the stack pointer to be placed **inside the kernel's code section**, leading to silent corruption.

## Current State (as of Jan 3, 2026)

### Kernel Binary Size

| Build   | File Size | Loaded Size (text+data+bss) |
|---------|-----------|----------------------------|
| Release | 3.8 MB    | ~2.9 MB                    |
| Debug   | 50 MB     | Much larger                |

Section breakdown (release):
- `.text`: 2,942,872 bytes (~2.8 MB)
- `.data`: 520 bytes
- `.bss`: 98,000 bytes (~96 KB)

### Boot Stack Setup

In `src/boot.rs`:

```asm
.equ STACK_SIZE,        0x100000        // 1MB stack

_boot:
    ldr     x0, =KERNEL_PHYS_BASE       // 0x40000000
    add     x0, x0, #STACK_SIZE         // + 1MB = 0x40100000
    mov     sp, x0
```

### The Collision

```
Memory Layout (BROKEN):

0x40000000  ┌─────────────────────┐
            │                     │
            │   Kernel .text      │  ← Code loads here
            │   (~2.8 MB)         │
            │                     │
0x40100000  │─────────────────────│  ← SP set here (1MB offset)
            │                     │     Stack grows DOWN into code!
            │                     │
            │                     │
0x402E6870  └─────────────────────┘  ← Actual kernel end (~2.9 MB)
```

The stack pointer is set to `0x40100000`, which is **inside the `.text` section**. Every function call, exception frame, or local variable allocation writes into kernel code.

## Why It "Works" (Sort Of)

1. **Identity mapping**: The 1GB block mapping means all memory is accessible
2. **Stack usage patterns**: The boot sequence may not use much stack before jumping to Rust code
3. **Lucky corruption**: The overwritten code paths may not be immediately executed
4. **Threading**: After boot, spawned threads get their own 32KB stacks from the heap, avoiding the boot stack

## Symptoms

- Random crashes with `EC=0x0` (undefined instruction) at kernel addresses
- Intermittent failures that seem unrelated to recent code changes
- Debug builds (50MB) are guaranteed to fail catastrophically
- Corruption may only manifest when specific code paths are taken

## Fix Required

The boot code must place the stack **after** the kernel ends, not at a hardcoded offset.

### Option 1: Use `_kernel_phys_end` from Linker

The linker script already exports `_kernel_phys_end`:

```ld
// In linker.ld
_kernel_phys_end = .;
```

Boot code should use this symbol:

```asm
_boot:
    // Load kernel end address
    adrp    x0, _kernel_phys_end
    add     x0, x0, :lo12:_kernel_phys_end
    
    // Add stack size and align
    add     x0, x0, #STACK_SIZE
    and     x0, x0, #~0xF              // 16-byte align
    mov     sp, x0
```

### Option 2: Place Stack at Fixed High Address

Use an address guaranteed to be above the kernel:

```asm
.equ STACK_TOP,         0x42000000     // 32MB, matches Code+Stack region

_boot:
    ldr     x0, =STACK_TOP
    mov     sp, x0
```

This relies on the memory layout documentation stating Code+Stack gets 32MB minimum.

## Correct Memory Layout

After fixing:

```
0x40000000  ┌─────────────────────┐
            │   Kernel .text      │
            │   Kernel .data      │
            │   Kernel .bss       │
0x402E6870  ├─────────────────────┤  ← _kernel_phys_end
            │                     │
            │   Stack space       │  ← Stack grows DOWN
            │   (1MB)             │
            │                     │
0x403E6870  └─────────────────────┘  ← SP set here (kernel_end + 1MB)
            │                     │
            │   (remaining space  │
            │    before heap)     │
            │                     │
0x42000000  ├─────────────────────┤  ← Heap starts (Code+Stack = 32MB)
```

## Why No Memory Protection?

The kernel code can be overwritten because the boot page tables use **1GB block mappings** with no fine-grained permissions:

```asm
// In boot.rs
.equ NORMAL_BLOCK, (PT_VALID | PT_BLOCK | PT_AF | PT_SH_INNER | PT_ATTR_NORMAL)

// L1[1] = 0x4000_0000 - 0x7FFF_FFFF (RAM, 1GB block)
ldr     x0, =0x40000000
ldr     x1, =NORMAL_BLOCK
orr     x0, x0, x1
str     x0, [x13, #8]
```

The `NORMAL_BLOCK` flags are missing:
- **No `AP_RO_xxx`** - Memory is read-write (default)
- **No `PXN`** (Privileged Execute Never) - Code can execute anywhere
- **No `UXN`** (User Execute Never) - Not relevant for kernel

This means the entire 1GB RAM region is **RWX** (read-write-execute). There's no hardware protection because everything is one giant block.

### Why This Matters

| What Should Happen | What Actually Happens |
|-------------------|----------------------|
| Stack write to code → Page fault | Stack write to code → Silent corruption |
| Code section is read-only | Code section is writable |
| Guard page between stack/code | No guard, stack grows into code |

### Proper Fix (Future Work)

To add memory protection, the kernel would need to:

1. **Use 4KB page mappings** for the kernel region instead of 1GB blocks
2. **Mark sections appropriately**:
   - `.text` → Read-only + Executable (`AP_RO_EL1`)
   - `.rodata` → Read-only + No-execute (`AP_RO_EL1 | PXN`)
   - `.data/.bss` → Read-write + No-execute (`AP_RW_EL1 | PXN`)
   - Stack → Read-write + No-execute (`AP_RW_EL1 | PXN`)
3. **Add guard page** - Unmap one page between stack and code so overflow causes a fault

This requires significant changes to:
- `src/boot.rs` - Create L2/L3 tables for kernel region
- `linker.ld` - Page-align sections and export section boundaries
- Potentially `src/mmu.rs` - Helper functions for kernel page setup

The AArch64 flags needed are already defined in `src/mmu.rs`:

```rust
pub const AP_RO_EL1: u64 = 2 << 6;  // Read-only at EL1
pub const PXN: u64 = 1 << 53;       // Privileged execute never
```

## Related Documentation

- `docs/MEMORY_LAYOUT.md` - Overall memory layout and sizing
- `src/boot.rs` - Boot code with stack setup
- `src/mmu.rs` - Page table flags (AP, PXN, UXN)
- `linker.ld` - Linker script with `_kernel_phys_end` symbol

## Testing

After fixing, verify with:

```bash
# Check kernel size
rust-size target/aarch64-unknown-none/release/akuma

# Ensure stack is placed correctly (add debug output to boot or check in GDB)
lldb target/aarch64-unknown-none/release/akuma -o "gdb-remote 1234"
(lldb) register read sp
```

The SP value should be **greater than** the kernel end address shown by `rust-size`.

