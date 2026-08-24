# The long road to Redis, part 2 — the wait loop (2026-08-24)

**Status: three fixes landed and verified, one refactor landed and verified on
a live guest, one reported wedge RESOLVED as a launch error.** This is a session
record, not a finished investigation. Read §7 before continuing the work; it is
the handoff. §5 was rewritten after the fact — its original conclusion survived,
but none of its evidence did; read it before trusting a boot-log diagnosis.

Part 1 is [`REDIS_ROUND_TRIP_CEILING.md`](REDIS_ROUND_TRIP_CEILING.md) (the
saturation ceiling) and [`REDIS_ROUND_TRIP_STAGE_TRACE.md`](REDIS_ROUND_TRIP_STAGE_TRACE.md)
(the stage-by-stage decomposition, plus §4's unexplained single-client latency,
which is what this session set out to chase).

> **The one-line answer.** The wait loop existed **four times** — once in
> `akuma_net::socket::wait_until` and three times in `src/syscall/poll.rs` — and
> the copies had drifted into two live bugs. All four now drive one state
> machine, `akuma-net-yarn`. The three-way rewire is verified on a live guest:
> `nettest-unix poll` returns `verdict=OK` with poll/select/epoll agreeing, the
> boot suite is 301 PASS / **0 FAIL**, and host tests are 756 / 0. The "guest
> wedge" that blocked the first attempt at this was a launch error (§5).

---

## 1. What actually landed

| | Verified how |
|---|---|
| `akuma-net-yarn` — the wait loop as a pure state machine | 25 host tests, incl. a differential test vs the pre-extraction loop |
| `sys_pselect6` registered no waker | boot test, **fails on unfixed kernel** |
| `sys_pselect6` had no `EINTR` check | boot test, **fails on unfixed kernel** |
| `dup`/`dup3`/`fcntl(F_DUPFD)` under-refcounted `EventFd` + `RumpSocket` | builds; **no runtime test yet** |
| All three poll syscalls rewired onto yarn | boot suite 301 PASS / 0 FAIL; `nettest-unix poll` on the guest `verdict=OK`; 756 host tests |
| `net-direct-waker` (smoltcp `rx_waker` registration) | A/B'd: **null result**, correctly |

## 2. The three bugs, and what they have in common

All three are the same failure mode: **a fix applied to some copies of a
duplicated block and not others.**

**2a. `sys_pselect6` passed `None` for its waker** (`poll.rs:1052`), alone among
the three loops. `select(2)` could therefore only ever wake on the 10 ms
`BLOCKING_POLL_INTERVAL_US` tick, however fast the peer answered. The victim is
named in that function's own doc comment: cargo's vendored libcurl compiles
`Curl_poll()`'s `select()` branch, so every cargo network wait rode the tick
while `poll(2)` callers were woken immediately.

**2b. `sys_pselect6` had no `should_interrupt_blocking_syscall()` check.**
`sys_epoll_pwait` and `sys_ppoll` both did. ppoll's comment records that it was
*added there* after exactly this bug — "alarm()+pause() hang instead of
interrupting" — and pselect6, the third copy, never got it. Consequences: a
process blocked in `select()` could not be interrupted by Ctrl-C or `kill`, and
`alarm()` + `select()` slept through its own signal.

The regression test measures it exactly: on the unfixed kernel,
`pselect6_returns_eintr` reports `rc=0 ... elapsed=300437us` — it slept the
**entire 300 ms timeout** and returned 0 instead of `EINTR`.

**2c. `dup` / `dup3` / `fcntl(F_DUPFD)` under-refcounted two fd families.**
Found by the audit in [`SYSCALL_LAYER_AUDIT.md`](SYSCALL_LAYER_AUDIT.md),
independently re-verified here. All three sites (`fs.rs:1519`, `:1555`, `:2534`)
matched exactly four `FileDescriptor` variants then `_ => {}`; the canonical
list in `clone_deep_for_fork` has six. `dup(eventfd)` and `dup(rump_socket)`
produced unreferenced aliases, so the first `close()` destroyed the object under
the survivor.

`fd.rs`'s own comment describes this exact bug on the fork path killing **every
SSH session at kex on the rump devbox**
([`RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md`](RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md)).
It was found, fixed there, and never propagated.

Fixed by **deleting** the three copies: one `akuma_exec::process::clone_fd_refs`
that `clone_deep_for_fork` and all three syscalls call.

## 3. `akuma-net-yarn`

