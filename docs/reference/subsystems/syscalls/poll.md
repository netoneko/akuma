# poll syscalls

poll (via `ppoll`) / `pselect6` / `epoll_create1` / `epoll_ctl` / `epoll_pwait`.
Source: `src/syscall/poll.rs`. `epoll_*` is gated behind the `sc-epoll`
feature — Tier 2 in [`../syscalls.md`](../syscalls.md)'s "Feature gates &
ExecRuntime stubs" table, meaning turning it off requires the no-op
`ExecRuntime` stub (`epoll_destroy: noop_u32`, `src/main.rs:451`) rather than
being pure dead weight. For the generic blocking/wait-queue pattern every
syscall here uses, see `../syscalls.md` "Blocking vs non-blocking"; for what
"readable"/"writable" means per resource type (sockets, pipes, eventfd,
child channels), see `../networking.md` and [`../vfs.md`](../vfs.md) — this
doc covers only the syscall entry points: argument validation, the
epoll interest-list semantics, and known instability history.

The family's **pure logic** — the readiness map, the interest list, the
`EPOLLET` decision and the two wire marshallings — lives in
`crates/akuma-syscalls-poll` since 2026-08-29; `src/syscall/poll.rs` keeps
every effect. See "Where the logic lives" below before changing either.

> **Stability: B (mostly stable, one open gotcha).** The March 2026 EL1-crash
> /stack-overflow/DNS-hang cohort (6 root causes) is resolved and dormant —
> no epoll crash fixes since. One item stays genuinely **OPEN**:
> `epoll_destroy` is not reference-counted, so sharing an epoll fd via `dup`
> across `fork` lets either side's `close()` destroy the other's interest
> list. The recurring lesson: **never hold `EPOLL_TABLE` while doing a
> readiness check** — every readiness check can recurse into
> `current_process()` (`PROCESS_TABLE`) or a resource's own lock, and two
> historical whole-kernel deadlocks (`EPOLL_TABLE` ↔ `PROCESS_TABLE`,
> `NETWORK` ↔ `SOCKET_TABLE`) came from violating exactly that ordering.

## Where the logic lives (2026-08-29)

The family's pure logic is `crates/akuma-syscalls-poll`; `src/syscall/poll.rs`
keeps every effect. The seam is inside what used to be one function:
`epoll_check_fd_readiness` **probed** an fd and **mapped** what it found onto
event bits in a single `match`, and only the second half was ever testable.

| decision | where | what it is |
|---|---|---|
| fd state → event bits | `akuma_syscalls_poll::readiness` | one `FdState` variant per fd kind the kernel models, plus which bits are maskable |
| the interest list | `akuma_syscalls_poll::interest` | fd → `{events, data, last_ready}`, and `epoll_ctl`'s effect on it |
| `epoll_ctl` op decode + errno set | `akuma_syscalls_poll::ctl` | `ADD`/`MOD`/`DEL`/unknown, whether an op reads a user `epoll_event`, `ENOENT`/`EINVAL` |
| the `EPOLLET` armed-state decision | `akuma_syscalls_poll::edge` | `revents & !last_ready`, and what the entry records afterwards |
| `select(2)` fd-set marshalling | `akuma_syscalls_poll::fdset` | the `MAX_FDS` cap, word arithmetic, bit set/test, the bits-not-fds count |
| `poll(2)` `POLL*`↔`EPOLL*` | `akuma_syscalls_poll::pollfd` | both directions, including which bits `events` masks |
| **the wait loop** | `akuma-net-yarn` | already extracted 2026-08-24 — see the section below, and do not merge it with anything |
| every probe | `src/syscall/poll.rs` | `socket_can_recv_tcp`, `pipe_hup`, `listener_ready`, `rump_socket_readable`, … |
| every waker registration | `src/syscall/poll.rs` | which arms register a poller, and under which requested bit |
| `EPOLL_TABLE`'s lock + IRQ masking | `src/syscall/poll.rs` | the AB-BA argument, and the `EPOLL_TABLE` ↔ `PROCESS_TABLE` ordering |
| the interest-list snapshot | `src/syscall/poll.rs` | 128-entry stack array before a heap `Vec` — an allocation policy, kept out of an IRQ-masked hold |
| the `EBADF` checks | `src/syscall/poll.rs` | fd-table lookups, including the membership probe that keeps `EBADF` ahead of `EFAULT` |

