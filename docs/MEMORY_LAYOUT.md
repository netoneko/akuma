# Memory Layout

This document describes the kernel memory layout and important sizing constraints.

## Physical Memory Map

The kernel runs on QEMU's `virt` machine with the following physical address layout:

| Address Range | Size | Description |
|---------------|------|-------------|
| 0x00000000 - 0x3FFFFFFF | 1GB | Device memory (GIC, UART, etc.) |
| 0x40000000 - 0x401FFFFF | 2MB | DTB (Device Tree Blob, placed by QEMU) |
| 0x40200000 - onwards | Configurable | Kernel + RAM (default 256MB) |

## ARM64 Image Header Boot

The kernel uses the ARM64 Linux Image header format for flat binary loading. This allows
QEMU to pass the DTB address in register `x0`. Key details:

- **Kernel load address**: `0x40200000` (RAM_BASE + 2MB)
- **DTB location**: `0x40000000` (first 2MB of RAM)
- **Boot stack**: 1 MB, placed immediately above the kernel image — *not* a fixed
  address. Its location is derived from the actual linked image size (see
  *Boot-stack reservation (linker-derived)* below).

The ARM64 Image header (first 64 bytes of the binary) specifies:
- `text_offset = 0` with `image_size != 0` → QEMU adds 2MB, loading at `0x40200000`.
  `image_size` is the linker symbol `IMAGE_RESERVE` (load address → boot-stack
  bottom), so it tracks the real binary instead of a hand-tuned per-profile value.
- `ARM\x64` magic at offset 56 enables DTB passing in `x0`

## Boot-stack reservation (linker-derived)

The 1 MB boot stack sits immediately above the kernel image, inside the
Code + Stack region. Its bounds are **not** hardcoded — `linker.ld` derives them
from `_kernel_phys_end` (the true end of the linked image, incl. `.bss`) and
exports three absolute symbols that every consumer reads, so the reservation
auto-tracks the binary on every build:

```ld
STACK_BOTTOM  = ALIGN(_kernel_phys_end, 0x1000) + 0x2000;  /* page-align + 2-page guard */
STACK_TOP     = STACK_BOTTOM + 0x100000;                   /* 1 MB boot stack; initial SP */
IMAGE_RESERVE = STACK_BOTTOM - KERNEL_PHYS_BASE;           /* ARM64 Image header image_size */
```

- `src/boot.rs` asm loads `STACK_TOP` as the initial SP and emits `IMAGE_RESERVE`
  in the Image header.
- `src/main.rs` reads `STACK_BOTTOM`/`STACK_TOP` for the kernel↔stack overlap
  guard, the `code_and_stack` heap reservation, and the threading-crate
  `ExecConfig` boot-stack bounds.
- `src/exceptions.rs` reads `STACK_TOP` for the boot thread's exception stack.

This replaced a per-profile `IMAGE_SIZE` constant that had to be kept in 3-way
lockstep across `build.rs`/`boot.rs`/`main.rs` — a stale copy had previously
stamped the boot-stack canary into the kernel heap at low RAM. The boot self-test
`test_boot_stack_reservation_invariants` (`src/process_tests.rs`) guards the
relationships. See `docs/LOW_MEMORY_ENVIRONMENT.md` for the per-profile numbers.

## Kernel Memory Regions

Within RAM, memory is divided as follows:

| Region | Size Formula | Description |
|--------|--------------|-------------|
| DTB | 2MB (fixed) | Device Tree Blob at 0x40000000 |
| Code + Stack | max(1/16 of RAM, 8MB) | Kernel binary and boot stack |
| Heap | fixed 16MB | Kernel heap managed by allocator |
| User Pages | Remaining | Physical pages for user processes, managed by PMM |

### Example: 256MB RAM

```
=== Memory Layout ===
Total RAM: 256 MB at 0x40000000
Code+Stack: 16 MB (0x40000000 - 0x41000000) [min 8MB]
Heap:       16 MB (0x41000000 - 0x42000000) [fixed 16MB]
User pages: 224 MB (0x42000000 - 0x50000000) [remaining]
=====================
```

### Example: 1024MB (1GB) RAM

