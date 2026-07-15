# fw_cfg (QEMU firmware config)

Legacy selector+data MMIO driver for QEMU's `fw_cfg` device, plus a DMA path
for writes. Source: `src/fw_cfg.rs`. Physical MMIO at `0x0902_0000`, accessed
via the remapped VA `akuma_exec::mmu::DEV_FW_CFG_VA`.

> **Stability: A (stable).** Dormant since Mar 3 2026
> (`try to solve the device info mapping bug`); no functional changes since.

## What it's actually used for

There is exactly one consumer in the kernel: `src/ramfb.rs`, the QEMU ramfb
(RAM-based framebuffer) driver. This is **not** a boot-argument/cmdline
parsing path — `fw_cfg.rs` provides two primitives (file lookup, DMA write)
and `ramfb.rs` is the only caller of either. The `sc-framebuffer` Cargo
feature (see [`config-flags.md`](../config-flags.md)) gates the framebuffer
subsystem this feeds; `fw_cfg.rs` itself is unconditional (no feature gate
of its own).

## Selector/data interface (reads)

`select(key: u16)` (`:39-44`) writes a big-endian `u16` to the selector
register (`FW_CFG_SELECTOR`, base `+0x08`); subsequent reads from
`FW_CFG_DATA` (base `+0x00`) return bytes from the selected entry one at a
time (`read_bytes`, `:47-53`). `read_be_u32()` (`:56-60`) is the common
4-byte-big-endian-field helper built on top.

`find_file(name: &str) -> Option<(u16, u32)>` (`:65-106`) selects the
well-known file-directory entry (`FW_CFG_FILE_DIR = 0x0019`), reads the
big-endian entry count, then walks 64-byte directory entries
(`size(4) + select(2) + reserved(2) + name(56)`, all big-endian) looking for
a name match — returning that entry's `(selector, size)`. This is the lookup
`ramfb.rs` uses to find `etc/ramfb`.

## DMA interface (writes)

The data register is read-only for most entries, so writes go through the
DMA register (`FW_CFG_DMA`, base `+0x10`) instead.
`write_entry(selector, data)` (`:115-141`, `unsafe fn`) builds an
`FWCfgDmaAccess` descriptor (`control`, `len`, `addr`, all big-endian, with
`SELECT | WRITE` control bits plus the selector packed into the top 16 bits
of `control`), writes the descriptor's own physical address to `FW_CFG_DMA`,
then spin-waits on `control` reading back zero (success) or the `ERROR` bit
(failure). This is what `ramfb.rs` uses to hand QEMU the ramfb configuration
structure (framebuffer address, format, dimensions, stride).

## Background

No archive doc is specifically about `fw_cfg.rs` — the closest adjacent
history is in `docs/archive/DEVICE_MMIO_VA_CONFLICT.md` and
`docs/archive/MEMORY_LAYOUT.md`, which cover the VA-remapping scheme
(`DEV_FW_CFG_VA` and its siblings) this driver's base address depends on,
and `docs/archive/DOOM.md`, which documents `ramfb.rs`'s framebuffer
consumer (now removed; ramfb itself remains).