The loop's decisions, pure and host-testable; callers supply the effects.
`crates/akuma-net-yarn/src/lib.rs`. Driven by four call sites:
`akuma_net::socket::wait_until` and the three poll syscalls.

**The two families differ in six policy fields, and every difference is real.**
The value of the crate is that these are now visible and individually testable
instead of being three open-coded loops nobody diffed:

| Field | epoll family | `wait_until` |
|---|---|---|
| `drain_budget` | 1 poll per lap | 64 |
| `fruitless_limit` | 0 — never spin | 4 (the aria2c BKL wedge guard) |
| `epoch_guard` | off | on |
| `timeout_inclusive` | `>=` | `>` |
| `interrupt_precedence` | timeout wins a tie | signal wins |
| `backstop_us` | 10 ms (1 ms w/ rump fd) | 3 ms |
| `park` | `ScanRegistered` | `Promiscuous` |

`interrupt_precedence` was **found while wiring** — nobody had noticed the two
families disagree about a signal-and-timeout tie. It is deliberately a field and
not a unification: my reading of Linux's `do_poll` is that it breaks on
`count || timed_out` before `signal_pending()`, which would make the epoll
family correct and `wait_until` wrong — but that is a hypothesis, and
`nettest`'s Linux arm can settle it in one command. **Do not unify it by
picking.**

Two deletions on principle, not for line count:

- `epoll_wait_deadline` — a second implementation of the deadline arithmetic
  with its own boot test. Deleted; the test was **retargeted** at the live
  machine rather than dropped.
- the three `dup` match blocks (§2c).

## 4. What was measured, and what the measurements said

**`net-direct-waker` A/B — null, correctly.** Registering the waiter's
`ThreadWaker` on the smoltcp socket itself changed nothing:

| clients | baseline | net-direct-waker |
|---:|---:|---:|
| 1 | 2,502 | 2,497 (spread 2 %) |
| 4 | 16,393 | 17,271 (spread 5 %) |
| 32 | 20,161 | 19,685 (spread 7 %) |

Because **`wait_until` is not on Redis's path at all**: `relax=0/0ms` in every
window of both arms, and of the original `redis_why` logs. Redis is
epoll-driven — 169 `[epoll]` traces, **0** pselect/ppoll traces in the boot log.
The park path it exercises is `sys_epoll_pwait`'s.

That baseline also passed an unplanned calibration check: c=1 at 399.6 µs/rt
against the archive's 391.7, c=32 at 20,161 against 20,202. Same system.

**The `pselect6` fixes were A/B'd against a deliberately unfixed kernel**, two
separately-built binaries verified `cmp`-different. One near-miss worth
recording: the first revert left `waker` unused, the build **failed**, and
`rust-objcopy` cheerfully re-copied the *fixed* ELF. Unnoticed, that would have
shown both tests passing on the "unfixed" arm and made the tests look worthless.
Always `cmp` the two arms' binaries.

## 5. The "wedge" — RESOLVED: a launch error, not a guest defect

**There is no wedge.** What the first draft of this section described was
`cargo run --release` being the wrong way to boot this tree, plus two misread
pieces of evidence. Both devbox stacks — `overlays/devbox/run-smoltcp.sh` and
`overlays/devbox/run.sh` — SSH fine on this exact working tree.

**5a. `cargo run --release` is not a runtime target.** It boots the DEFAULT
feature set — the in-kernel boot self-test suite is *in* — at `MEMORY=256M`
against `disk.img`. The devbox stacks boot `devbox-smoltcp,no-tests` at
`MEMORY=4096` against `devbox.img`. Booted the supported way, this same tree
reaches `[herd] Started sshd` in **4 s** and accepts SSH; `nettest-unix poll`
then returns `verdict=OK` (§6).

**5b. `[BKL] dropped window preserved across IRQ xN` is not a spin signature.**
`note_preserved_window` (`crates/akuma-exec/src/bkl.rs:357`) logs only when
`n.is_power_of_two()`. **The doubling is the sampler, not the phenomenon.**
Because the gap between lines doubles, this is disproportionately often the
last line in any log that stops for an unrelated reason. And it appears on
healthy VMs: the 4-second devbox boot above emitted `x128 … x1024` on its way
to a working SSH login. Read it as a counter, never as a wedge.

