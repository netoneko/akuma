# Dynamic DTB: ARM64 Image Header Boot Protocol

## Summary

The kernel uses QEMU's natively generated Device Tree Blob (DTB), which reflects the
actual machine configuration (including dynamic memory size set via `MEMORY` env var).
QEMU passes the DTB address in register `x0` when it recognizes the ARM64 Linux Image
header in the kernel binary.

## ARM64 Linux Image Header

QEMU only passes the DTB address in `x0` for kernels it recognizes as Linux. When
loading a flat binary via `-kernel`, QEMU checks for the `ARM\x64` magic at offset
0x38. If found AND `image_size != 0`, QEMU:

1. Uses `text_offset` to determine the load address
2. Passes the DTB address in `x0`

The kernel includes a 64-byte ARM64 Image header at the `_boot` entry point
(`src/boot.rs`):

```
Offset  Size  Field         Value
0x00    4     code0         b _boot_code (branch past header)
0x04    4     code1         0
0x08    8     text_offset   0 (QEMU adds 2MB, loads at 0x40200000)
0x10    8     image_size    0x300000 (3MB, non-zero to enable text_offset)
0x18    8     flags         0 (LE, unspecified page size)
0x20    8     res2          0
0x28    8     res3          0
0x30    8     res4          0
0x38    4     magic         0x644d5241 ("ARM\x64")
0x3C    4     res5          0
```

### Load Address Details

When QEMU sees the ARM64 magic and `image_size != 0`:
- If `text_offset < 4KB`, QEMU adds 2MB to avoid bootloader overlap
- Our `text_offset = 0` results in kernel loaded at `RAM_BASE + 2MB = 0x40200000`
- DTB is placed at `RAM_BASE = 0x40000000` (first 2MB of RAM)

This layout cleanly separates DTB from kernel code.

## Memory Layout

```
0x40000000  ┌─────────────────┐
            │   DTB (2MB)     │  ← QEMU places DTB here
0x40200000  ├─────────────────┤
            │   Kernel        │  ← _boot entry point
            │   (~2.2 MB)     │
0x40430000  ├─────────────────┤
            │   Boot Stack    │
0x40A00000  ├─────────────────┤
            │   Heap + PMM    │
            └─────────────────┘
```

## Build and Run

The kernel is built as an ELF and converted to a flat binary by `scripts/cargo_runner.sh`:

```bash
rust-objcopy -O binary "$ELF" "$BIN"
qemu-system-aarch64 ... -kernel "$BIN"
```

Set RAM size via the `MEMORY` environment variable:

```bash
MEMORY=1024M cargo run --release   # 1 GB RAM
MEMORY=512M cargo run --release    # 512 MB RAM
cargo run --release                # default: 256 MB
```

## DTB Discovery Fallback