```
=== Memory Layout ===
Total RAM: 1024 MB at 0x40000000
Code+Stack: 64 MB (0x40000000 - 0x44000000) [min 8MB]
Heap:       16 MB (0x44000000 - 0x45000000) [fixed 16MB]
User pages: 944 MB (0x45000000 - 0x80000000) [remaining]
=====================
```

## Verified boot floors (measured June 2026)

Every (profile, RAM) cell was booted and probed over SSH with `busybox echo`
(a real userspace spawn). Full matrix + PMM free-page numbers:
`docs/LOW_MEMORY_ENVIRONMENT.md` → *Full boot + hello matrix*.

| Profile | Boot-to-hello floor | Notes |
|---------|---------------------|-------|
| `extreme` | **4.5 MB** | 821 KB image; smallest reserve → boots+hello at 4.5 MB (523 free pages) |
| `size`    | **6 MB** | 881 KB image; at 5 MB the layout guard trips (0 user pages) |
| `release` | **≤ 16 MB** | 2875 KB image |

**≤ 4.0 MB does not boot on any profile** — QEMU aborts with `Not enough space for
DTB after kernel/initrd` (the kernel loads at +2 MB, leaving too little for the
DTB). That is a QEMU guest-layout limit, not a kernel OOM, so shrinking the kernel
cannot break the ≤4.0 MB wall. Between 4.125–4.375 MB the extreme kernel starts
but its layout guard halts (0 user pages); 4.5 MB is the first size with usable
user pages. All profiles boot and run a userspace `hello` from their floor up to
4096 MB.

Below ~6 MB the binding constraint is the *workload's* working set, not the
kernel. On `extreme`, lightweight userspace runs at the 4.5 MB kernel floor —
including a one-shot LLM chat (`meow -c "say hi"`, verified replying at
4.5/5.0/5.5/6.0 MB) — while `tcc hello.c` needs 6 MB (it exhausts the PMM pool
demand-paging `libtcc.so`). Full breakdown: `docs/LOW_MEMORY_ENVIRONMENT.md` →
*Workload floors on extreme*.

## Kernel Binary Size Limit

**Important**: The kernel binary must fit within the Code + Stack region, with room for the stack.

| RAM Size | Code+Stack Region | Max Kernel Size (recommended) |
|----------|-------------------|-------------------------------|
| 256MB | 16MB | ~8MB |
| 512MB | 32MB | ~24MB |
| 1024MB | 64MB | ~56MB |

The current kernel is ~2.9MB (`release`); the `size`/`extreme` profiles are
~0.8–0.9MB. The boot-stack reservation is no longer a fixed per-profile ceiling —
`linker.ld` derives it from the actual linked size (see *Boot-stack reservation
(linker-derived)*), so a larger binary simply pushes the 1 MB boot stack up by the
same amount; it cannot silently collide with it.

## Boot Logging

The kernel logs memory layout decisions during boot:

```
DTB ptr from boot (x0 arg): 0x48000000
x0 at _boot entry: 0x48000000
Akuma Kernel starting...
Kernel binary: 2232 KB (0x40200000 - 0x4042e1c0)
[Memory] Detected from DTB: base=0x40000000, size=1024 MB

=== Memory Layout ===
Total RAM: 1024 MB at 0x40000000
Code+Stack: 64 MB (0x40000000 - 0x44000000) [min 8MB]
Heap:       16 MB (0x44000000 - 0x45000000) [fixed 16MB]
User pages: 944 MB (0x45000000 - 0x80000000) [remaining]
=====================
```

## Known Issue: Binary Size and Loading

### Symptom

When the kernel binary grows too large, the system crashes at boot with:
```
[Exception] Sync from EL1: EC=0x0, ISS=0x0, ELR=0x402xxxxx
```

Debugging with GDB/LLDB shows `udf #0x0` (undefined instruction = zeros) at code addresses that should contain valid instructions.

### Root Cause

The kernel code isn't being fully loaded into memory. This can happen when:
1. The binary size exceeds the Code+Stack region
2. Build artifacts become corrupted during incremental compilation
3. The ELF file has incorrect segment offsets

### Solutions

1. **Clean rebuild**: `cargo clean && cargo build --release`

2. **Increase RAM** (via `MEMORY` env var):
   ```bash
   MEMORY=1024M cargo run --release
   ```

3. **Check binary size**: Ensure it fits within the Code+Stack region
   ```bash
   rust-size target/aarch64-unknown-none/release/akuma
   ```