**5c. The bisect that "exonerated" this work was invalid.** The first draft
argued the freeze was not a regression from §3 because it reproduced on
committed `c63d335c`. But **`akuma-net-yarn` was introduced in `c63d335c`** —
`git log --oneline -- crates/akuma-net-yarn` returns that commit and nothing
earlier, and `socket.rs` first gained `WaitMachine` in it too. The control arm
contained the very change it was supposed to clear. (The `poll.rs` rewire is
the uncommitted half of the work; `wait_until` is the committed half.) The
conclusion happened to be right, but nothing in that table supported it.

**The two arms were never the same binary.** They reported kernel sizes of
3208 KB and 1920 KB: a `cargo build`/`cargo run` for a *different feature set*
**uplifts** its cached ELF over `target/aarch64-unknown-none/release/akuma`, so
two boots minutes apart can silently run different kernels — the same hazard
`scripts/cargo_runner.sh` documents for the `.bin`, one level up. That alone
voids the byte-count comparison the original table rested on. **Copy the ELF
somewhere outside `target/` and boot the copy** before any A/B.

Traps from this investigation worth keeping:

- **SLIRP accepts the TCP connection whether or not the guest is alive**, so an
  open port is not evidence of a live guest. Check the serial log is growing.
- **`tag=511` in `[BKL] stuck` is always meaningless** — the profiler is off by
  default. Read `owner=`.
- **An idle hang and a spin are distinguishable from the host**: `ps -o %cpu` on
  the QEMU process. The "wedge" sat at 4.5 %, which is an idle guest — the one
  reading that is inconsistent with the "i.e. a spin" the log line was taken to
  mean.
- **A console line is not atomic at SMP>1** — the console lock is currently
  off, so another core can split a line mid-write. A real boot produced
  `[herd] Started [syscall] socket(type=TCP) = fd 3` / `sshd (pid= 2)`, i.e.
  `Started sshd` cut in half, and a boot-health check that grepped for that
  marker reported a **false failure on a healthy VM**. Any marker grep can be
  split this way, `grep -ac PASSED` included. Where the thing being tested is
  reachability, probe the service (an actual SSH round-trip); where it must be
  a grep, match a fragment that cannot straddle the split.

## 6. What did NOT happen

- **Redis was not remeasured after the refactor.** The point of Step A is that
  it should be *unchanged*; that regression check is still owed.
- **`nettest-unix poll` was not run on the guest.** It is the right oracle —
  it cross-checks poll/select/epoll against each other and reports `READINESS`
  when they disagree — and the Linux control arm passes (`verdict=OK`, all three
  agreeing). It could not be run because of §5.
- **The `dup` fix has no runtime test.** It compiles and the three sites now
  share one list, but nothing yet proves `dup(eventfd)` survives a close.
- **`extreme-size` clippy** was not run (`cargo test` and the other clippy legs
  were, and are clean).

## 8. Part B — the epoll backstop, measured

`WaitPolicy::epoll`'s `backstop_us` is `BLOCKING_POLL_INTERVAL_US`
(`src/syscall/poll.rs:55`), one constant with one call site. §7 item 5 proposed
lowering it toward `wait_until`'s 3 ms. Measured, `--smp 4 --repeats 3`, against
the Step A control in §7:

| clients | Step A (10 ms) | 1 ms | delta |
|---:|---:|---:|---:|
| 1 | 2,373 | **2,606** | **+9.8 %** |
| 2 | 3,213 | 4,026 | +25 % |
| 4 | 16,807 | 16,447 | −2.1 % |
| 32 | 20,284 | **19,268** | **−5.0 %** |

