# User Stack Size Increase (64KB → 128KB)

**Date:** January 2026  
**Issue:** Stack overflow in scratch during git pack parsing

## Symptom

When cloning a GitHub repository, scratch crashed with a data abort:

```
scratch: pack contains 4 objects
[Fault] Data abort from EL0 at FAR=0x3ffef110, ELR=0x421388, ISS=0x47
[Process] PID 12 thread 8 exited (-11)
```

Key indicators:
- **FAR=0x3ffef110**: Address inside the guard page (0x3ffef000)
- **ISS=0x47**: Write access fault
- **Exit -11**: SIGSEGV equivalent

## Root Cause

The 64KB userspace stack was insufficient for the deep call chain during pack parsing:

```
_start → main → cmd_clone → clone → fetch_pack_streaming → parse_pack_from_file
  → parse_all → parse_entry → decompress_next → decompress_with_consumed → inflate
```

The miniz_oxide `inflate()` function has significant internal stack usage even though `InflateState` (~32KB) is heap-allocated via `new_boxed()`.

## Decision: Increase to 128KB

### Why 128KB?

1. **Matches kernel-side thread stack**: `USER_THREAD_STACK_SIZE` was already 128KB
2. **Conservative increase**: Doubles the stack without excessive memory use
3. **Minimal impact**: Only loses 64KB of mmap region (765MB → 767MB available)

### Alternatives Considered

| Option | Pros | Cons |
|--------|------|------|
| Increase to 128KB | Simple, matches kernel config | Uses 64KB more per process |
| Reduce call depth | No memory increase | Invasive refactoring required |
| Increase to 256KB | More headroom | Excessive for most programs |

### Memory Layout Impact

The layout is dynamically calculated, so the change is safe:

```
With 64KB stack:                    With 128KB stack:
├─ Guard:  0x3FFEF000               ├─ Guard:  0x3FFDF000
├─ Stack:  0x3FFF0000-0x40000000    ├─ Stack:  0x3FFE0000-0x40000000
├─ mmap limit: 0x3FEF0000           ├─ mmap limit: 0x3FED0000
└─ mmap available: ~767MB           └─ mmap available: ~765MB
```

The 1MB buffer between mmap region and stack is preserved.

## Changes Made

### Kernel (src/)

1. **config.rs**: `USER_STACK_SIZE` 64KB → 128KB
2. **elf_loader.rs**: Updated comment to reflect 128KB default
3. **process.rs**: Updated comment for stack address example
4. **threading.rs**: Fixed 4 outdated comments referencing 64KB

### Userspace (userspace/)

1. **libakuma/src/lib.rs**: Added exported constants:
   - `USER_STACK_SIZE = 128KB`
   - `STACK_TOP = 0x40000000`
   - `PAGE_SIZE = 4096`

### Documentation

1. **userspace/scratch/docs/PACK_PARSING_CRASH.md**: Updated with root cause and fix

## Verification

```bash
cargo build --release
cargo run --release
# Inside Akuma:
scratch clone https://github.com/user/repo
```

The pack parsing should complete without data abort.

## Future Considerations

If 128KB proves insufficient for other workloads:
- Consider per-binary stack size (ELF header or config file)
- Profile stack usage with canary checking (`ENABLE_STACK_CANARIES`)
- Maximum reasonable increase: 256KB (would leave ~763MB mmap)