The crate cannot take a lock, touch user memory, probe an fd or wake a thread,
because it has no way to. `#![forbid(unsafe_code)]`.

**Why the `requested` gating appears on both sides.** Several kernel arms probe
*conditionally* — `PipeWrite` only asks `pipe_can_write` when `EPOLLOUT` was
requested, because the same branch registers a poller, and registering one for
an event nobody asked about adds a wakeup source out of nowhere. Those arms
report `false` for facts they never established. That is safe only because
every use of such a fact in the map is already `&&`-ed with the same requested
bit, so an unprobed `false` and a probed `true` give the same answer. The
kernel's copy decides which *effects* run; the crate's decides which *bits* come
out. Pinned by `an_unprobed_fact_cannot_change_the_answer_for_an_unrequested_bit`.

Background: [`../../../archive/AKUMA_EXTRACT_SYSCALLS.md`](../../../archive/AKUMA_EXTRACT_SYSCALLS.md) §8.2.

## Known divergences from Linux

Each is **preserved, not fixed** — an extraction that quietly fixes something
cannot be A/B'd against what it replaced. Each is pinned by a host test named
to say what it is, and `epollops` reports the first as a `DIVERGE` line rather
than a failure (the same static binary answers `PASS` on Linux, which is what
proves the probe is asking the right question).

| divergence | Linux | here | pinned by |
|---|---|---|---|
| `EPOLL_CTL_ADD` on an fd already in the interest list | `EEXIST`, registration untouched | overwrites it like a `MOD` — events, data and edge state — and returns 0 | `an_add_on_a_present_fd_overwrites_instead_of_reporting_eexist`; `epollops` `epoll_ctl_add_twice` |
| `poll(2)` on an fd the process does not have | `POLLNVAL` | `POLLHUP\|POLLERR` (the fd reaches the map as `FdState::Missing`) | `a_bad_fd_reports_pollhup_pollerr_rather_than_pollnval` |
| `poll(2)` asked for `POLLRDHUP` | reports a half-close | dropped in both directions, though `epoll` on the same socket reports `EPOLLRDHUP` | `pollrdhup_and_pollpri_are_dropped_in_both_directions` |
| `select(2)` with a high `nfds` | no limit | `nfds > 1024` is `EINVAL`, a cap `ppoll`/`epoll` do not have | `nfds_above_the_hard_cap_is_rejected` |
| more ready fds than `maxevents` | a ready-list rotation, so nobody starves | the interest list is walked in ascending fd order and truncated, deterministically | `the_interest_list_is_walked_in_ascending_fd_order` |
| `epoll_pwait`/`ppoll`/`pselect6` `sigmask` | applied for the duration of the wait | accepted and discarded (see "Argument validation" below) | — |
| an fd `close()`d while still in an interest list | dropped implicitly by `eventpoll_release_file` | pruned lazily, on the next `epoll_pwait` scan | `pruning_a_closed_fd_is_what_stops_a_synthetic_hup_err` |

`POLLPRI`/`exceptfds` being always empty is **not** on this list: Akuma has no
out-of-band TCP data, so "none" is the honest answer. What matters there is
that it is *written down* — see `exceptfds` below.

## Testing