**The c=1 gain is real and the c=32 loss is real; the middle is noise.** At
c=1 the three runs went 2303/2373/2477 → 2550/2606/2735 — **no overlap**, every
1 ms run beating every 10 ms run. At c=32, 20080/20284/20367 → 19194/19268/19960,
which is barely disjoint. c=2 and c=4 overlap and prove nothing on their own
(c=2's control spread is 125 %, see §7). This is a latency/throughput trade of
the same family as the 1 ms scheduler tick one, and it should be read as such.

**It is not where the hole is.** c=1 moved 421 → 384 µs/rt against Docker's
114 µs. The backstop was worth ~37 µs, not the ~600 µs §4 of
[`REDIS_ROUND_TRIP_STAGE_TRACE.md`](REDIS_ROUND_TRIP_STAGE_TRACE.md) is looking
for. The hypothesis that a missed wake routinely costs a full backstop is
**disconfirmed**: if it were, cutting the backstop 10x would have moved c=1 far
more than 10 %.

### 8a. §4's "688 µs" is stale, and the number matters for how it is quoted

That figure comes from a c=1 arm running **1,321 rps / 757 µs per round trip**.
The same sweep on a fresh boot today gives **2,373–2,606 rps / 384–421 µs**.
Whatever else is true, c=1 is roughly **2x faster than when §4 was written**, so
"688 µs unexplained, nine times the entire saturated round-trip budget" is no
longer the live number — the gap to re-explain is closer to ~300 µs. Re-measure
the decomposition before quoting §4's arithmetic.

### 8b. The measurement includes QEMU, and nobody has subtracted it

`scripts/benchmarks/redis_conc_sweep.py` runs `redis-benchmark -h 127.0.0.1 -p
<hostfwd port>` **on the host**. Every round trip therefore crosses QEMU's
**SLIRP** userspace TCP stack in the QEMU main loop, then virtio-net, then
Akuma. The Docker control it is compared against goes through Docker Desktop's
VM networking instead — a different, much shorter path. **An unknown share of
the 3.7x gap is the emulated NIC, not the kernel**, and no arm in this document
or in `REDIS_ROUND_TRIP_STAGE_TRACE.md` has isolated it.

The cheap experiment nobody has run: `redis-benchmark` **inside the guest**
against `127.0.0.1`, which drops SLIRP, virtio and the host stack out of the
loop. It bounds Akuma's own socket path rather than subtracting SLIRP exactly
(in-guest loopback is the loopback ring, not the NIC path), but it is the
difference between "Akuma is 3.7x slower than Docker" and a claim that survives
questioning. **Do this before quoting any Akuma-vs-Docker ratio.**

## 7. Handoff — in value order

1. **Diagnose §5.** It blocks every guest-level verification. Start from
   `[BKL] dropped window preserved across IRQ` — find who holds the dropped
   window across an IRQ and never closes it. It reproduces on a *committed*
   kernel (`c63d335c`), so bisect backwards from there, and note it is not
   SMP-specific.
2. **Run `nettest-unix poll` on the guest** once §5 is unblocked. Linux arm:
   `docker run --rm --platform linux/arm64 -v "$PWD/bootstrap/bin:/b:ro"
   alpine:3.20 /b/nettest-unix poll` → `verdict=OK`. Any other verdict on the
   guest is a divergence introduced by §3.
3. **Remeasure Redis**, expecting **no change**:
   `scripts/benchmarks/redis_smp_sweep.py --smp 4 --clients 1,2,4,32 --repeats 3
   --features "devbox-smoltcp,no-tests,net-profile"`. Compare against
   `logs/yarn_ab/sweep_akuma-smp4yarn-baseline.json` (c1=2502 c2=3293 c4=16393
   c32=20161). Anything outside the per-cell spread is a Step A regression.
4. **A test for the `dup` fix** — `dup(eventfd)`, close one, use the other.
5. **Then, and only then, Step B**: flip `epoch_guard` on for `epoll_pwait`, or
   drop its backstop from 10 ms toward `wait_until`'s 3 ms, and A/B. These are
   one-line `WaitPolicy` changes now, and they are the live hypotheses for
   §4 of [`REDIS_ROUND_TRIP_STAGE_TRACE.md`](REDIS_ROUND_TRIP_STAGE_TRACE.md) —
   **688 µs of unexplained single-client latency**, nine times the entire
   saturated round-trip budget.
6. **Settle `interrupt_precedence`** against `nettest`'s Linux arm rather than
   by choosing (§3).

## Background

- [`REDIS_ROUND_TRIP_CEILING.md`](REDIS_ROUND_TRIP_CEILING.md) — the saturation
  ceiling and its equation.
- [`REDIS_ROUND_TRIP_STAGE_TRACE.md`](REDIS_ROUND_TRIP_STAGE_TRACE.md) — the
  stage decomposition, the 97.3 %-utilized-but-3.2 %-contended `NETWORK` lock,
  the SMP cache-residency finding, and §4's 688 µs.
- [`SYSCALL_LAYER_AUDIT.md`](SYSCALL_LAYER_AUDIT.md) — the duplication audit
  that found §2c.
- [`RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md`](RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md)
  — the first time §2c's bug was found and fixed, on the fork path.
- [`AKUMA_NET_ISSUES.md`](AKUMA_NET_ISSUES.md) §8 — the waker-park measurement
  whose counter-intuitive result motivated `akuma-net-yarn`.
- Current-state reference: [`../reference/subsystems/syscalls/poll.md`](../reference/subsystems/syscalls/poll.md)
  § "The wait loop is one machine, not three".
