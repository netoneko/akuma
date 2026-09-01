# Boot

Current-state architecture for the boot sequence: entry, MMU init, DTB parse,
bringup.

> **Stability: B (watch).** Boot math (image size, stack reserve, DTB
> placement, HVF quirks) still bit through June. The layout guard
> (`!!! FATAL ...`) is the tripwire — if it fires, a region constant is wrong.

For debugging, see [`../../runbooks/debug-boot-hang.md`](../../runbooks/debug-boot-hang.md).

## Boot sequence (concrete stages)

1. **QEMU load.** ARM64 Image header (magic `ARM\x64`@0x38, `text_offset`=1 MB,
   `image_size`=`IMAGE_RESERVE`). QEMU loads flat binary @ `0x40100000`, places
   DTB, sets `x0`=DTB ptr.
2. **`_boot` (`src/boot.rs:61`).** Save `x0`→`boot_x0_at_entry`; enable FPU
   (`cpacr_el1`); `mov sp, =STACK_TOP`; zero BSS.
3. **`setup_boot_page_tables`.** 6 BSS pages: L0 (identity), L1 (device/RAM
   1 GB blocks), L0[1]→L1→L2→L3 device MMIO chain (`0x80_0000_0000+`).
4. **`configure_mmu_regs`.** MAIR (device/normal attrs), TCR (T0SZ=T1SZ=16,
   48-bit VA, IPS=5), TTBR0/1, `tlbi vmalle1`.
5. **Enable MMU.** SCTLR `M|C|I` + DZE/UCT/UCI; `isb`; `bl rust_start`.
6. **`rust_start` → `kernel_main(dtb_ptr)`.** `detect_memory` parses FDT
   `/memory`; fallback 256 MB @ `0x40000000`.
7. **Memory init.** layout guard → `allocator::init` (talc) → `pmm::init` →
   `allocator::mark_pmm_ready` (switch to page-backed growth) → reclaim
   pre-kernel region.
8. **`mmu::init`.** `extend_boot_ram_identity_map` (1 GB blocks for RAM > 2 GB).
9. **Exec subsystem**, GIC init, timer init (`CNTV_*`), (RNG, RTC).
10. **Threading init** (stack pool from PMM); enable 10 ms preemptive timer →
    SGI.
11. **Boot self-tests** (see below).
12. **Filesystem init** (block device → ext2 mount).
13. **SSH server / network** bring-up; main loop polls `memory_monitor()`.

## HVF vs TCG; GIC selection

- **Default:** runner auto-selects HVF on Darwin/arm64; `HVF=0` forces TCG.
  HVF ~70× (prompt) / ~100× (gen) faster for LLM; non-deterministic. Use TCG
  for deterministic crash repro.
- **GIC:** runner passes `-machine virt,gic-version=3` for both. `crates/akuma-gic`
  is the only driver (system-register CPU interface `ICC_*`, redistributor); the
  GICv2 MMIO backend and its `gic-v2` feature were deleted 2026-09-01 — it could
  not run under HVF and nothing enabled it (`archive/AKUMA_GIC_CONSOLIDATION.md`).
  Akuma uses SGI 0 + PPI 27/30 only.
- **HVF-specific fixes (all landed):** GICv3 driver; unified **virtual** timer
  `CNTV_*` (physical CNTP trapped under HVF); IC IVAU via kernel alias not
  unmapped user VA; inline-asm MMIO helpers forcing base-register addressing
  (post-indexed stores give ISV=0).
- **Single-core:** HVF gain is native instruction execution, not parallelism —
  keep `-t 1`.

## Boot self-test mechanism

- **Modules:** `src/tests.rs`, `src/async_tests.rs`, `src/daif_tests.rs`,
  `src/fs_tests.rs`, `src/sync_tests.rs`, `src/pthread_tests.rs`,
  `src/process_tests.rs`, `src/shell_tests.rs`.
- **Order** (`src/main.rs:911+`): DAIF → memory → async → (after fs mount) fs →
  threading → futex sync → pthread → process → shell → benchmarks.
- **Halt on regression:** returns false → `!!! … TESTS FAILED - HALTING !!!` →
  `halt()`. Threading halt bypassable via `IGNORE_THREADING_TESTS`.
- **Skip conditions:** `DISABLE_ALL_TESTS`, `no-tests` cfg, or RAM ≤
  `LOW_MEM_TEST_SKIP_MB`=32.
- **Notable regression guards:** `test_munmap_teardown_conserves_pmm`,
  `test_aliased_pa_not_double_freed` (`df_delta==0`),
  `test_unmap_and_free_respects_refcount`, `test_oom_user_page_reserve`,
  `test_heap_grow_backoff_plan`, `test_boot_stack_reservation_invariants`,
  `boot_map_covers_full_ram`.
- **Benchmarks** (`run_cow_benchmarks`, `run_benchmarks`) print grep-able
  `[BENCH]` lines and **never fail**.

## Reboot (`sc-reboot`, in `default` since 2026-08-25)

There is no warm in-kernel reboot (no kexec) — `reboot(2)`'s `Restart`/`PowerOff`
actions (`docs/reference/subsystems/syscalls/proc.md#reboot`) issue a real PSCI
`SYSTEM_RESET`/`SYSTEM_OFF`, so QEMU replays this entire boot sequence from
stage 1 exactly as it does for the very first boot — the design was chosen
specifically to avoid needing any of the machinery (SMP park/quiesce, cache/MMU
teardown, self-relocation) a software-driven warm reboot would. Because the
reset takes the whole machine (every box) down with it, the syscall is
restricted to box 0 (`caller_may_reboot`, `src/syscall/reboot.rs`) — a boxed
caller gets `EPERM`, the same restriction `mount`/`umount` already have.
`archive/AKUMA_BOOT_EXTRACTION.md` has the full reasoning.

## Background

- `archive/MEMORY_LAYOUT.md`, `archive/DYNAMIC_DTB.md`,
  `archive/QEMU_HVF_ISV_BUG.md`, `archive/IDENTITY_MAPPING_DEPENDENCIES.md`.
- `archive/AKUMA_BOOT_EXTRACTION.md` — the `reboot(2)` syscall this boot
  sequence gets replayed for, and the `KERNEL_DROPOFF` self-host iteration loop.
