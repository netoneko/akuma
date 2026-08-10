# Debug boot hangs

Symptom-driven debugging when the kernel hangs or crashes during boot, before
SSH is reachable.

> **Stability of this area: B (watch).** Lower churn than memory, but boot
> math (image size, stack reserve, DTB placement) still bit in June. Most
> boot hangs are region-boundary math errors — see the table.

For architecture, see [`../reference/subsystems/boot.md`](../reference/subsystems/boot.md).

## Where it hangs → cause → fix

| Where it hangs / crashes | Cause | Status | Fix |
|---|---|---|---|
| QEMU aborts pre-kernel: `Not enough space for DTB after kernel/initrd` | DTB didn't fit ≤ 4 MB (kernel @ RAM_BASE+2 MB) | FIXED | ARM64 Image `text_offset`=1 MB → kernel @ `0x40100000`, DTB @ `0x40200000` |
| `!!! FATAL: Kernel binary overlaps with boot stack !!!` (`src/main.rs:568`) | Kernel image grew past `STACK_BOTTOM` | FIXED | `STACK_BOTTOM`/`IMAGE_RESERVE` linker-derived from `_kernel_phys_end`; runtime + linker ASSERT |
| `!!! FATAL: kernel memory layout invalid (overlap / out of bounds)` (`src/main.rs:642`) | Layout guard trips (0 user pages, heap_start < BOOT_STACK_TOP) | FIXED (guard) | Fix the constant; at 64 MB code+stack is 11 MB |
| Hangs after `[ELF] Stack: …` (no further output) | PMM/heap lock deadlock or silent crash during `alloc_and_map` | FIXED | PMM + allocator + PROCESS_TABLE all IRQ-guarded |
| `(isv)` assertion in QEMU HVF (`hvf.c:1883`) | GICv2 MMIO under GICv3; post-indexed MMIO store; CNTP timer trapped; IC IVAU on unmapped user VA | FIXED | GICv3 driver default; inline-asm `mmio_r32/w32` (ISV=1); unified virtual timer (CNTV); IC IVAU via kernel alias |
| `EC=0x0 ISS=0x0 ELR=0x402xxxxx` (zeros / `udf #0`) at boot | Kernel binary not fully loaded (size > Code+Stack, or stale build) | WORKAROUND | `cargo clean && cargo build`; raise `MEMORY=`; `rust-size` check |
| `!!! MEMORY/ASYNC/THREADING TESTS FAILED - HALTING !!!` (`src/main.rs:921/929/976`) | Boot self-test regression | BY DESIGN | Fix the regression; or `DISABLE_ALL_TESTS=true` / `no-tests` feature |
| `[TESTS] low-mem (N MB <= 32 MB): skipping boot self-test suite` | Heuristic skip so small RAM boots to SSH | BY DESIGN | `config::LOW_MEM_TEST_SKIP_MB`=32 |
| `yield_now`-spin hang after a self-test at `MEMORY≥8G` | Test hardcoded ~7.5 GB scratch VA, landed in extended identity map | FIXED | scratch VAs moved to 260+ GB (`0x41_0000_0000`) |
| Boot stack overwritten at low RAM (silent) | `code_and_stack = max(ram/16, 8MB)` forgot the KERNEL_BASE offset → heap contained boot stack | FIXED | `code_and_stack` covers `BOOT_STACK_TOP + 1 MB guard` |

## Boot verification markers (successful boot)

```
Akuma Kernel starting...
Kernel binary: NNNN KB
[Memory] Detected from DTB: base=0x40000000, size=NNN MB
=== Memory Layout === ...
Allocator initialized (talc mode)
Initializing PMM...
PMM initialized, allocator switched to page mode
Initializing MMU...
Initializing exec subsystem...
GIC initialized
Timer initialized
Initializing threading...
========== Memory Tests ==========     (unless skipped)
--- Filesystem Initialization ---
[FS] Filesystem mounted successfully
[Main] sshd started (tid=8)            ← boot-to-SSH readiness (or [herd] Started sshd)
```

Wait on `"sshd started|Started sshd"` (the `[SSH Server] Listening` marker was
the in-kernel server's and no longer exists). Never wait on
the QEMU process (it runs forever).

## Boot-time debug knobs

| Knob | How | Effect |
|---|---|---|
| Accelerator | `HVF=0` | Force TCG (deterministic; faithful gdbstub PC; HVF misreports PC as exception-vector entry) |
| GDB stub | `GDB=1` (`:1234`); `GDB_WAIT=1` waits at entry | `lldb … -o "gdb-remote 1234"` |
| RAM | `MEMORY=NNN` | Sizes all regions; default 256 MB |
| Tests off | `DISABLE_ALL_TESTS=true` / `no-tests` / RAM ≤ 32 MB | Skip self-tests |
| GIC version | runner passes `-machine virt,gic-version=3`; `gic-v2` feature = TCG fallback | HVF requires GICv3 |

## Background

- `archive/BOOT_STACK_BUG.md`, `archive/DYNAMIC_DTB.md`, `archive/QEMU_HVF_ISV_BUG.md`.
- `archive/DEVICE_MMIO_VA_CONFLICT.md`, `archive/IDENTITY_MAPPING_DEPENDENCIES.md`.
