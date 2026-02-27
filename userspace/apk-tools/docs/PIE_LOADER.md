# Static-PIE ELF Loading on Akuma

## Background

Alpine's `apk` binary is compiled as static-PIE (`-static-pie`), producing an
ELF with type `ET_DYN` instead of `ET_EXEC`. This required two changes to the
kernel.

## Change 1: ELF loader accepts ET_DYN (`src/elf_loader.rs`)

The loader previously rejected anything that wasn't `ET_EXEC`. Now it also
accepts `ET_DYN` (static-PIE):

- Segments are loaded at a fixed base address `0x1000_0000` (PIE p_vaddr
  values start near 0).
- Entry point, PHDR address, and all segment VAs get the base offset added.
- Kernel-side relocations are **skipped** for PIE binaries. musl's startup
  code (`_dlstart_c` / `__dls3`) self-relocates by processing RELR entries
  before calling `main`.

## Change 2: mmap region must start after code (`src/process.rs`)

### The Bug

After loading the PIE binary at `0x1000_0000`, the process's mmap allocator
was **also** configured to start at `0x1000_0000` (hardcoded). When musl's
startup code called `mmap` for TLS allocation (`__copy_tls`), the allocator
handed out pages at `0x1000_0000` and mapped them as `RW_NO_EXEC` —
overwriting the code's page table entries.

Result: instruction permission fault at the first code address that got
remapped.

```
[ELF] Segment: VA=0x10000000 filesz=0x448ca0 memsz=0x448ca0 flags=R-X
[Process] PID 18 memory: mmap=0x10000000-0x3fec0000
[Fault] Instruction abort from EL0 at FAR=0x100186e8, ISS=0xf
```

ISS=0xf = permission fault, level 3. The L3 page table entry existed but had
`RW_NO_EXEC` flags (from mmap) instead of `RX` (from ELF loading).

### The Fix

`ProcessMemory::new()` now computes `mmap_start` dynamically:

```rust
let mmap_start = (code_end + 0x1000_0000) & !0xFFFF;
```

This places the mmap region 256MB after the end of the code/data/heap area,
64KB-aligned. The 256MB gap leaves room for heap growth via `brk` (musl's
`malloc` uses `brk` for small allocations, growing upward from `code_end`).

| Binary type | code_end | mmap_start | Heap room |
|-------------|----------|------------|-----------|
| ET_EXEC (e.g. XBPS) | ~0x700000 | ~0x10700000 | 256 MB |
| PIE (e.g. apk) | ~0x104e0000 | ~0x204e0000 | 256 MB |

The old hardcoded `0x1000_0000` gave ET_EXEC binaries ~250MB of heap
room. The new formula gives both binary types a consistent 256MB, while
ensuring PIE code pages are never overwritten.

**Why 1MB was not enough:** XBPS downloads repository index files (~2MB)
and inflates them in memory. With only 1MB between heap and mmap, the heap
(`brk`) grew past the mmap region, corrupting mmap allocations and causing
a NULL pointer dereference.

## Memory Layout (PIE)

```
0x0000_0000 ┌──────────────────────┐
            │ (unmapped)           │
0x1000_0000 ├──────────────────────┤
            │ Code (RX)            │ ← ELF LOAD segment 1
0x1045_0000 ├──────────────────────┤
            │ Data/BSS (RW)        │ ← ELF LOAD segment 2
0x104e_0000 ├──────────────────────┤
            │ Heap (brk, RW)       │ ← grows up
            │ (256MB VA gap)       │   no physical memory used
0x204e_0000 ├──────────────────────┤
            │ mmap region (RW)     │ ← grows up (TLS, mmap calls)
            │                      │
0x3fec_0000 ├──────────────────────┤
            │ (1MB guard)          │
0x3ffc_0000 ├──────────────────────┤
            │ Stack (RW)           │ ← grows down
0x4000_0000 └──────────────────────┘
```

## Virtual vs Physical Memory

The 256MB gap between the heap and mmap regions is a **virtual address
reservation only**. No physical memory is consumed by the gap. Both `brk`
and `mmap` allocate physical pages on demand via `alloc_and_map`, which
grabs pages from the PMM one at a time as they are touched. A process only
uses physical RAM for pages it has actually mapped.

The gap exists so that `brk` (growing upward) and `mmap` (also growing
upward from a higher base) don't allocate overlapping virtual addresses,
which would cause one to silently overwrite the other's page table entries.
There is no enforced upper limit on `brk` — it can grow until physical
memory runs out — but exceeding the 256MB gap would collide with mmap
allocations.
