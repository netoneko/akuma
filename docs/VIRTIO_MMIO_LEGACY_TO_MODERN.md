# VirtIO MMIO: Legacy (v1) → Modern (v2) Migration

## Summary

The QEMU runner previously forced **all** virtio-mmio devices into legacy mode
via `-global virtio-mmio.force-legacy=true`. That flag existed for exactly one
reason: the hand-rolled RNG driver (`src/rng.rs`) only implemented the legacy
(version 1) MMIO register layout. The block and network drivers never needed it
— they go through the `virtio-drivers` crate's `MmioTransport`, which speaks
both v1 and v2 transparently.

This was a blocker for **virtio-sound**, which is a modern-only device (it has
no legacy layout) and refuses to initialize behind a legacy proxy.

The migration rewrote `src/rng.rs` to drive the modern (version 2) transport
and switched the QEMU runner to `force-legacy=false`. The legacy (v1) code path
has been removed; a non-v2 RNG device now panics at init with a descriptive
message rather than silently falling back.

### Gotcha: `force-legacy` defaults to **true**

QEMU's `virtio-mmio` proxy defaults `force-legacy` to `true` on the `virt`
machine. **Removing** the `-global virtio-mmio.force-legacy=true` line does *not*
select modern — the device still presents as version 1. You must set
`-global virtio-mmio.force-legacy=false` explicitly. (This bit us mid-migration:
with a dual v1/v2 driver and the flag merely removed, the driver was quietly
still using its legacy path; SSH/TLS/tcc all "passed" on v1. The truth only
surfaced once the v1 path was deleted and the v2-only driver panicked on the
still-v1 device.)

## Why force-legacy existed (root cause)

- `src/rng.rs` is a standalone driver because `virtio-drivers` 0.7 does not
  expose an RNG device. It hand-wrote the legacy register sequence and bailed
  on anything other than `VERSION == 1`.
- Networking depends on hardware RNG hard: `src/main.rs` wires
  `rng::fill_bytes(...).expect("RNG required for networking")`. RNG init failure
  is tolerated at boot (prints "Hardware RNG not available"), but the first
  TLS/SSH key draw would then **panic**.
- So the chain was: a modern-only device → must drop `force-legacy` → which
  breaks the legacy-only RNG → which panics networking. Hence the RNG driver had
  to learn v2 before the flag could go.

`block.rs` (`VirtIOBlk`) and `crates/akuma-net` (`VirtIONetRaw`) both use
`MmioTransport` and required no changes — the crate negotiates v1/v2 internally.

## Legacy (v1) vs Modern (v2): what the driver does

The split-virtqueue data structures (descriptor table, available ring, used
ring) are **identical** between the two transports; only device setup differs.
The table below records both for reference, but the driver now implements only
the **Modern (v2)** column — the v1 path was deleted.

| Step | Legacy (v1) — removed | Modern (v2) — current |
|------|-------------|-------------|
| Version gate | required `VERSION == 1` | requires `VERSION == 2`, else `panic!` |
| Feature negotiation | single 32-bit `DriverFeatures` word, ack none | windowed via `DeviceFeaturesSel`/`DriverFeaturesSel`; must ack `VIRTIO_F_VERSION_1` (bit 32 → word 1, bit 0) |
| Feature commit | none | set `FEATURES_OK` (status bit 8) and verify the device keeps it |
| Page size | write `GuestPageSize` (0x028) | not used |
| Queue address | one contiguous region via `QueueAlign` (0x03c) + `QueuePFN` (0x040) | three independent 64-bit physical addresses: `QueueDesc{Low,High}` (0x080/0x084), `QueueDriver{Low,High}` = avail (0x090/0x094), `QueueDevice{Low,High}` = used (0x0a0/0x0a4) |
| Queue enable | implied by PFN write | write `QueueReady` (0x044) = 1 |
| Final status | `ACKNOWLEDGE \| DRIVER \| DRIVER_OK` | `ACKNOWLEDGE \| DRIVER \| FEATURES_OK \| DRIVER_OK` |

### Queue allocation

The driver allocates a single page-aligned region with the descriptor table at
offset 0, the available ring immediately after, and the used ring at the next
page boundary. Modern only needs desc/avail/used at 16/2/4-byte alignment, which
the page boundary trivially exceeds; the three physical addresses are derived
from that one allocation and written to the address-pair registers. (The
page-boundary placement is a holdover from the legacy single-PFN window — it is
harmless over-alignment for modern.)

The data path (`read_bytes`: descriptor fill → available ring → `QueueNotify` →
poll used ring) is transport-independent and unchanged.

## Files changed

- `src/rng.rs` — modern-only (v2) transport: feature windowing,
  `VIRTIO_F_VERSION_1` ack, `FEATURES_OK` handshake, and split desc/avail/used
  address registers + `QueueReady`. A non-v2 device `panic!`s at init. Legacy
  (v1) register constants and code path removed.
- `scripts/cargo_runner.sh` — set `-global virtio-mmio.force-legacy=false`
  (must be explicit; the QEMU default is `true`).
- `src/process_tests.rs` — added `test_rng_entropy_live`: two non-empty
  `fill_bytes` calls that must succeed and differ, guarding the RNG path in the
  default (test-bearing) profile.

## Validation

All on genuine modern (`force-legacy=false`); the v2-only driver would `panic!`
on a v1 device, so a successful RNG init is itself proof of v2.

| Check | Result |
|-------|--------|
| Panic path: still-v1 device (flag merely removed → QEMU default v1) | kernel panics with "MMIO version 1 unsupported … force-legacy set?" — fail-fast confirmed |
| Release boot, `force-legacy=false` | RNG inits slot 2; `[Test] rng entropy-live PASSED`; SSH up |
| Extreme-size boot at 4 MB | RNG inits, no OOM/abort, SSH up |
| `tcc -static -B /usr/lib/tcc … hello.c` ×3 (4 MB) | 3/3 → `Hello, Akuma!` |
| RNG e2e — SSH handshake | login succeeds (Ed25519 KEX draws `fill_bytes`) |
| RNG e2e — TLS | `curl https://google.com/` completes the TLS handshake, returns `HTTP/1.0 301` |
| `meow -c "say hi"` (4 MB) | replies |

Pre-existing, transport-independent test failures (`PermissionDenied → EPERM`,
LFN `gguf` read, `stp_xzr_ec15`) are unrelated to this change. Block device init
(1024 MB, slot 1) and net both work on v2 via the crate's `MmioTransport`.

## Follow-ups

- **virtio-sound** (modern-only) is now unblocked: probe a free virtio-mmio slot
  for device id 25, build an `MmioTransport`, and drive the crate's `VirtIOSound`
  (`pcm_set_params` / `pcm_prepare` / `pcm_start` / `pcm_xfer`). Add
  `-audiodev …` + `-device virtio-sound-device,bus=virtio-mmio-bus.N` to the
  runner.