4. **Debug with GDB**: Start QEMU with `-s -S` and connect with LLDB:
   ```bash
   lldb target/aarch64-unknown-none/release/akuma -o "gdb-remote 1234"
   ```
   Check if memory at crash address contains actual code or zeros.

## Page Table Configuration

The kernel uses AArch64 4-level page tables with 4KB granule:

- **TTBR0_EL1**: Used for both kernel and user space (identity mapping)
- **TTBR1_EL1**: Points to same tables as TTBR0 (unused, reserved for future)

### Boot page tables

Boot code (`src/boot.rs`) sets up a **static** identity mapping using 1GB blocks,
covering only the first 3GB (it runs before the DTB is parsed, so it can't know
the RAM size):
- L0[0] → L1 table for identity mapping
- L1[0]: Device memory (0x00000000 - 0x3FFFFFFF)
- L1[1]: RAM block 1 (0x40000000 - 0x7FFFFFFF) — includes DTB at 0x40000000 and kernel at 0x40200000
- L1[2]: RAM block 2 (0x80000000 - 0xBFFFFFFF)

**Runtime extension for RAM > 2GB:** once the DTB gives the real RAM size,
`mmu::extend_boot_ram_identity_map()` (called from `mmu::init`) writes additional
1GB `NORMAL` block entries into this same boot L1 table for `[3GB, ram_end)`
(L1[3], L1[4], …). This is required because the PMM hands out frames across all
detected RAM and the kernel zeroes/accesses any frame via `phys_to_virt` (VA == PA)
while the boot table may be the active TTBR0 — without it, a frame ≥ 3GB faults in
kernel mode. See "RAM > 2 GB" below.

Device MMIO is also mapped via L0[1] at high virtual addresses:
- L3[0]: GIC distributor (PA 0x08000000 → VA 0x80_0000_0000)
- L3[1]: GIC CPU (PA 0x08010000 → VA 0x80_0000_1000)
- L3[2]: UART (PA 0x09000000 → VA 0x80_0000_2000)
- L3[3]: fw_cfg (PA 0x09020000 → VA 0x80_0000_3000)

### User page tables

Each process gets its own TTBR0 page tables. These include:
- **L1[0] → L2 table**: User code/data pages via L3 tables, plus VirtIO device
  pages at L2[80] (0x0a00_0000). GIC, UART, and fw_cfg are NOT mapped here —
  the kernel accesses them via a temporary TTBR0 swap to boot page tables
  (see `docs/DEVICE_MMIO_VA_CONFLICT.md`).
- **L1[1..] → L2 tables**: Kernel RAM identity-mapped as 2MB blocks covering the
  **full detected RAM** `[ram_base, ram_end)` (`add_kernel_mappings`), so
  `phys_to_virt()` works for any PMM-allocated page during syscalls. These blocks
  are EL1-only (AP=0b00). The mmap VA allocator and the MAP_FIXED / lazy-fault
  guards skip this range, using the **dynamic** `mmu::kernel_va_end()` (=
  `round_up(ram_base + ram_size, 1GB)`), not a fixed constant — see "RAM > 2 GB".
  If a user `MAP_FIXED` lands in this range, the affected 2MB block is shattered
  into 4KB L3 page entries preserving the identity mapping.

## RAM > 2 GB: the kernel/user VA split and the identity-map extent

Historically the kernel assumed RAM ≤ 2 GB. Two constants were hardcoded to a
2 GB machine, so booting with `MEMORY=3584M`/`4096M`/`6144M` made user programs
(notably `rustc`) crash. This is now fixed; the split tracks detected RAM.

### The two bugs

1. **EL0 user-VA collision (the `KERNEL_VA_END` constant).**
   `add_kernel_mappings()` identity-maps the *full* detected RAM `[ram_base, ram_end)`
   into every user address space as **EL1-only** 2MB blocks. But the user-VA guards
   (`alloc_mmap`, the MAP_FIXED overlap guard, the lazy-fault guard, the fork copy
   bounds) all used `KERNEL_VA_END = 0xC000_0000` (3 GB). Crucially, `alloc_mmap`
   *jumps* a large allocation to `KERNEL_VA_END`, so the dynamic linker's reservation
   for the big `rustc` binary landed at ~3 GB. With RAM ≤ 2 GB that is just *above*
   the identity map (safe); with RAM > 2 GB it is *inside* it, so an EL0 access hits
   an EL1-only block → permission fault → `SIGSEGV` (seen deterministically at
   `FAR=0xfecb2bf8`).

