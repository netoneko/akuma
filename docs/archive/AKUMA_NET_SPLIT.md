# Splitting `akuma-net`: survey, test coverage, and the `unsafe` audit

**Date: 2026-08-30.** Survey of the second-largest crate in the tree ahead of a
split. Three questions were asked: what is actually in it, what tests cover it,
and where does its `unsafe` live. The split itself is **not done** — §5 is the
plan. What *is* done is the `unsafe` work in §4, which was pulled forward
because it decides where one of the seams goes.

Companion to [`AKUMA_NET_ISSUES.md`](AKUMA_NET_ISSUES.md) (the performance
record for the same code) and [`UNIX_SOCKET_IMPROVEMENTS.md`](UNIX_SOCKET_IMPROVEMENTS.md)
(which produced `unix.rs`, the one part of this crate that is already shaped the
way the rest should be).

---

## 1. What is in the crate

At survey time 8,633 lines, second only to `akuma-exec` (28,388) — of which
1,951 were its own tests, so the production body was 6,682. **8,845 now**: §4
added `frames.rs`/`nic.rs` and tests, §1.2 deleted 504 lines.

| file | lines (at survey) | what | smoltcp-gated | `unsafe` |
|---|---|---|---|---|
| `smoltcp_net.rs` | 2,069 | virtio device wrapper, loopback ring, `poll()`, socket set, `TcpStream` | whole file | **22** |
| `socket.rs` | 1,791 | `SOCKET_TABLE`, bind/listen/accept/connect/send/recv, wait loop | 60 attrs | 0 |
| `unix_tests.rs` | 1,318 | 90 AF_UNIX tests | no | 0 |
| `unix.rs` | 1,158 | AF_UNIX state machine | no | 0 |
| `tests.rs` | 488 | 37 tests | — | 0 |
| `nicstat.rs` | 443 | `net-profile` counters | no | 0 |
| `virtio_rings.rs` | 400 | `net-noalloc` static RX/TX rings | yes | **16** |
| ~~`locks.rs` + `lock_tests.rs`~~ | ~~504~~ | ~~lock hierarchy + 8 tests~~ — **deleted, §1.2** | no | 0 |
| `rump_tap.rs` | 151 | raw L2 NIC for rump | no | **3** |
| `runtime.rs`, `dns.rs`, `lib.rs` | 305 | callback table, resolver, wiring | mixed | 0 |

### 1.1 The feature surface is not experiment cruft

An early draft of this survey claimed the crate carried "five measured-and-rejected
experiments as still-compiled paths". That was wrong in both directions and the
correction matters, because it inverts the argument for splitting.

`akuma-net` declares **11** features. The kernel's `default` set is
`smp-shared, smoltcp, sound, rump, fs-cache, sc-*, many-sessions, sc-reboot`,
and `smp-shared` itself expands to include `no-bkl-network` and `no-bkl-vfs`. So
**6 of the 11 are on in a stock `cargo build --release`**:

| feature | default build | status |
|---|---|---|
| `smoltcp` | **on** | the native stack |
| `rump` | **on** | |
| `many-sessions` | **on** | default since 2026-08-10 |
| `smp-shared` | **on** | |
| `no-bkl-network` | **on** | via `smp-shared` |
| `no-bkl-vfs` | **on** | via `smp-shared` |
| `small-sockets` | off | size knob, reached via `no-tests` — not an experiment |
| `net-profile` | off | measurement instrument, "never ship it" — working as intended |
| `net-noalloc` | off | **conditional**, not rejected |
| `net-waker-park` | off | **rejected** |
| `net-direct-waker` | off | **unmeasured** |

And the three `net-*` ones are three different things:

- **`net-waker-park`** is the only rejected one. Measured worse (parks
  3,918 → 2,565 but us/park 1,172 → 1,787, total parked time flat at ~4.59 s of a
  5 s window). Its own note says beating the default needs a scheduler "light
  sleep" state, not a net change.