| gate | what it covers | how to run |
|---|---|---|
| crate host tests | the readiness map, the interest list, `epoll_ctl`'s errno set, the `EPOLLET` decision, both marshallings — 36 tests, milliseconds, no VM | `cargo test -p akuma-syscalls-poll --target $(rustc -vV \| grep '^host:' \| cut -d' ' -f2)` |
| `epollops` | the same questions asked of a live kernel, op by op: ET re-arm on a drained read and on a blocked write, the pipe EOF edge, `epoll_ctl`'s errno set, a zero timeout being non-blocking, level-triggered repetition, `poll(2)`'s unrequested `POLLHUP`, `select(2)` overwriting `exceptfds` and counting bits, and the TCP group | `scripts/epoll_suite.py --port 2322` |
| the same binary on Linux | proves the probe itself is right — every `FAIL` in the guest should be a `PASS` there, and every `DIVERGE` too | `scripts/epoll_suite.py --linux` |
| boot suite | `test_epoll_eintr_when_signal_pending`, `epoll_edge_rearm_symmetry`, `epoll_pipe_eof_edge_after_partial_drain`, `run_pselect6_*` | any `cargo run --release` without `no-tests` |
| cost A/B/A | that a change to this family is free: seven non-parking arms, ratio against a `getpid` control | `scripts/benchmarks/epoll_ab_run.sh <label> <outdir> 4 12` |

Tests here are named for the **incident**, not the function: a test called
`test_epoll_wait` tells the next person nothing, while
`a_pipe_at_eof_reports_hup_so_the_eof_transition_is_an_edge` tells them what
breaks and roughly how long they will spend finding it in a VM.

## Syscall table

| Syscall | nr | Entry point | Gate |
|---|---|---|---|
| `pselect6` | 72 | `sys_pselect6` | always |
| `ppoll` | 73 | `sys_ppoll` | always |
| `epoll_create1` | 20 | `sys_epoll_create1` | `sc-epoll` |
| `epoll_ctl` | 21 | `sys_epoll_ctl` | `sc-epoll` |
| `epoll_pwait` | 22 | `sys_epoll_pwait` | `sc-epoll` |

There is no plain `poll(2)` — userspace's `poll()` wrapper (musl) is expected
to go through `ppoll` with a null timeout, which this dispatcher treats as
infinite wait (see below). When `sc-epoll` is off, any binary calling
`epoll_create1`/`epoll_ctl`/`epoll_pwait` gets `[ENOSYS] nr=20/21/22` — decode
via [`../syscalls.md`](../syscalls.md) "Porting a new binary".

## Argument validation & error codes

**`sys_epoll_create1`**: no argument validation beyond having a current
process (else `EBADF`, not `ESRCH` — a boundary-quirk worth knowing). Honors
`EPOLL_CLOEXEC` in `flags`; any other bit is silently accepted.

**`sys_epoll_ctl`**: `epfd` must resolve to a live `FileDescriptor::EpollFd`
→ else `EBADF`. `EPOLL_CTL_ADD`/`MOD` validate `event_ptr` for the 16-byte
ARM64 `epoll_event` layout (`events: u32, _pad: u32, data: u64` — **not**
packed, unlike x86_64) before `copy_from_user_safe`; a bad pointer → `EFAULT`.
`ADD` on an already-present `fd` silently upgrades to a `MOD` (logged, not an
error) rather than `EEXIST`. `MOD`/`DEL` on an absent `fd` → `ENOENT`.
Unknown `op` → `EINVAL`.

**`sys_epoll_pwait`**: `maxevents <= 0` → `EINVAL`. The output buffer is
validated for `maxevents * 16` bytes → `EFAULT` if it doesn't fit. `timeout`
follows the standard three-way convention: `> 0` real timeout (ms→µs),
`== 0` a single non-blocking poll, `< 0` infinite wait. `is_current_interrupted()`
→ `EINTR`. **The `sigmask`/`sigsetsize` arguments (args 4/5) are accepted and
logged when `SYSCALL_DEBUG_NET_ENABLED`, but never applied** — `epoll_pwait`
does not actually mask signals for the duration of the wait, unlike real
Linux `pwait` semantics. Same gap in `ppoll`'s `_sigmask` and `pselect6`'s
`_sigmask_ptr` — both parameters are received and discarded (see the
leading underscore in their signatures).

**`sys_ppoll`**: `nfds == 0` → `0` immediately (no error). Buffer sized
`nfds * size_of::<PollFd>()` is validated → `EFAULT`. A null `timeout_ptr`
means infinite wait; otherwise it's read as a 16-byte `timespec` → `EFAULT`
on a bad pointer.