If `x0` is zero (shouldn't happen with flat binary + ARM64 header), the kernel checks
`0x40000000` for the FDT magic (`0xd00dfeed`). If not found, it falls back to
conservative defaults (256 MB at 0x40000000).

## Boot Flow

```
cargo run --release
  → cargo_runner.sh converts ELF to flat binary
  → QEMU loads binary at 0x40200000 (sees ARM64 magic, applies 2MB offset)
  → QEMU generates DTB at 0x40000000
  → QEMU sets x0 = 0x48000000 (DTB address for 1GB config)
  → jumps to _boot (0x40200000)
    → _boot: b _boot_code (skip 64-byte header)
    → _boot_code: saves x0, zeros BSS, sets up page tables, enables MMU
      → rust_start(dtb_ptr) → kernel_main(dtb_ptr)
        → detect_memory(dtb_ptr) parses FDT /memory node
        → prints: "[Memory] Detected from DTB: base=0x40000000, size=1024 MB"
```

## Known issue: RAM under-detected at large `MEMORY` (RESOLVED, 2026-06-09)

> **RESOLVED — does not reproduce on current `main`.** A freshly-built
> `extreme-size` kernel under HVF at `MEMORY=2048` now detects the full 2048 MB
> across two deterministic boots (`size=2048 MB`, `Total RAM 2048 MB`, user
> pages 2045 MB, `PMM stats: 524288 total` = exactly 2 GB at 4 KB/page). The
> investigation below is kept for the record; see **Resolution** at the end for
> what changed and why the original "HVF hands a corrupt DTB" theory is wrong.

On the `extreme-size` kernel under HVF, the kernel detects **far less RAM than
QEMU was given** once `MEMORY` is large. Observed with `MEMORY=2048M`:

```
(QEMU launched with -m 2048M)
DTB ptr from boot (x0 arg): 0x48000000
[Memory] Detected from DTB: base=0x40000000, size=1048 MB   ← should be 2048 MB
User pages: 1045 MB (0x4024a000 - 0x81800000)
PMM stats: 268288 total ...                                  ← 268288 pages = 1048 MB
```

`-m 1024M` detects ~1024 MB; `-m 2048M` detects only ~1048 MB. The extra
gigabyte never reaches the PMM, so a "2 GB" run is really a ~1 GB run — and any
workload sized for the larger figure (e.g. `llama-server` + a 532 MB model)
exhausts the PMM and trips the OOM `brk #1` (see
`docs/NET_BOUNCE_OOM_KERNEL_ABORT.md`).

### What has been ruled out

This is **not** the parser, **not** the launch command, and **not** a RAM cap:

1. **We don't pass a DTB** — no `-dtb` flag, no `.dtb` in-repo; QEMU
   auto-generates it and passes the pointer in `x0`.
2. **QEMU's DTB is correct.** `qemu-system-aarch64 ... ,dumpdtb=...` with the full
   runner device set yields `/memory reg = <0x0 0x40000000 0x0 0x80000000>` =
   exactly 2048 MB.
3. **The `fdt = "0.1"` crate parses it correctly** — running the *same* parse on
   the host against QEMU's dumped DTB returns `size = Some(2147483648)` = 2048 MB.
4. **No clamp** sits between the `fdt` read and the `[Memory] Detected ...` print
   in `detect_memory()` (`src/main.rs`) — the printed value *is* the raw
   `region.size`. (`MEM_CALC_CLAMP_MB` clamps only the kernel's *own* reserve math,
   not `user_pages_size`.)

### Conclusion / suspect

The DTB bytes the kernel reads at runtime (`x0 = 0x48000000`) must differ from
QEMU's pristine DTB — the `fdt::from_ptr` header check still passes, but the
`/memory` size cell reads as `0x41800000` instead of `0x80000000`. Prime suspect:
QEMU parks the DTB at `0x48000000`, which is **inside the kernel's own user-page
region** (`0x4024a000 - 0x81800000`); something perturbs the size cell, or HVF
hands the guest a different DTB than the TCG `dumpdtb` path. (Note the
"Memory Layout" / load-address section above predates the `text_offset = 1 MB`
move to `0x40100000` and the observed `0x48000000` DTB placement — treat those
addresses as illustrative, not current.)

### Next diagnostic

Read the live DTB at `0x48000000` and diff against QEMU's pristine 2 GB DTB:
boot `MEMORY=2048 GDB=1 INSTANCE=1`, attach lldb to the gdbstub (`:1235`, see
`docs/` lldb+gdbstub notes), and dump the `/memory` node bytes right after boot
(before any OOM). That settles whether the size cell is corrupted in guest RAM
(→ fix DTB placement, e.g. via the boot header's `image_size` so QEMU stops
parking the DTB inside live RAM) or HVF is the source.

### Resolution (2026-06-09)

Re-tested on current `main` (HEAD includes the day's `extreme`/HVF/GICv3 work).
The `extreme-size` kernel under HVF at `MEMORY=2048` detects **2048 MB** on
every boot:

```
DTB ptr from boot (x0 arg): 0x48000000        ← same pointer as the buggy report
[Memory] Detected from DTB: base=0x40000000, size=2048 MB
Total RAM: 2048 MB at 0x40000000
User pages: 2045 MB (0x4024a000 - 0xc0000000)
PMM stats: 524288 total, 335 allocated, 523953 free   ← 524288 × 4 KB = exactly 2 GB
```

**The "HVF hands the guest a corrupt DTB" prime suspect is disproven.** The same
`x0 = 0x48000000` now yields a correct read, and the *release* kernel always read
that same pointer correctly (see the user's 2048 MB llama.cpp boot log). QEMU's
DTB at `0x48000000` is correct and readable from both profiles — the size cell is
not perturbed in guest RAM.

**What actually moved the numbers — per-build-size memory calc.** Commit
`7042485` ("some fixes for extreme profile") reworked the layout math into the
pure `compute_memory_layout` + `reserve_calc_ram` functions and added
`config::MEM_CALC_CLAMP_MB` (4 MiB on `extreme`, 0 elsewhere):

- **Before:** `heap_size = compute_heap_size(ram_size, …)` used the **raw** RAM
  size, so on a big box `extreme`'s reserves scaled with RAM (`heap ≈ ram/8`
  capped at 256 MB, `code_and_stack ≈ ram/16`). At 2 GB that buried hundreds of
  MB in kernel reserves that never reached the user-page pool — the downstream
  "the extra gigabyte never reaches the PMM" symptom.
- **After:** the reserve/heap math runs on `reserve_calc_ram(ram_size, 4 MiB)`
  while `user_pages_size` is carved from the **real** `ram_size`. The kernel
  still sees and maps all RAM; the surplus now flows to userspace.

**Caveat on the original symptom.** `detect_memory()` prints the *raw*
`region.size` and was **not** changed by `7042485`, so that commit alone does
not explain a *detect-print* reading `1048 MB`. If the original `[Memory]
Detected from DTB: size=1048 MB` line was transcribed accurately it points to an
even-older binary (e.g. pre-`text_offset = 1 MB` / `0x40100000` boot-layout
move, where DTB placement/overlap differed); that path is also fixed today. The
wrong read could not be reproduced on the current `extreme` build. If it ever
resurfaces, capture the live DTB bytes via the "Next diagnostic" above before
assuming the cause.

## Key Files

- `src/boot.rs` — ARM64 Image header and early boot assembly
- `src/main.rs` — `detect_memory()` and `scan_for_dtb()` functions
- `scripts/cargo_runner.sh` — ELF to flat binary conversion and QEMU launch (supports `GDB`/`GDB_WAIT`/`MEMORY`)
- `linker.ld` — Kernel linked at 0x40200000
