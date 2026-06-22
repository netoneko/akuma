# Phase 3 — Kernel `rump` feature: raw L2 packet device

Status: **implemented & verified** (kernel side). This is the in-kernel half of
the rump port — the raw Ethernet path a userspace rump `virtif` will drive. It
does **not** yet include the userspace rump libraries (Phases 0/1/2/4) or the
`box --net` payload (Phase 5).

This document records what was built, how it is wired, how it was verified, and
the deliberate limitations to revisit in later phases. See
[IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) §6 Phase 3 for the original spec
and [DEV_ZERO.md](DEV_ZERO.md) for the kernel prerequisite that preceded this.

---

## What it does

Behind a new kernel Cargo feature `rump`, Akuma binds a **second** virtio-net
device (NIC1) to a raw L2 packet path and exposes it as a **`/dev/net/tap0`**
character device. `read()`/`write()` move whole Ethernet frames; a no-op
`TUNSETIFF` ioctl lets rump's stock Linux `virtif` backend bind it without
source changes. NIC0 stays owned by the native smoltcp stack — the two stacks
have clean L2 isolation (the plan's §4 option A, *dedicated second NIC*).

```
 QEMU NIC0  virtio-mmio-bus.0 ─► smoltcp ─► AF_INET sockets        (native, unchanged)
 QEMU NIC1  virtio-mmio-bus.4 ─► rump_tap ─► /dev/net/tap0         (NEW, feature "rump")
                                              read()/write() raw Ethernet frames
                                              TUNSETIFF → no-op success
```

---

## Feature gating — release only

`rump` is included in the **`default`** feature set, so a normal
`cargo build --release` carries it. The constrained profiles build with
`--no-default-features` (`scripts/build_size.sh`, `build_extreme_size.sh`) and
therefore **omit** it — rump targets roomy VMs (≥256 MB), not the 4 MB extreme
floor (plan §7 risk 6). This matches the maintainer's instruction: *only include
this feature in release*.

- `Cargo.toml`: `rump = ["akuma-net/rump"]`, listed in `default`.
- `crates/akuma-net/Cargo.toml`: `rump = []` (adds `crate::rump_tap`).
- No `build.rs` change needed — `#[cfg(feature = "rump")]` is a first-class Cargo
  feature cfg (unlike the `extreme`/`size` *profile* discriminators).

The second NIC itself only exists when the QEMU runner is started with
`RUMP_NIC=1`. Without it, `rump_tap::init` finds no second virtio-net, logs a
notice, and `/dev/net/tap0` returns `ENODEV`. **The default boot is unaffected.**

---

## Files touched

| File | Change |
|------|--------|
| `Cargo.toml` | `rump` feature, added to `default` |
| `crates/akuma-net/Cargo.toml` | `rump = []` feature |
| `crates/akuma-net/src/lib.rs` | `#[cfg(feature="rump")] pub mod rump_tap;` |
| `crates/akuma-net/src/rump_tap.rs` | **new** — NIC1 bind + raw frame send/recv |
| `crates/akuma-exec/src/process/types.rs` | `FileDescriptor::Tap` variant (unconditional) |
| `src/vfs/proc.rs` | `Tap → "/dev/net/tap0"` name arm |
| `src/syscall/fs.rs` | openat `/dev/net/tap0`, read (frame→buf), write (buf→frame), fstat (char dev) |
| `src/syscall/term.rs` | `TUNSETIFF` no-op ioctl for Tap fds |
| `src/main.rs` | call `rump_tap::init(&mmio_addrs)` after native net init |
| `src/process_tests.rs` | `test_rump_tap` self-test in `run_network_tests` |
| `scripts/cargo_runner.sh` | `RUMP_NIC=1` → second `-netdev user` + virtio-net on bus.4 |

`FileDescriptor::Tap` is **unconditional** (always in the enum) so exhaustive
matches compile in non-rump builds; only the code that *constructs* and *acts on*
it (in `fs.rs`/`term.rs`/`main.rs`) is `#[cfg(feature = "rump")]`.

---

## How the raw path works

`crates/akuma-net/src/rump_tap.rs` reuses the same `VirtIONetRaw` plumbing as
`smoltcp_net.rs`, but as a standalone global instead of a smoltcp `Device`:

- **`init(mmio_addrs)`** — scans the virtio-mmio slots, **skips the first**
  virtio-net (smoltcp owns NIC0) and **claims the second**. Returns the NIC1 MAC,
  or `Err` if there is no second NIC. Independent of smoltcp init order (it only
  relies on scan order).
- **`read_frame(buf) -> Option<usize>`** — the two-phase VirtIO receive
  (`receive_begin` → `poll_receive` → `receive_complete`), copying the frame
  (past the virtio-net header) into `buf`. `None` ⇒ no frame ready ⇒ caller
  returns `EAGAIN`.
- **`write_frame(frame) -> Result<usize>`** — `VirtIONetRaw::send`, which prepends
  the virtio-net header internally, so `frame` is the bare Ethernet frame.

The device + DMA buffers live behind a `Spinlock<Option<RumpTapNic>>`; a separate
`AtomicBool` exposes `is_ready()`.

### Syscall surface (`/dev/net/tap0`)
- `openat` → `FileDescriptor::Tap` (only if `rump_tap::is_ready()`, else `ENODEV`).
- `read` → one frame, or `EAGAIN` when none queued.
- `write` → one frame (Ethernet frames are < 2 KB, well under the write loop's
  64 KB chunk, so a frame is never split).
- `fstat` → char device, `rdev = makedev(10, 200)` (Linux TUN/TAP misc node).
- `ioctl(TUNSETIFF)` → no-op success on a Tap fd, `ENOTTY` otherwise.

---

## Verification

Built clean in both configurations:
- `cargo build --release` (rump **on**) — clean.
- `cargo check --release --no-default-features --features "sc-*"` (rump **off**) —
  clean (the rump-off code path compiles; the bare `--no-default-features`
  msgqueue errors are pre-existing and unrelated to this work).

Booted under QEMU/HVF with `MEMORY=1024M RUMP_NIC=1`:

```
[rump] /dev/net/tap0 bound to NIC1, MAC 52:54:00:12:34:57
--- Process Network Tests ---
[Test] rump_tap PASSED
[Test] dev_zero PASSED
```

`test_rump_tap` opens `/dev/net/tap0`, writes a crafted 60-byte broadcast ARP
frame (accepted in full), and reads once (`EAGAIN`/frame — both fine), all via
the real `handle_syscall` path with `BYPASS_VALIDATION`. It is registered in
`run_network_tests()` (which runs **after** network init binds NIC1) — not
`run_all_tests()`, which precedes init and would always see `is_ready() == false`.

> **Note on memory size:** at 256 MB the boot self-test suite panics in the
> *pre-existing, unrelated* `test_mmap_file_oom` (it mmaps a 507 MB `/models`
> file expecting a SIGSEGV that doesn't occur on this branch's disk state). That
> test only runs when a `/models` file is larger than RAM, so booting at
> ≥512 MB skips it and lets the boot reach the network tests. This is not caused
> by the rump change (it reproduces without `RUMP_NIC`).

---

## Deliberate limitations (revisit in later phases)

1. **Non-blocking read only.** `read()` returns `EAGAIN` when no frame is queued
   rather than blocking on an RX interrupt/waker. A rump virtif typically runs a
   dedicated receive thread; if it busy-spins, wire NIC1's RX IRQ to a per-fd
   waker (mirror the socket/eventfd blocking-read pattern). Sufficient for the
   Phase 3 exit test; revisit when integrating the real virtif (Phase 4).
2. **No `poll`/`epoll` readiness** for the tap fd yet (same reason).
3. **Single tap.** Only `/dev/net/tap0` / one NIC1. Multiple boxes each with
   their own stack (the cluster vision) will need N taps / NICs.
4. **`TUNGETIFF` and the rest of the TUN/TAP ioctl surface** are not implemented —
   only the `TUNSETIFF` no-op rump's virtif needs to bind. Add more if a real
   virtif build demands them (Phase 4).
5. **NIC1 bus slot is `.4`** (avoiding sound's `.3`); within the kernel's 8-slot
   virtio-mmio scan. If more devices are added, keep slots distinct.

---

## Next (Phase 4+)

The kernel now offers everything a rump `virtif` needs: a frame-granular
`/dev/net/tap0` with the TUN/TAP-shaped ABI. The remaining work is userspace:
cross-build `librump*` for `aarch64-linux-musl` (Phases 0/1), the Rust `rumpuser`
staticlib (Phase 2), bind rump's Linux virtif backend to `/dev/net/tap0`
(Phase 4), then the `rump-net` box payload + `box open --net` (Phase 5) and the
DHCP + curl milestone (Phase 6).
