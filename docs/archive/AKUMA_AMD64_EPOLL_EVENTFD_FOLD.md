# amd64: wiring `epoll_create1`/`epoll_ctl`/`epoll_pwait`/`eventfd2`, plus a syscall-table audit

**Date:** 2026-09-17
**Status:** landed in the working tree, **uncommitted** — the user drives commits on this repo.
**Trigger:** `akuma-cli matrix` over ssh on amd64 failed at start-up with
`Error: Failed to initialize input reader`. Root cause (found in the prior
session, in the sibling `akuma-cli` repo, not here): crossterm's default input
backend uses `mio`, and `mio`'s epoll reactor needs `epoll_create1`/`eventfd2`
— both compiled out of amd64's syscall glue. The workaround shipped there
(crossterm's `use-dev-tty` feature, a `poll()`/`socketpair()`-based backend)
unblocked that one binary; this session wires the syscalls themselves so any
`mio`-based program works, not just the one that got a Cargo-feature escape
hatch.

## What was wired

| syscall | x86_64 | asm-generic | dispatch |
|---|---:|---:|---|
| `eventfd2` | 290 | 19 | `to_glue`, unconditional |
| `epoll_create1` | 291 | 20 | `to_glue`, unconditional |
| `epoll_ctl` | 233 | 21 | `to_glue`, unconditional |
| `epoll_pwait` | 281 | 22 | `to_glue`, unconditional |
| `epoll_create`(legacy, size discarded) | 213 | — | x86-only shim → `EpollCreate1` |
| `epoll_wait`(legacy, no sigmask) | 232 | — | x86-only shim → `EpollPwait` |
| `getsockopt` | 55 | 209 | `to_glue`, unconditional |
| `shutdown` | 48 | 210 | `to_glue`, unconditional |

The last two were not part of the original ask — found by the audit in
"What else was missing" below, and cheap enough (unconditional one-line
`to_glue` forwards, same shape as `Pselect6`) to land in the same pass.

### `crates/akuma-syscalls-abi`

Four new `syscall_table!` rows (`Eventfd2`, `EpollCreate1`, `EpollCtl`,
`EpollPwait`) and two new entries in the x86-only legacy-spelling list
(`epoll_create`(213), `epoll_wait`(232)). `Getsockopt`/`Shutdown` already had
rows — see below. 18/18 host tests still pass; `tables_disagree_where_linux_does`
and `x86_only_legacy_spellings_are_not_in_the_table` cover the new rows without
needing new test bodies.

### `amd64/src/usermode.rs`

Two legacy shims (`213`, `232`) before the neutral-table decode, four new
`Syscall::` arms next to `Ppoll`/`Pselect6` (readiness family), and two new
arms next to `Setsockopt` (socket family). `X86_ONLY` in
`dispatch_smoke_test` grew from 23 to 25 entries; four `hop()` assertions and
two neutral-table-overlap checks were added, following the pattern C3's timer
batch used — number-mapping checks only, not live round trips (see
"Verification" for why).

**Why `epoll_pwait` needed no amd64-specific wait-loop arm the way `Ppoll`
does:** `Ppoll` is served by `crate::fd::sys_ppoll`, amd64's own
implementation, while `Pselect6` already forwards to glue. Glue's
`sys_epoll_pwait` blocks through the same `akuma_net_yarn::WaitPolicy` machine
`sys_pselect6` already exercises on this target (`akuma_exec::threading`'s
park/wake, proven to compile and, via `Pselect6`, to run on amd64 already) —
so `epoll_pwait` rides existing, already-provisioned infrastructure rather
than needing a new one.

**Why `getsockopt`/`shutdown` went through glue while the rest of the socket
family (`Socket`/`Bind`/`Listen`/`Accept`/`Connect`/`Sendto`/`Setsockopt`)
stays native `crate::sock`:** those two never had a native amd64
implementation at all — there was no "second family" to avoid growing by
routing through glue, unlike `Sendto`/`Recvfrom`'s existing AF_UNIX carve-out.
`net::dispatch_getsockopt`/`dispatch_shutdown` resolve the same
`FileDescriptor::Socket(idx)` into the same `akuma_net::socket` table
`crate::sock::sys_socket` already allocates out of, so the two paths share
data even though they're different code.

### `amd64/src/exec_runtime.rs`

`eventfd_close`, `eventfd_clone_ref`, `epoll_destroy` were `not_wired!` (a
panic) — real now, behind `#[cfg(feature = "sc-eventfd"/"sc-epoll")]`. These
are `SharedFdTable::close_all()` teardown hooks: once `eventfd2`/
`epoll_create1` can be dispatched, `FileDescriptor::EventFd`/`EpollFd` are
reachable variants, and every process exit fires these hooks unconditionally.
Left `not_wired!` past that point, the first process to open one of these fds
and exit would have taken the kernel down — the exact failure mode the
module's own header already documents for the socket/pipe/AF_UNIX hooks
above them.

The module header's stub count was also stale and got corrected: it said
"nine `not_wired!` stubs" from before `unix_sock_*` wired with `socketpair`
(2026-09-12), a count that never got updated when that landed. It's four now:
`rump_socket_clone_ref`, `pidfd_close` (not built for this target), and
`resolve_file_id`/`read_at_by_inode` (no mount table on this target to answer
from).

### `amd64/Cargo.toml`

New `sc-epoll`/`sc-eventfd` features, forwarding to
`akuma-syscalls-glue/sc-epoll`/`sc-eventfd`, added to `default`.

## A real bug, found by actually booting it: `struct epoll_event` is the wrong shape on x86_64

Wiring the numbers is not the same as the family working, and a live round
trip through the boot suite proved it: `epoll_wait after wakeup write: n=1
events=0x1 fd=0` — the interest list correctly reported the eventfd ready,
but `data.fd` came back `0` instead of the real descriptor. On real Linux,
`struct epoll_event` is `__attribute__((packed))` on x86_64 — 12 bytes,
`events: u32` immediately followed by `data: u64` with no gap — while every
other 64-bit architecture, aarch64 included, uses the natural, 16-byte layout
(a 4-byte pad before `data`). This is a real, if obscure, wart in upstream
Linux's own ABI, not an Akuma invention.

`akuma_syscalls_linux::EpollEvent` — the struct
`akuma-syscalls-glue::poll`'s `sys_epoll_ctl`/`sys_epoll_pwait` read and write
user memory with — is the 16-byte aarch64 shape, unconditionally, on both
kernels. Its own doc comment already named the crossing ("**On aarch64 this
is NOT `__attribute__((packed))`**, unlike x86-64, where the same struct is
12 bytes") — the fact was recorded, nothing had acted on it, because nothing
on amd64 had ever reached this code before this session.

**The consequence is two bugs, not one:**

1. `epoll_ctl`'s event argument, and `epoll_wait`'s output array, are
   marshalled at the wrong stride — `data` is read/written 4 bytes off from
   where the caller's own (packed) struct puts it, which is what produced the
   `fd=0`.
2. `sys_epoll_pwait`'s buffer-size check (`maxevents * size_of::<EpollEvent>()`,
   i.e. `maxevents * 16`) validates against the **wrong** total. An x86_64
   caller sizes its array at `maxevents * 12` bytes; the kernel copies out
   `maxevents * 16`. `validate_user_ptr` only checks the destination range is
   *mapped and writable*, not that it matches the caller's own allocation —
   so for any `maxevents > 1` this silently overwrote up to `4 * (maxevents -
   1)` bytes of whatever the caller had placed immediately after its array
   (this session's live probe used `maxevents = 4`, `struct epoll_event
   evbuf[4]` — a 48-byte stack array taking a 64-byte write). Contained inside
   the caller's own address space, not a kernel privilege issue, but a live
   stack/heap corruption bug in every real caller (`mio`'s reactor included:
   it always asks for more than one event).

### The fix

`crates/akuma-syscalls-glue/src/poll.rs`: a `#[cfg(target_arch = "x86_64")]`
12-byte packed `EpollEventX86 { events: u32, data: u64 }`, used **only** at
the two user-memory boundaries — the `read_user_into` in `sys_epoll_ctl` and
the final `copy_to_user` in `sys_epoll_pwait`'s `WaitStep::Ready` arm — plus
an arch-conditional `EPOLL_EVENT_SIZE` (12 on x86_64, 16 elsewhere) so the
buffer-size validation matches the caller's real stride. Every step in
between — the interest list, the readiness map, the wait loop, the
`kernel_events: Vec<EpollEvent>` the computation builds — is completely
untouched and stays the 16-byte aarch64 shape on **both** architectures; only
the marshalling at the edges is arch-aware, and the aarch64 branch of both
`#[cfg]`s is character-for-character the code that was already there. This
mirrors the existing `akuma-syscalls-abi::stat`/`open_flags` precedent (a
struct that means different bytes on the two architectures, converted once at
the boundary) — the difference is this conversion lives inside
`akuma-syscalls-glue` itself rather than in `akuma-syscalls-abi`, since
nothing outside `poll.rs` needs to know the x86_64 layout exists.

Re-verified after the fix, same live probe: `epoll_wait after wakeup write:
n=1 events=0x1 fd=3 OK` — `fd=3` is the real eventfd descriptor. Host tests
(`cargo test -p akuma-syscalls-linux -p akuma-syscalls-glue`, 43 tests
including `io::tests::epoll_event_array_stride_is_16_not_12`) and
`cargo clippy` stay clean on **both** `x86_64-unknown-none` and
`aarch64-unknown-none`, and the amd64 boot suite still reports 713/713 after
the fix (unchanged from before it — this bug had no boot-suite coverage
either way, since the suite never previously reached live epoll code with
more than a trivial case).

## What else was missing: a full syscall-table audit

Beyond the specific ask, every row in the (now 113-entry) `Syscall` table was
diffed against every `Syscall::` reference in `amd64/src/usermode.rs`, to find
any table row with a glue implementation and no amd64 dispatch arm at all —
the exact shape of gap `epoll_ctl` etc. were in before this session.

**One remains: `brk`(12).** It has a table row and a glue implementation
(`mem::sys_brk`), but amd64's entire memory-management family
(`Mmap`/`Munmap`/`Madvise`/`Mremap`) is native `crate::mm`, not glue — so
wiring `brk` isn't a one-line `to_glue` forward like epoll was; it needs real
per-process program-break bookkeeping amd64 doesn't have yet (glue's own
`sys_brk` assumes a heap-region shape tied to `akuma_exec`'s generic address
space tracking, and whether that shape matches amd64's own would need its own
look). Left as a finding, not fixed here.

## Whole families still gated off

Amd64's `akuma-syscalls-glue` dependency is `default-features = false` plus
only `smoltcp` — every `sc-*` family the AArch64 kernel turns on by default is
off here. Beyond the `sc-epoll`/`sc-eventfd` pair this session adds, checked
each remaining one for standalone buildability
(`cargo check -p akuma-syscalls-glue --no-default-features --features
smoltcp,<feature> --target x86_64-unknown-none`):

| feature | syscalls | builds standalone for x86_64? | notes |
|---|---|---|---|
| `sc-timerfd` | `timerfd_create`/`_settime`/`_gettime` | yes | |
| `sc-pidfd` | `pidfd_open`, `pidfd_send_signal` | yes | `pidfd_close` teardown hook already `not_wired!`, same shape as eventfd/epoll were |
| `sc-aio` | `io_setup`/`io_submit`/`io_getevents`/`io_cancel`/`io_destroy` | yes | Linux native AIO — the AArch64 kernel needed this for `bun` (`docs/archive/BUN_MISSING_SYSCALLS.md`; `sys_io_setup` has to write a real mmap'd `aio_ring`, not a small integer, because `bun` dereferences the returned context immediately) |
| `sc-sysv-ipc` | `shmget`/`shmat`/`shmdt`/`shmctl`, `semget`/`semop`, `msgget`/`msgsnd`/`msgrcv` | yes | matters for anything Postgres-shaped |
| `sc-containers` | mount/namespace syscalls for the "box" abstraction | **no**, standalone | needs `akuma-vfs-glue/sc-containers` forwarded too (root `Cargo.toml` does this because the aarch64 binary depends on `akuma-vfs-glue` directly, same as amd64 does — amd64's own `Cargo.toml` just never defined the forwarding feature). Separately, unclear amd64 has any "box"/namespace concept for this to attach to yet |

Each of the first four is the same shape as this session's `sc-epoll`/
`sc-eventfd` work: turn the feature on, add `syscall_table!` rows, add
dispatch arms, wire any teardown hooks the new `FileDescriptor` variant needs.
None of that was done here — this is a survey, not an implementation.

## A build-tooling trap that cost real time in this session

**`.cargo/config.toml`'s `[build] target = "aarch64-unknown-none"` is the
default for the *whole workspace*, amd64 package included.** Running
`cd amd64 && cargo build --release` — no `--target` flag — silently builds the
`akuma-amd64` package against `aarch64-unknown-none`. Every
`#[cfg(target_arch = "x86_64")]` block in `akuma-mmu` (including the entire
x86_64 `UserAddressSpace` impl — `map_page_pte`, `pte_prot`,
`for_each_leaf_in_range`, `rewrite_leaves_in_range`, `for_each_user_leaf`)
compiles out, `UserAddressSpace` resolves to the aarch64 struct instead, and
`amd64/src/uas.rs`/`uaccess.rs` — which call those x86_64-only methods by
name — fail with "no method named `X` found for struct `UserAddressSpace`".
111 such errors, completely reproducible, looking exactly like a genuine
pre-existing breakage on this branch.

**It is not one.** `amd64/build.rs` and the crate's own doc comments say the
correct invocation is `cargo build --release --target x86_64-unknown-none`
from inside `amd64/` (there is deliberately no `[target.x86_64-unknown-none]`
default in the workspace config, precisely so ad-hoc
`cargo check -p <crate> --target x86_64-unknown-none` probes of individual
crates stay clean of the amd64 linker script). Rebuilding with the explicit
flag: **zero errors**, `akuma-amd64` links, `target/x86_64-unknown-none/release/akuma-amd64`
is a real 3 MB static x86-64 ELF.

This session initially reported the false "111 pre-existing errors, unrelated
to this change" reading to the user before catching the mistake later in the
same conversation — worth flagging so the correction is on the record rather
than silently overwritten. It is the same *family* of trap as a
`--manifest-path` build silently skipping a tree's own `.cargo/config.toml`
(seen before in this project's history on the aarch64 self-host side): a
config file applying — or, there, not applying — based on which directory the
`cargo` invocation runs from rather than which package or target was asked
for.

## Verification

This session did have a local amd64 QEMU rig — `amd64/run.sh`, PVH boot under
TCG (`-M microvm`, no accel: an x86_64 guest on Apple Silicon has no faster
option) — missed on the first pass and corrected once pointed at it. Booted
locally with `SSH_PORT=2322 HTTP_PORT=8180 INIT=/bin/sshd sh amd64/run.sh`
(non-default ports: another instance already held 2222/8080), against the ssh
key `mkdisk.sh` bakes into the disk image at
`target/x86_64-unknown-none/release/amd64-ssh-test-key`.

| check | result |
|---|---|
| `cargo test -p akuma-syscalls-abi` (host) | 18/18 pass |
| `cargo test -p akuma-syscalls-linux -p akuma-syscalls-glue` (host) | 43/43 pass |
| `cargo clippy`, both crates above, both `x86_64-unknown-none` and `aarch64-unknown-none` | clean |
| `cargo build --release --target x86_64-unknown-none` in `amd64/` | clean, binary links |
| **Boot suite, QEMU/TCG, `SSH_PORT=2322`** | **713 passed, 0 failed** (same after the ABI fix below as before it) |
| **`dispatch_smoke_test`'s new `hop()`/overlap checks, live** | **all `[OK]`** — `epoll_create1 291 -> 20`, `epoll_ctl 233 -> 21`, `epoll_pwait 281 -> 22`, `eventfd2 290 -> 19`, both 213/232 overlap checks |
| **Live `userspace/epollprobe/c/epoll_op_cost`, cross-built for x86_64 and pushed over ssh** | `epwait_empty`/`epwait_1fd` `ret=0`, `epwait_ready` `ret=1`, `epctl_mod` `ret=0` — epoll agrees with `ppoll`/`select` on the same fd's readiness |
| **Live eventfd2 + epoll integration probe** (ad hoc, not committed — see below) | roundtrip OK, drained-read `EAGAIN` OK, registered-in-epoll wakeup delivered with the **correct** fd after the fix (see the ABI-bug section above) |

The eventfd/epoll integration probe was written for this session
(`eventfd(2)` round trip, a drained non-blocking read, then the exact `mio`
pattern — register the eventfd in an epoll set, write to it, confirm
`epoll_wait` reports `EPOLLIN` for the right fd) and run from the scratch
directory rather than added to `userspace/`; `userspace/epollprobe/c/` is the
tree's own, already-existing epoll probe and needed only a cross-build
(`x86_64-linux-musl-gcc`, no source changes) to serve as the multi-fd,
multi-syscall-family live check.

**Not done:** bare-metal verification on the physical reference box (this was
a local QEMU/TCG boot only), and a live `mio`/crossterm/`akuma-cli matrix`
round trip over ssh — the scenario that started this — which needs the
sibling `akuma-cli` repo's binary rebuilt against these syscalls now being
real (it currently still carries the `use-dev-tty` workaround, which is
harmless to leave in place but no longer necessary).

## What's left

- Bare-metal boot verification, and the live `akuma-cli matrix`/`mio` round
  trip that started this (above).
- `brk`(12) — the one syscall-table row with no amd64 arm; needs real
  heap-bookkeeping design work, not a forward.
- `sc-timerfd`, `sc-pidfd`, `sc-aio`, `sc-sysv-ipc`, `sc-containers` — each a
  bounded follow-up in this session's shape; `sc-containers` additionally
  needs the `akuma-vfs-glue` feature-forwarding fix described above. Given
  what turned up in `struct epoll_event`, any future family with its own
  x86_64-vs-aarch64 wire struct (`sc-aio`'s `io_event`, `sc-sysv-ipc`'s
  `shmid_ds`/`semid_ds`/`msqid_ds`) should be checked against the real x86_64
  Linux headers **before** trusting a straight `to_glue` forward, not after a
  live probe catches it by accident.

## Background

- `docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md` — the `Syscall`
  table/`to_glue` shape this work extends.
- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH3C.md` — the fold-shape precedent
  (forward to an already-written glue arm, fix the hook gap it surfaces).
- `docs/archive/AKUMA_AMD64_STALE_FALSE_HOOKS.md` — the same "a stub's stated
  reason stopped being true and nothing noticed" pattern the `exec_runtime.rs`
  header-count fix here repeats.
- `docs/reference/subsystems/syscalls/poll.md` — `akuma-syscalls-poll`'s
  readiness map and the `WaitPolicy` shared wait loop `epoll_pwait` rides.
