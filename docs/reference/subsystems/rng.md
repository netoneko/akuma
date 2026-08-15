# RNG (entropy source)

Hand-rolled VirtIO RNG (`virtio-rng`) driver — the kernel's actual entropy
source. Source: **`crates/akuma-virtio/src/rng.rs`** (was `src/rng.rs` until the
Phase 3 driver consolidation). For the `/dev/urandom`/`/dev/random` device
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

## The device is untrusted input

This is the tree's **only** hand-rolled virtqueue — everything else
(`block.rs`, `audio.rs`, `smoltcp_net.rs`, `rump_tap.rs`) goes through
`virtio-drivers`, which has no RNG device. So the guarantees that library
provides have to be written out here, and all three are load-bearing because
whatever comes out of this path goes straight to userspace via `getrandom`:

| guard | why |
|---|---|
| `used_elem.id` must equal the descriptor we published | the device could complete a chain we never sent |
| `completion_copy_len(used_elem.len, to_read)` clamps the **device-reported length** to what the descriptor offered — *not* to the caller's remaining space | the source is a fixed 256-byte staging allocation. Clamping against the caller's space lets a device reporting `len > to_read` over-read that allocation and hand the spill to userspace |
| `copy_len == 0` is an error, not a retry | a device completing without writing leaves `bytes_read` unadvanced, so the outer loop reissues the same request forever |

Ordering: `VirtqAvail`/`VirtqUsed`'s `idx`/`flags`/`*_event` are `AtomicU16`, and
the poll loop does `idx.load(Ordering::Acquire)`. Observing the new `idx` is what
makes `used.ring[…]` and the DMA'd staging buffer valid to read — `read_volatile`
would constrain only the compiler, not the CPU's load/load reordering. `ring[]`
stays a plain read, covered by that acquire, exactly as `virtio-drivers` models
its own rings.

`UNSAFE_AUDIT.md` §4 P2(e) analyses the first and last of these as **open
defects**; they were fixed, and that section now carries a status header saying
so. Read it for the reasoning, not for the status.

## Tests

`completion_copy_len` and `calc_queue_layout` are split out as pure functions
purely so they can be host-tested — the rest of the driver needs a device:

```bash
cargo test -p akuma-virtio --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

Three tests, added 2026-08-13: the length clamp against a lying device
(`u32::MAX`, `4096`), an honest one, a short final request and zero; and that the
three DMA rings stay disjoint and aligned across queue sizes 1…1024, with the
used ring's deliberate page-over-alignment pinned so relaxing it to the spec's
4 bytes has to be a visible decision.

Before that, `akuma-virtio` had **no tests at all** across ~1,470 lines — the
only crate in the workspace with none, and the one holding this virtqueue. A
one-word clamp with no test is one careless edit from being a heap over-read
into `getrandom` output.

## Background

- `docs/archive/VIRTIO_MMIO_LEGACY_TO_MODERN.md` — the Jun 2026 migration
  that rewrote this file for the v2-only transport; explains the
  `force-legacy` QEMU flag gotcha and why the assert fails loud instead of
  falling back.
- `docs/archive/DEV_RANDOM.md` — the `/dev/urandom`/`/dev/random` device-node
  layer built on top of this driver (see [`vfs.md`](vfs.md) instead for the
  current-state version of that layer).
- [`../../archive/UNSAFE_AUDIT.md`](../../archive/UNSAFE_AUDIT.md) §4 P2(e) —
  the analysis of this virtqueue's two defects. Both fixed; the section is kept
  for the reasoning and carries a status header.
- [`../../archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](../../archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
  §6.2 — why this crate had zero tests and what closing that turned up.