> Both of these read their timeout through
> `akuma_syscalls_time::read_timeout_us`, which returns `Option<u64>` —
> `None` is the null-pointer "wait forever". Until 2026-08-28 they carried
> byte-identical ten-line copies of the copy-in and the arithmetic, and a
> separate `infinite` flag beside `timeout_us` that every later use had to
> remember to consult. The two locals still exist because the wait loops read
> them separately, but they are now derived from one value and cannot
> disagree. Rationale and the overflow behaviour it fixed:
> [`time.md`](time.md) § "the timespec-to-timeout conversion". Negative `fd` entries in the array are skipped (matches
POSIX `poll()`: negative fd means "ignore this slot").

**`sys_pselect6`**: `nfds == 0` → `0`. `nfds > 1024` (`MAX_FDS`) → `EINVAL` —
this is a **hard cap** not present in the epoll/ppoll paths; a caller
`select()`-ing on a very high fd number will get `EINVAL` where `ppoll`/
`epoll` would work fine. `readfds_ptr`/`writefds_ptr`, when non-null, are
validated for `nfds.div_ceil(64) * 8` bytes.

`exceptfds_ptr` reports no exceptional conditions — Akuma has no out-of-band
TCP data, so the answer is always "none" — but it **is written**: every return
path zeroes the caller's set. Reporting by overwriting is what `select()` is,
and a set left untouched reads back as "every fd I asked about has an
exceptional condition". That was a live bug until 2026-08-20 and it broke
`cargo` completely: the nightly toolchain's libcurl compiles `Curl_poll()`'s
`select()` branch (curl-sys' `build.rs` defines `HAVE_POLL_H`/`HAVE_POLL_FINE`
but not plain `HAVE_POLL`) and asks for `POLLPRI` on a connecting socket, which
that branch places in `exceptfds`. The stale set made libcurl synthesise
`POLLPRI`, map it to `CURL_CSELECT_ERR`, and abandon a socket that had just
reached `Established` with `SO_ERROR == 0`. See
[`../../../runbooks/cargo-cannot-reach-crates-io.md`](../../../runbooks/cargo-cannot-reach-crates-io.md).
Regression: `run_pselect6_exceptfds_test` (`[PASS] pselect6_clears_exceptfds`).

## epoll interest-list semantics

`EPOLL_TABLE: Spinlock<BTreeMap<u32, EpollInstance>>` is a **process-global**
table keyed by an internal `epoll_id` (not by fd) — an `EpollFd(id)`
`FileDescriptor` variant is just a handle into it. `sys_epoll_pwait`'s loop:

1. Snapshots the interest-list fd keys into a stack array (≤128 fds; a
   heap `Vec` only for larger interest lists) **then releases
   `EPOLL_TABLE`** before calling `epoll_check_fd_readiness` per fd — the
   lock-ordering fix from the `EPOLL_TABLE`/`PROCESS_TABLE` deadlock (see
   Background).
2. `epoll_check_fd_readiness` (`poll.rs:276`) is the single readiness
   dispatch used by `epoll_pwait`, `ppoll`, and `pselect6` alike — it
   switches on the fd's `FileDescriptor` variant and, when a `Waker` is
   passed, registers it with the underlying resource (socket/pipe/eventfd/
   child-channel/timerfd/pidfd) so a producer-side write wakes this thread
   directly instead of waiting for the next poll tick. Fd types with no
   real readiness model (e.g. a plain file) fall through to the catch-all
   arm and are reported ready for whatever was requested — always-ready,
   not polled.