2. **EL1 kernel-context fault (the 3 GB boot map).**
   `src/boot.rs` statically maps only `[0, 3GB)`. The PMM, however, allocates frames
   across all RAM, and the kernel zeroes a frame via `phys_to_virt` (VA == PA) while
   the boot table may be the active TTBR0 (e.g. the deactivate→swap window in
   `replace_image`). On a > 2 GB machine a frame at PA ≥ 3 GB then faults in kernel
   mode (`EC=0x25` translation fault, `FAR=0xc1573000`), killing `clang`/`ld` during
   exec so the compile never links.

### The fix

- **`mmu::kernel_va_end()`** = `round_up(ram_base + ram_size, 1GB)` replaces the
  `KERNEL_VA_END` constant in all user-VA guards (`process/types.rs` `alloc_mmap`,
  `syscall/mem.rs` MAP_FIXED guard, `exceptions.rs` fault guard, `process/mod.rs`
  fork copy). `mmu::ram_base()` / `ram_end()` expose the raw bounds. The const
  `ProcessMemory::KERNEL_VA_END = 0xC000_0000` is kept only as a pre-init fallback
  (host unit tests, where RAM size is unknown).
- **`mmu::extend_boot_ram_identity_map()`** (from `mmu::init`) extends the boot L1
  table with 1GB `NORMAL` blocks for `[3GB, ram_end)`, so kernel-context access to
  any valid frame works regardless of which TTBR0 is active.

Net effect: physical RAM is fully usable. User VAs are simply placed above the
(now larger) identity map; the bump allocator / linker reservation jump to
`kernel_va_end()`. Free RAM scales with `MEMORY` (e.g. a `rustc hello.rs` compile
goes from ~127 MB free at 2 GB to ~3.9 GB free at 6 GB).

### Self-tests (boot suite, `src/tests.rs`)

- `boot_map_covers_full_ram` — walks the boot L1 and asserts every 1GB entry over
  `[ram_base, ram_end)` is valid (would have caught bug #2). Passes at any size.
- `mmap_fixed_kernel_va_guard`, `lazy_fault_kernel_va_guard`,
  `fork_mmap_skips_kernel_va`, `alloc_mmap_skips_kernel_va_hole` — derive their
  expected boundary from `kernel_va_end()` so they are RAM-size-agnostic.
- The PTE-walk scratch tests (`map_127_pages…`) moved their scratch VA to the
  256 GB range (above any RAM identity map) so they don't collide with the
  extended boot map.

> Verified across `MEMORY` = 256M/512M/1024M (tcc `hello.c`) and
> 2048M/3584M/4096M/6144M (`rustc hello.rs`): all boot clean and the program
> compiles, links, and prints `Hello from Akuma!`. Use `scripts/test_memory_split.py`
> to re-run the matrix.

## Configuration Files

- **RAM size**: Set via `MEMORY` environment variable (e.g., `MEMORY=1024M cargo run --release`)
- **Memory layout**: `src/main.rs` (kernel_main function)
- **Linker script**: `linker.ld` (kernel load address 0x40200000; also derives the
  boot-stack reservation symbols `STACK_BOTTOM`/`STACK_TOP`/`IMAGE_RESERVE`)
- **Page tables**: `src/boot.rs` and `src/mmu.rs`
- **Boot script**: `scripts/cargo_runner.sh` (invoked by `cargo run`)

## DTB Detection

The kernel reads RAM size from the Device Tree Blob (DTB) passed by QEMU:

1. QEMU passes DTB address in `x0` register (via ARM64 Image header protocol)
2. Kernel parses DTB to get memory base and size
3. Fallback: 256MB if DTB detection fails

```rust
// In src/main.rs detect_memory()
const DEFAULT_RAM_SIZE: usize = 256 * 1024 * 1024; // Fallback if DTB fails
```

With the ARM64 Image header, DTB detection is reliable and the kernel correctly
detects any RAM size configured via the `MEMORY` environment variable.
