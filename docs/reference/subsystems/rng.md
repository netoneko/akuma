# RNG (entropy source)

Hand-rolled VirtIO RNG (`virtio-rng`) driver — the kernel's actual entropy
source. Source: `src/rng.rs`. For the `/dev/urandom`/`/dev/random` device
nodes that consume this (`FileDescriptor::DevUrandom`), see
[`vfs.md`](vfs.md) "File descriptors" — that's a separate layer and is not
duplicated here.

> **Stability: B (watch).** The legacy-to-modern transport rewrite
> (Jun 6 2026) is a real protocol change, not a bug fix, and has only ~5
> weeks of dormancy behind it as of this writing — not yet the 2+ months
> this doc's grading bar wants for an A. No churn since (`fix clippy`,
> Jun 19, was repo-wide and mechanical).

## What it is

A minimal, standalone VirtIO MMIO driver (`:1-11`) — not the `virtio-drivers`
crate, which doesn't expose an RNG device. It drives the **modern (version
2)** MMIO transport only. The legacy (v1) path existed until June 2026 and
was removed once QEMU's `virtio-mmio.force-legacy` flag was dropped; `new()`
now `assert!(version == 2, ...)` (`:218-222`) and panics loudly at init
rather than silently limping onto a legacy layout — see Background below for
why that assert exists.

## Device scan and init

`init()` (`:506-544`) scans the 8 well-known QEMU virt-machine VirtIO MMIO
slots (`VIRTIO_MMIO_ADDRS`, `:26-35`, physical base + `0x200` stride) for a
device whose `VIRTIO_MMIO_DEVICE_ID` reads `4` (`VIRTIO_DEVICE_ID_RNG`).
`VirtioRngDevice::new(base_addr)` (`:205-396`) does the full modern VirtIO
handshake: magic-value check, reset, `ACKNOWLEDGE` → `DRIVER` → feature
negotiation (only `VIRTIO_F_VERSION_1`, bit 32, is required — the RNG device
needs no feature bits of its own) → `FEATURES_OK` → program a single
split-virtqueue (desc/avail/used, `QUEUE_SIZE = 2`) with three independent
64-bit physical addresses → `DRIVER_OK`. `init()` immediately test-reads 8
bytes from the first device found; a failed test read moves on to the next
MMIO slot rather than committing to a broken device.

## Reading entropy

`fill_bytes(buf: &mut [u8])` (`:555-559`) locks the global
`RNG_DEVICE: Spinlock<Option<VirtioRngDevice>>` and calls
`read_bytes` (`:399-481`), which drives the virtqueue in ≤256-byte chunks
(the pre-allocated DMA buffer size): build one write-only descriptor pointing
at the buffer, publish it on the avail ring, kick `QUEUE_NOTIFY`, then
spin-wait (`core::hint::spin_loop()`, capped at 10,000,000 attempts) on the
used-ring index advancing. This is a synchronous, blocking call — there is
no IRQ-driven completion path for this device.

## Background

- `docs/archive/VIRTIO_MMIO_LEGACY_TO_MODERN.md` — the Jun 2026 migration
  that rewrote this file for the v2-only transport; explains the
  `force-legacy` QEMU flag gotcha and why the assert fails loud instead of
  falling back.
- `docs/archive/DEV_RANDOM.md` — the `/dev/urandom`/`/dev/random` device-node
  layer built on top of this driver (see [`vfs.md`](vfs.md) instead for the
  current-state version of that layer).