3. `EPOLLET` (edge-triggered) bookkeeping lives in this file — `last_ready`
   per interest-list entry tracks what was already reported, so an
   edge-triggered fd only re-fires on **new** bits since the last report — but
   the *resets* cannot. `last_ready` is refreshed only inside
   `sys_epoll_pwait`'s own loop, so a level transition that happens and
   un-happens between two passes is invisible to it. The I/O syscalls are the
   only code that witnesses those transitions, so `net.rs`/`fs.rs` report them
   back, in **both** directions:

   | Hook | Called from | Clears | Why |
   |---|---|---|---|
   | `epoll_on_fd_drained` | `recvfrom`/`recvmsg`/`read` on **sockets, pipes and socketpairs** — every successful read **and** every `EAGAIN` | `EPOLLIN` | a caller that reads one TLS record at a time without draining to `EAGAIN` (BoringSSL/bun) would never see `EPOLLIN` fire again for data that arrived in the same poll window |
   | `epoll_on_fd_write_blocked` | `sendto`/`sendmsg`/`write` — every **short** write **and** every `EAGAIN` | `EPOLLOUT` | a caller that fills the 16 KB TCP transmit buffer and waits for `EPOLLOUT` would wait forever: this loop drives `smoltcp_net::poll()` at the top of each pass, which usually flushes the buffer before readiness is computed, so `can_send()` is never *observed* false |

   The write hook did not exist until 2026-08-17 and its absence was an
   intermittent hang — intermittent precisely because it raced the flush
   described above. If you add a new readiness bit here, add its reset hook at
   the same time. Regression: `epoll_edge_rearm_symmetry` in the boot suite.

   The read hook was wired into the **socket** paths only, and later the same
   day the identical hole was found on **pipes**: `sys_read`'s `PipeRead` and
   `UnixSocket` arms never called it, so a child's stdout fired `EPOLLIN` exactly
   once and was `SUPPRESSED` for the rest of its life. That is the whole reason
   `tokio::process::Command::output()` never completed on Akuma. Two lessons
   generalise. First, **the hook belongs to the fd type, not to the syscall** —
   "`read` calls it" is not enough when `read` has eight arms. Second, a
   readiness predicate that folds two states into one bit has no edge between
   them: `pipe_can_read` answers true both for "has bytes" and "at EOF", which is
   why pipes now also report `EPOLLHUP` (`pipe_hup`) once the last writer is
   gone — that is the bit that makes the EOF transition an edge at all.
   Regression: `epoll_pipe_eof_edge_after_partial_drain`;
   [`../../../archive/TOKIO_PIPE_EPOLL_HANG.md`](../../../archive/TOKIO_PIPE_EPOLL_HANG.md),
   procedure in
   [`../../../runbooks/debug-async-subprocess-hang.md`](../../../runbooks/debug-async-subprocess-hang.md).

   To see these decisions live, set `SYSCALL_DEBUG_EPOLL_EDGE` (`src/config.rs`):
   one line per ready fd per scan, with `rev`/`last`/`new` and
   `deliver`/`SUPPRESSED`.

   Note also what `epoll_check_fd_readiness` must NOT report: a TCP socket
   still in `SynSent` answers `is_active() && !may_recv()`, the same pair a
   peer's FIN produces, and reporting that as `EPOLLIN`/`EPOLLRDHUP` told
   clients a *connecting* socket was already read-closed. See
   [`../networking.md`](../networking.md) "Readiness reporting".
4. No event ready and `timeout != 0`: `schedule_blocking(deadline)` — never
   a busy `yield_now()` spin. The 10 ms `BLOCKING_POLL_INTERVAL_US` cap
   still exists as a safety net for resources that don't yet support wakers
   (e.g. `TimerFd`), but a woken thread returns immediately rather than
   waiting for the next tick.

## The wait loop is one machine, not three (2026-08-24)

`sys_epoll_pwait`, `sys_pselect6` and `sys_ppoll` used to carry three
open-coded copies of the same loop — drive the stack, scan for readiness,
return if ready, check timeout, check signal, park on a deadline. They now all
drive [`akuma_net_yarn::WaitMachine`](../../../../crates/akuma-net-yarn/src/lib.rs)
under `WaitPolicy::epoll(effective_poll_interval_us(..))`, and supply only the
effects. `akuma_net::socket::wait_until` drives the same machine under a
different policy.

**Read the policy, not the loop.** Every way this family differs from the
socket family is a field on `WaitPolicy`, each of which is a real divergence
that predates the extraction:

| Field | epoll family | `wait_until` |
|---|---|---|
| `drain_budget` | 1 poll per lap | 64 |
| `fruitless_limit` | 0 — never spin on unrelated progress | 4 |
| `epoch_guard` | off | on |
| `timeout_inclusive` | `>=` — this is what makes a **zero** timeout non-blocking | `>` |
| `interrupt_precedence` | timeout wins a tie | signal wins |
| `backstop_us` | `effective_poll_interval_us()` — 10 ms, 1 ms with a rump fd | 3 ms |
| `park` | `ScanRegistered` — the waker was registered during the readiness scan | `Promiscuous` |

`epoll_pwait` refreshes `backstop_us` every lap (`machine.set_backstop`)
because `epoll_ctl` can add or remove a rump fd underneath it; its two
siblings hoist the computation above their loops.

**Why it was worth doing:** the three copies had drifted, and the drift was
bugs. `sys_pselect6` passed `None` for its waker (so `select(2)` could only
wake on the 10 ms tick — cargo's libcurl compiles the `select()` branch) and
had no `should_interrupt_blocking_syscall()` check at all (so a process
blocked in `select()` could not be interrupted by Ctrl-C or `kill`, and
`alarm()` + `select()` slept through its own signal). Both fixed 2026-08-24
with `run_pselect6_registers_waker_test` and `run_pselect6_eintr_test`; both
verified to fail on the unfixed kernel. Full account:
[`../../../archive/SYSCALL_LAYER_AUDIT.md`](../../../archive/SYSCALL_LAYER_AUDIT.md)
and [`../../../archive/REDIS_ROUND_TRIP_STAGE_TRACE.md`](../../../archive/REDIS_ROUND_TRIP_STAGE_TRACE.md) §10.

The deadline arithmetic lives in `WaitMachine::park_deadline` and nowhere
else. A standalone `epoll_wait_deadline` helper used to compute the same thing
next to it; it was **deleted**, not kept, because a second implementation with
its own tests is how the three loops drifted apart to begin with.

`epoll_destroy(epoll_id)` removes the instance outright — see the Stability
callout above; it is invoked from `sys_close`'s `EpollFd` arm and from the
CLOEXEC-fd-closing path on `execve`, and (per the fork fix below) `EpollFd`
entries are stripped from a child's fd table across both `fork` and
`vfork`/`clone_thread`, so an ordinary fork+exec never triggers this shared-
destroy hazard — only an explicit `dup` across a fork can.

## DNS-hang-under-bun (historical, resolved)

`docs/README.md`'s symptom matrix lists "epoll crash / DNS hang under bun" →
[`../../../runbooks/debug-network.md`](../../../runbooks/debug-network.md)
"epoll issues". That table's entries are now all `FIXED` except the dup-across-fork
non-refcounting gap noted above. The DNS-hang mechanism specifically was a
socket-table-exhaustion chain, not an epoll bug per se: a crashing process
used to leak its open sockets (no cleanup on the crash path), and after a
few crashes `MAX_SOCKETS` was exhausted, so the next `bun install`'s DNS
resolver hung waiting on a UDP socket that could never be allocated. Fixed
by routing the EL1 fault-recovery pad through `return_to_kernel(-14)`
(proper fd/socket cleanup) instead of looping forever. See Background.

## Background

- `archive/EPOLL_EL1_CRASH_FIX.md` — the six-cause March 2026 cohort: EL1
  data-abort recovery, lazy stack region, the reverted kernel-VA exclusion,
  `epoll_destroy` on a child-shared fd, `EPOLL_CLOEXEC` being ignored, and
  the socket-exhaustion DNS-hang chain.
- `archive/EPOLL_PERFORMANCE.md` — the waker-based reactive-polling
  rewrite, the two lock-inversion deadlocks (`EPOLL_TABLE`↔`PROCESS_TABLE`,
  `NETWORK`↔`SOCKET_TABLE`) and their fixes, and a Go-toolchain compatibility
  survey run through this subsystem.
- `archive/NETWORKING_POLLING_AND_ACK_FIXES.md`.
- [`../../../runbooks/debug-network.md`](../../../runbooks/debug-network.md) —
  "epoll issues" table (symptom/cause/status/fix), the live source of truth
  for what's fixed vs. open in this file.