- **`net-noalloc`** measured *mixed*: lock-hold halved (472 → 211 ms per 5 s
  window, tx wait 27.8 → 9.2 us/pkt) but HTTP p90 went 1,172 → 3,433 us. Parked
  with a documented use ("a pipelined workload (redis) where the lock-hold win
  should dominate"), not dead.
- **`net-direct-waker`** is marked UNMEASURED as of 2026-08-24.

**Consequence for the split.** The dominant `#[cfg]` in this crate is not an
experiment — it is `smoltcp`, which is *on* by default, and the 60 attributes in
`socket.rs` exist for exactly one reason: the rump-only devbox builds the crate
with `default-features = false` and still needs `socket`'s address vocabulary
and all of `unix.rs`. Those attributes are a build-decoupling problem being
solved with conditional compilation because the AF_UNIX state machine lives in
the same crate as the TCP/IP stack. That is an argument *for* extraction, and it
is a different argument from "shed dead paths".

---

### 1.2 504 lines that were deleted, not split (2026-08-30)

`locks.rs` (359) + `lock_tests.rs` (145) — 5.4% of the crate — are **gone**.

All 15 of their public symbols (`NETWORK_LOCK`, `SOCKET_TABLE_LOCK`,
`acquire_network_lock`, `get_lock_stats`, the `LOCK_LEVEL_*` constants, …) had
**zero references anywhere in the repo** outside the two files themselves. They
were Phase 1 scaffolding from
[`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md), written
2026-07-24 and marked "✅ COMPLETE", for a Phase 2 that took a different route:
`PreemptGuard` plus the existing `NETWORK`/`SOCKET_TABLE` spinlocks under
`no-bkl-network`.

They were also **not merely unused — they could not have worked**, which is why
this is a delete rather than a wire-up:

```rust
pub fn acquire_network_lock(holder_id: u32) {
    ...
    let _guard = NETWORK_LOCK.lock();   // drops at end of function
    mark_lock_held(LOCK_LEVEL_NETWORK);
}   // <- exclusion ends here
pub fn release_network_lock() {         // never touches NETWORK_LOCK
    mark_lock_released(LOCK_LEVEL_NETWORK);
    NETWORK_LOCK_HOLDER.store(LOCK_HOLDER_NONE, Ordering::Relaxed);
}
```

The guard dropped at function return, so the "lock" granted no mutual exclusion
at all; `HELD_LOCKS` was a single global `AtomicU32` rather than per-thread, so
two cores would corrupt each other's ordering bits under `smp-shared`; and
`LOCK_LEVEL_SOCKET` had no lock object behind it. A caller who trusted the doc
comments would have gotten silent data races. That analysis is not new — it is
[`REDIS_ROUND_TRIP_STAGE_TRACE.md`](REDIS_ROUND_TRIP_STAGE_TRACE.md) §2, which
quotes the code and is now its surviving record.

**One trap worth naming.** [`TRIM_FAT_DEAD_CODE.md`](TRIM_FAT_DEAD_CODE.md)
records a near-miss: someone almost deleted `lock_tests.rs` alone, and that
would have been wrong — it covered code that still existed. Deleting *both*
together does not hit that trap. The "the tests use it, so it isn't dead"
argument is circular once the tests are the only consumer; the question to ask
is whether the module is reachable from *production*, and this one never was.

Host tests went 149 → 138, which is exactly the 11 tests that covered only the
deleted code. `cargo build --release`, `--no-default-features`, and clippy are
all clean.

## 2. Test coverage

**138 host tests in-crate, running in 0.00 s** (149 after §4), plus 25 in the
already-extracted `akuma-net-yarn`.

```
cargo test -p akuma-net --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
→ 138 passed   (before §4)
→ 149 passed   (after: +8 FrameArena, +3 SocketHandle layout)
```

The distribution is extremely uneven, and the split falls along the same line:

- **`unix.rs` — near-total.** 90 tests across `addr`/`sock_type`/`framing`/
  `channels`/`names`/`listen`/`rendezvous`/`dgram`/`credentials`/`shutdown`/
  `lifecycle`. Every `pub fn` except `channel_mut`/`get_mut` is exercised. This
  is the model.
- **`locks.rs` — complete.** 8 tests covering hierarchy and stats.
- **`socket.rs` — only the pure predicates.** `connect_step`, `connect_outcome`,
  `tcp_recv_ready`, `tcp_reached_established`, `backlog_handle_is_live`,
  `bind_port_for` are well covered (visibly written after
  [`SOCKET_DELAYED_FIRST_BYTE_HANG.md`](SOCKET_DELAYED_FIRST_BYTE_HANG.md)).
  Everything touching `SOCKET_TABLE` — `alloc_socket`, `socket_accept`,
  `socket_connect`, `remove_socket`, `listener_refresh`, `wait_until`, the
  timeout/keepalive setters — has **zero** host coverage.
- **`smoltcp_net.rs` — effectively uncovered.** Only `NetSite` round-tripping,
  `network_holder_snapshot` and `purge_connecting`. The notable gaps each have
  an incident doc behind them: `is_loopback_frame` (pure byte-offset frame
  parsing), `LoopbackRing::push`/`pop`
  ([`LOOPBACK_RING_CONVERSION.md`](LOOPBACK_RING_CONVERSION.md)), and
  `grow_soft_cap`/`reclaim_pending_slots`
  ([`SOCKETSET_EXHAUSTION_FIX.md`](SOCKETSET_EXHAUSTION_FIX.md)).
- **`nicstat.rs`, `virtio_rings.rs`, `rump_tap.rs`, `dns.rs`, `runtime.rs` — zero.**

### 2.1 Where the untested half actually gets exercised

At VM cost, which is the point:

| tier | what |
|---|---|
| kernel boot suite | `src/network_tests.rs` (1: loopback TCP), `src/process_tests.rs` (~20: socket refcount, `SO_KEEPALIVE`, socketpair ×5, AF_UNIX ×11, socket-waker/epoll ×4), `src/rump_tests.rs` (6) |
| Python harnesses | `ssh_harness.py`, `sshd_concurrency_test.py`, `sshd_crash_hunt.py`, `epoll_suite.py`, `redis_stream_integrity.py` |
| benchmarks | `bench_nic_rtt.py`, `nicstat_breakdown.py`, `run_nic_ab.py`, `serial_httpd_ref.py` |
| acceptance | 11 (rump IRC), 13 (`cargo install` over the network) |

**The summary is: AF_UNIX costs a second to test, TCP costs a boot.** Every
extraction below is chosen to move something across that line.

---

## 3. Where the `unsafe` was

41 sites, all in three files. **Zero** in `unix.rs`, `socket.rs`, `locks.rs`,
`dns.rs`, `nicstat.rs`, `runtime.rs`, `lib.rs` — the grep hit in `runtime.rs` is
the word "unsafe" in a doc comment about spinlocks, not an `unsafe` block.

Four classes, which want four different answers:

| class | sites | answer |
|---|---|---|
| (a) `&mut` self-aliasing in `Device::receive` | 1 | **removed** — real UB, and the fix already existed 200 lines below |
| (b) `static mut` frame buffers | ~14 | **removed** — unchecked pointer arithmetic replaced by a bounds- and borrow-checked arena |
| (c) `virtio-drivers` `unsafe fn` API | ~12 | **eliminated at every in-tree call site** — see §4.3; this went further than planned |
| (d) `transmute::<SocketHandle, usize>` | 2 | **kept, now proved** — no safe route exists; the layout assumption is a checked one |
| (e) MMIO registers | 2 | left — already behind `akuma_primitives::mmio::MmioReg`, correctly documented |

### 3.1 Before and after

Counting `unsafe {` / `unsafe fn` / `unsafe impl`:

| file | before | after | |
|---|---|---|---|
| `smoltcp_net.rs` | 22 | **9** | 2 MMIO, 2 frame-offset, 1 discard, 2 token slices, 2 transmutes |
| `virtio_rings.rs` | 16 | **0** | the whole file is safe now |
| `rump_tap.rs` | 3 | 3 | unchanged — already the right shape |
| `frames.rs` | — | 5 | new: the arena's internals |
| `nic.rs` | — | 11 | new: every virtio-drivers call, in one place |
| **total** | **41** | **28** | |

The count matters less than the location: the three packet-path files went **38 → 12**, and 16 of the 28 are now in two files whose entire purpose is to hold them.

---

## 4. What landed (2026-08-30)

### 4.1 (a) The self-alias in `VirtioSmoltcpDevice::receive`

```rust
let rx = VirtioRxToken { buffer: unsafe { core::slice::from_raw_parts_mut(ptr, len) } };
let tx = VirtioTxToken { dev: unsafe { &mut *(&raw mut *self) } };   // <- two live &mut
```

Two live `&mut` to the same place — UB by the language's rules independently of
whether the device races them. The file's own comment claimed disjoint field
borrows "cannot express it", but `LoopbackAwareDevice::receive` 200 lines below
already solved exactly this by choosing the frame *before* building the tx
token.

The same fix works here for a reason worth recording: `take_rx_frame` returns a
**raw pointer**, and that pointer's provenance is the BSS static
(`RX_BUFFER` / `virtio_rings::RX_BUFS`), **not** `self`. So once it returns,
`self` is unborrowed and the frame pointer does not alias it:

```rust
let tx = VirtioTxToken { dev: self };
let rx = VirtioRxToken { buffer: unsafe { core::slice::from_raw_parts_mut(ptr, len) } };
```

Verified to compile with and without `net-noalloc`. The `#[allow(clippy::deref_addrof)]`
on the `impl` block and the comment justifying it both go with it.

### 4.2 (b) `FrameArena`: the `static mut` frame buffers

Five statics — `RX_BUFFER`, `LOOPBACK_BUFS`, `RX_BUFS`, `TX_BUFS`, `TX_DISCARD` —
each with a hand-written safety argument saying the same thing ("one device, one
owner, serialised by `NETWORK`"), reached through three accessor pairs
(`rx_buf`/`rx_frame`, `tx_buf`/`tx_frame`, `loopback_buf`) that each do
**unchecked** `.add(slot * LEN)`.

The unchecked arithmetic is the part that is not merely untidy. `loopback_buf`
and `rx_buf` are `unsafe fn` whose stated contract is `slot < RING`, and nothing
enforces it; a desynchronised slot index writes past the arena into whatever BSS
follows.

Replaced with one `FrameArena<SLOTS, LEN>` (`src/frames.rs`) offering:

- `slot_ptr(slot) -> Option<*mut [u8]>` — **safe**, bounds-checked. Forming a
  pointer is safe; dereferencing it is the caller's obligation, which is the
  distinction the pre-existing `rx_buffer()` comment already drew and this
  generalises.
- `with_slot(slot, f)` — **safe**, bounds- and borrow-checked scoped exclusive
  access via a per-slot flag. Returns `None` rather than aliasing.
- `lease(slot)` — a guard for the two borrows that must outlive the call (the
  `RxToken` handed up to smoltcp, and `LoopbackRing::pop`); `Drop` releases.

The borrow flag turns "two `&mut` to one slot" from UB into an observable
`None`, and costs one relaxed atomic per frame against an MMIO notify. Host
tests cover bounds rejection, double-borrow rejection, and release-on-drop —
the first host coverage this file has ever had.

### 4.3 (c) `Nic`: the virtio-drivers obligation, discharged

`receive_begin`, `receive_complete`, `transmit_begin` and `transmit_complete`
are `unsafe fn` upstream, and their contract crosses function boundaries:

> The buffer is **owned by the device** from `*_begin` until the matching
> `*_complete` for the same token. It must stay allocated, at a fixed address,
> and untouched by the driver for that whole window, and the completing call
> must be given the *same* buffer.

The plan was to *localize* this — one file, one stated contract, callers still
writing `unsafe`. It turned out to be eliminable, and the reason generalises, so
it is worth recording.

**No wrapper can discharge that obligation while the caller supplies the
buffer.** A safe signature taking `&mut [u8]` would accept a stack temporary and
let it die under live DMA. So the wrapper stops taking a buffer: `Nic`'s entry
points take a [`FrameArena`] slot instead, and each half of the contract becomes
a type-level fact.

| obligation | discharged by |
|---|---|
| stays allocated, fixed address | the arena is a `static` in BSS |
| untouched until completion | the `FrameLease` the caller holds for the device's whole ownership window |
| same buffer at completion | the lease *is* the buffer; completion consumes it |

`post_rx` / `complete_rx` / `submit_tx` / `complete_tx` therefore have safe
signatures that are not lies, the raw `unsafe` calls appear exactly once each in
`nic.rs`, and **`virtio_rings.rs` ends up with no `unsafe` at all** — down from
16.

Two details fell out of writing it that were not in the plan:

- **`complete_tx` returns the lease *back* on failure** rather than dropping it.
  A refused completion means the token map has desynchronised and the device may
  still own the buffer; releasing the slot would hand it to the next transmit
  under live DMA. Holding it leaks one slot, which is what the ring did before
  and is the safe direction.
- **The transmit *drop* path does not use `with_slot`.** When every ring slot is
  in flight the frame is written to a shared discard buffer, and a borrow
  refusal there would leave no correct way to honour smoltcp's
  `TxToken::consume` contract that the fill closure runs against a buffer of the
  length it asked for — handing it a shorter one can panic inside smoltcp. It
  uses a bounds-checked `first_slot_ptr` deref with the argument stated at the
  site: the buffer is write-only garbage nothing reads back.

The shape is `rump_tap.rs`'s, which has wrapped the same calls in
`akuma_rump::RawNic` since the rump port so the orchestration and its host tests
need no virtio knowledge. It had simply never been applied to the smoltcp path.

### 4.4 (d) Proving the `SocketHandle` transmute

smoltcp 0.12 declares `pub struct SocketHandle(usize)` with a private field and
no accessor beyond `Display`, so there is no safe route to the index, and the
index is load-bearing: `is_valid_handle` guards five real paths against a
corrupted handle reaching the socket set.

The problem was the guard. A `size_of` assertion proves nothing about field
offset, and nothing at all about a future smoltcp adding a second field or
changing `repr`. Added a host test that builds a **real** `SocketSet`, adds
sockets to it, and asserts `socket_handle_index(h) == i` for each — converting a
layout assumption into something `cargo test` fails on at the next smoltcp bump.

### 4.5 Bonus: `TakeOnce` and the socket-storage claim

Not in the original four, but the same defect class and one line away.
`SocketSet::new` needs `&'static mut [SocketStorage; MAX_SOCKETS]`, minted with
a bare `unsafe { &mut SOCKET_STORAGE[..] }` — sound only because `init` is the
sole caller, which is a property of the whole program that nothing in the code
could check. A second claim, added later or on a second core, would have been
instant UB with no diagnostic.

`akuma_primitives::once::TakeOnce` makes the claim itself the check: the first
`take()` hands back the reference, every later one returns `None`. The call site
is safe now, and `init` reports "called twice" instead of aliasing. It lives
beside `OnceCopy`/`Registered` because it is the same family — written once at
boot, used forever — and that module's header now names all three.

---

## 5. The split (planned, not done)

Two kinds of move, and they should not be confused: **crate extractions** buy
host-testability or decouple a build; **module splits** buy navigability at
near-zero risk.

### 5.1 Crate extractions, in order

**A. `akuma-net-unix` — DONE 2026-08-30.** `unix.rs` + `unix_tests.rs`, 2,476
lines, 28% of the crate. Zero smoltcp, zero `unsafe`, one dependency
(`akuma-primitives`, for errno). Two arguments beyond size: it is not
networking in this crate's sense (no NIC, no IP, no port — it is IPC over the
kernel's pipes), and the rump-only devbox pulled all of `akuma-net` *just* to
get AF_UNIX for `rump_server`'s fd 3.

What it actually cost, since "nearly free" was the claim:

- `unix.rs` → `crates/akuma-net-unix/src/lib.rs`, `unix_tests.rs` → `src/tests.rs`
  (`git mv`, so history follows).
- **One import line** was the whole coupling: `use crate::socket::libc_errno`
  became `use akuma_primitives::errno as libc_errno`. Nothing else in
  `akuma-net` referenced `crate::unix`, and nothing in `unix.rs` referenced
  anything else in `akuma-net`.
- Crate boilerplate: `#![cfg_attr(not(test), no_std)]` + `extern crate alloc;`,
  which the module inherited from `akuma-net` before.
- Consumers: **one** — `src/syscall/unixsock.rs`'s import block, switched to
  `use akuma_net_unix::{self as unix, …}` so every `unix::` call site in that
  file is unchanged.
- Workspace `default-members`, and the kernel's `[dependencies]`.

**Not re-exported from `akuma-net`.** `akuma-mmap` is re-exported by
`akuma-exec` precisely so no call site changes, but the situation is the
opposite here: reaching AF_UNIX through the TCP/IP crate is the coupling the
move exists to remove, so leaving a re-export would keep the devbox's dependency
alive and make the split cosmetic.

Result: `akuma-net` 8,845 → 6,367 lines and 138 → 48 host tests; the 90 AF_UNIX
tests now run as `cargo test -p akuma-net-unix`. Total across both is unchanged.

The new crate carries `#![forbid(unsafe_code)]`, joining the 15 crates that
already do — which is the house convention for pure-logic crates and the same
guarantee extraction B is meant to buy for the *rest* of `akuma-net`. `forbid`
rather than `deny` is load-bearing and was verified rather than assumed: a
probe `#[allow(unsafe_code)]` on a function containing an `unsafe` block is
rejected with `E0453: allow(unsafe_code) incompatible with previous forbid`,
which is exactly the edit `deny` would have permitted silently.

**B. `akuma-virtio-net`** — the device layer: `VirtioSmoltcpDevice`,
`LoopbackAwareDevice`, `virtio_rings.rs`, `rump_tap.rs`, `nicstat.rs`, and after
§4 the `frames.rs`/`nic.rs` pair. ~1,200 lines holding **all** of the crate's
remaining `unsafe`, which lets everything else carry
`#![forbid(unsafe_code)]` — turning "is this crate sound?" from an 8.6k-line
question into a 1.2k-line one. Natural sibling to `akuma-virtio`, which already
owns the shared `Hal` and the MMIO probe. One snag to design around:
`LoopbackRing::push` calls `runtime().wake_netpoll`, so either the runtime table
moves down to `akuma-primitives` or the wake becomes an injected fn pointer.

**C. `akuma-net-policy`** — the `akuma-syscalls-*` pattern. Moving the six
already-tested predicates buys nothing; the seam is worth drawing only if it
takes the decisions that **currently cost a boot to test**, each of which has an
archive doc behind it:

- socket-set soft cap + reclaim algebra ([`SOCKETSET_EXHAUSTION_FIX.md`](SOCKETSET_EXHAUSTION_FIX.md))
- the connecting-table / connect-timeout bookkeeping ([`SOCKET_DELAYED_FIRST_BYTE_HANG.md`](SOCKET_DELAYED_FIRST_BYTE_HANG.md))
- `is_loopback_frame` and the loopback ring's push/pop/drop arithmetic
- listener backlog liveness and refresh

Same discipline as `akuma-syscalls-sync`: pick the four things every incident
doc is actually about. Depends only on smoltcp's `tcp::State` enum plus
`akuma-primitives`.

### 5.2 Module splits — cheap, independent, do first

- `socket.rs` (1,791) → `socket/{addr,table,wait,tcp,udp,opts,stat}.rs`. The 60
  `#[cfg(feature = "smoltcp")]` attributes collapse to seven in `mod.rs`.
- `smoltcp_net.rs` (2,069) → `stack/{device,loopback,poll,init,handles,sockets,udp,stream}.rs`.
  `poll()` alone is a 232-line function.

---

## 6. Verification

These changes are on the **per-packet path**, so a green host suite proves very
little on its own. Everything below was run.

### 6.1 Host and build

| check | result |
|---|---|
| `cargo test -p akuma-net` (host) | **149 passed** (was 138; +8 arena, +3 handle-layout) |
| `cargo test -p akuma-primitives` | **54 passed** (+2 `TakeOnce`) |
| `cargo build --release` | clean |
| `cargo clippy --release` | clean (both feature sets) |
| `scripts/build_extreme_size.sh` | 703 KB, under the 4 MB floor |
| `scripts/build_devbox_smoltcp.sh` | 2.6 MB |
| `--features net-noalloc` | builds and clippy-clean |

### 6.2 Boot suite

`cargo build --release` at `MEMORY=2048M` (HVF needs it — a smaller machine
dies in `hvf.c` with a boot-tests build, see `QEMU_HVF_ISV_BUG.md`):

- **316 PASSED, 0 failures** at SMP=2.
- The network-relevant entries all pass: `test_socket_refcount_survives_first_close`,
  `test_so_keepalive_arms_smoltcp`, socketpair ×5, AF_UNIX ×11, socket-waker /
  epoll ×12, `test_socket_wait_backstop_no_hang`.
- `scripts/epoll_suite.py` against the guest: **14 PASS, 0 FAIL, 1 DIVERGE** —
  the diverge is `epoll_ctl_add_twice`, one of the seven pinned divergences.

> **Trap worth recording.** The first boot-suite run showed *zero* `PASSED` and I
> nearly concluded the suite was gated off. `scripts/build_devbox_smoltcp.sh`
> writes to the **same path** as `cargo build --release`
> (`target/aarch64-unknown-none/release/akuma`), and the devbox build sets
> `no-tests`, which turns `cfg(kernel_tests)` off. I had booted the devbox ELF.
> Build order matters when comparing; copy the ELF aside before building another
> profile.

### 6.3 Live traffic

Both the default build and `net-noalloc` (which is otherwise never exercised —
it is off by default and nothing enables it):

| check | default | `net-noalloc` |
|---|---|---|
| ssh round-trip | ok | ok |
| DHCP lease + route | 10.0.2.15/24 | 10.0.2.15/24 |
| DNS (`nslookup`) | resolved | resolved |
| outbound HTTP | ok | ok |
| 1 MB download, md5 checked | `b6d81b36…` correct | `b6d81b36…` correct |
| loopback fetches, byte-compared | **100/100 identical** | **50/50 identical** |
| RX counters | `posted=1174 fail=0 recvd=1173` | n/a (ring path) |

The loopback column is the one that matters most: it is the only thing that
exercises `LoopbackRing`, and each fetch is a 20,595-byte multi-frame transfer
through the new lease-based push/pop.

### 6.4 SMP=4, and a storm that is not ours

At SMP=4 the boot suite produces a `[BKL] stuck: tag=511` storm. `tag=511` is
the profiler's *unknown* bucket, not a network tag, and
`BKL_TAG511_STORM_IS_LOAD_DRIVEN` records the class as pre-existing and
load-driven — but that is a claim about a different build, so it was measured
rather than assumed. A `git worktree` at HEAD, built and booted identically:

| | PASSED | `[BKL] stuck` lines | boots to ssh |
|---|---|---|---|
| pristine HEAD | 314 | 91 | yes |
| with these changes | 314 | 90 | yes |

Identical. The storm reproduces on pristine; 91 vs 90 is noise.

Under the same SMP=4 build: 1 MB download md5-correct, and **four concurrent
workers × 25 loopback fetches = 100/100 byte-identical**, which is the real test
of the arena's borrow bitmask — it is an atomic mask reached from four cores at
once.

---

## 7. What is left

- The split itself (§5). Nothing in §4 blocks it; extraction B is now much
  better defined, because `frames.rs` + `nic.rs` are exactly the pieces that
  move with the device layer.
- `rump_tap.rs`'s 3 `unsafe` are unchanged and appropriate: two are the
  `RawNic` impl (the pattern §4.3 copies) and one is `MmioTransport::new`,
  which is transport construction, not a buffer lifetime.
- The two `SocketHandle` transmutes stay. They are now checked against the real
  type at test time, which is the most that can be done while smoltcp keeps the
  field private.
