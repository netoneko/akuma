# Phase 7b: the `netpoll_drain`-shaped `ppoll`/`pselect6`/`epoll_pwait` window — and why the whole-syscall carve was reverted

**Status**: Piece 1 landed 2026-08-01 (unconditional, rides the existing `no-bkl-network`
gate — no new feature). Piece 2 was built, found to cause an intermittent data-corruption
race in a same-binary A/B, and **reverted the same session**. Per this workplan's own
instruction ("if it does NOT hold up... stop, do not build the guard, and leave piece 1
as the whole of 7b") — this is that stop, triggered empirically rather than by the static
audit.

Executed per the prompt in
`bkl-phase7-workplan.md` (deleted — a workplan is not a runbook; the live
remaining-work list is [`BKL_PHASE7F_OPTOUT_LIST.md`](BKL_PHASE7F_OPTOUT_LIST.md) §11) (Prompt C).

## 1. What changed

`src/syscall/poll.rs`: each of the three BKL-held `smoltcp_net::poll()` calls inside
`sys_epoll_pwait`/`sys_pselect6`/`sys_ppoll`'s readiness loop now runs in its own
dropped-BKL window:

```rust
#[cfg(all(kernel_smp_shared, kernel_no_bkl_network))]
akuma_exec::bkl::dropped_window_open();
#[cfg(feature = "smoltcp")]
akuma_net::smoltcp_net::poll();
#[cfg(all(kernel_smp_shared, kernel_no_bkl_network))]
akuma_exec::bkl::dropped_window_close();
```

Byte-for-byte the same mechanism as the `netpoll_drain` carve in `src/main.rs`
(`BKL_VFS_CARVE_OUT.md` §19–20): every piece of state `poll()` touches (`NETWORK`,
transitively `SOCKET_TABLE`) is already behind its own `PreemptGuard`-protected lock, so
the window doesn't need anything new locked — it's a second call site for a carve that
was already proven safe in production. Gated on `kernel_no_bkl_network` specifically (not
`kernel_smp_shared` alone) for the same reason as the original carve: that's the cfg that
makes `PreemptGuard::new()` mask IRQs for the inner `NETWORK` hold, which is what keeps a
nested IRQ from ever observing this core "holding `NETWORK`, wanting the BKL."

**No Cargo.toml change was needed.** `no-bkl-network` has been part of the default
`smp-shared` bundle since 2026-07-24, so this rides along for free — same situation as
the original `netpoll_drain` carve (`BKL_VFS_CARVE_OUT.md` §20.5).

A boot self-test, `test_poll_bkl_drop` (`src/process_tests.rs`), drives the real
`sys_ppoll`/`sys_pselect6`/`sys_epoll_pwait` entry points: early-error paths (`nfds == 0`,
`maxevents <= 0`, a bad epfd) and a real path using a freshly created pipe's WRITE end
(`pipe_can_write` is true the instant the pipe exists, so `POLLOUT`/`EPOLLOUT` is ready on
the first loop iteration — every call returns without ever reaching `schedule_blocking`,
which is what makes it safe to drive from a boot self-test).

## 2. Piece 2 — what was attempted

The audit (`BKL_PHASE7_AUDIT.md` §3) named `ppoll`'s inner `EPOLL_TABLE` lock and every
per-fd-type primitive as reasons a *whole-syscall* carve (not just the `poll()` call)
might be safe, matching `VfsBklGuard`'s shape. Before building it, every fd type
`epoll_check_fd_readiness` (`poll.rs:384`) touches was read (not assumed) and traced to
its lock:

| fd type | lock touched | peer-PID risk? |
|---|---|---|
| `Socket` | `SOCKET_TABLE`/`NETWORK` (`socket::with_socket`, `smoltcp_net::with_network`) | none — no process lookup at all |
| `EventFd`/`TimerFd` | own `Spinlock` (`EVENTFDS`/`TIMERFD_TABLE`) | none |
| `ChildStdout`/`PidFd` | `CHILD_CHANNELS`/`PIDFD_TABLE`, independent of the process table (`process/children.rs:16`) | keyed by child pid, but never calls `lookup_process` |
| `PipeRead`/`PipeWrite`/`UnixSocket` | `PIPES` | none |
| `Stdin` | `current_channel()` | self-process only |
| rump socket | `rump_proxy::rump_socket_readable` → `lookup_process(read_current_pid())` | self-teardown-only case the audit's §2.1.1 says is safe |

Every arm held up: no fd type ever calls `lookup_process(other_pid)`. On that basis
`PollBklGuard` was built — a `VfsBklGuard`-shaped RAII guard (latched at construction,
runtime kill switch, `no-bkl-poll` feature) dropping the BKL for the whole readiness loop
of all three syscalls, constructed after each syscall's validation/fd-table-lookup
prologue so early-error returns stay outside the window.

It passed the boot self-test suite at SMP=2 and SMP=4 cleanly (`test_poll_bkl_drop`
PASSED, 0 PANIC/WILD/SPURIOUS, only the two known pre-existing failures). It did **not**
pass the contention A/B.

## 3. The A/B that caught it

Same-binary A/B, SMP=4, `release-smp-shared --features devbox-smoltcp,no-tests,bkl-profile`,
`MEMORY=4096`, the unmodified `net4 → read4 → cp2 → rm` regimen
(`scripts/bkl_smp_regimen/`), source-toggled (piece 2's code swapped in/out, feature set
otherwise identical):

| run | digests | `[BKL] stuck` | `stale dropped-window` | notes |
|---|---|---|---|---|
| piece 1 + piece 2, run 1 | **2/6 wrong** (`d1.bin`, `c1.bin`) | 0 | **1** | thread 17 crashed: `[Exception] Unknown from EL0: EC=0x22, ISS=0x0, Thread=17, ELR=0x2, FAR=0x2, SPSR=0x0`, then recycled |
| piece 1 + piece 2, run 2 (identical binary, fresh boot) | 6/6 exact | 0 | 0 | clean |
| piece 1 only (no piece 2) | 6/6 exact | 0 | 0 | clean |

`d1.sha == c1.sha` (both wrong, both equal to each other) — the corruption happened once,
at download time, and `cp` faithfully reproduced the already-wrong bytes; this is a
one-shot data corruption, not a torn/racy read on every access. The rerun with the
*identical* piece-1+piece-2 binary came back clean, and piece 1 alone was clean twice
(this run, and the boot self-test's own real-guarded-path case) — so this is a real but
**intermittent** race, not a deterministic bug in the arithmetic. Per the playbook's rule
6 ("verify data integrity, not just didn't crash") and the standing instruction to stop
if piece 2's premise doesn't hold up under verification, piece 2 was reverted the same
session — see §1 for what shipped instead.

## 4. Root cause (best evidence, not fully pinned)

`[BKL] stale dropped-window depth 1 healed at EL0 entry (tid=17)` — thread 17 reached a
fresh syscall entry with a leftover open window from a *previous* kernel excursion that
never ran its `Drop`. The two documented ways that happens
(`docs/reference/subsystems/locking.md`'s "leaked dropped-window depth" note) are a
fault-kill mid-window (the EL1 abandon-the-stack path, which `reset_dropped_windows()`
is designed to heal — and did, here) or a thread getting recycled while a window was
still open. Either way, the healing tripwire worked as designed; what it revealed is that
piece 2's window was open when something else went wrong.

The `EC=0x22` / near-zero `ELR`/`FAR` signature on thread 17's crash matches a signature
already documented elsewhere in this codebase: `src/process_tests.rs`'s
`test_munmap_teardown_conserves_pmm` comment states "the EL1 crash in meow.log (Thread0,
EC=0x22, garbage ELR/SP) is the signature of a still-live physical page being returned to
the PMM and re-handed to a second owner." The regimen's `net4` phase forks/execs four
`curl` processes per phase; each one's exit tears down its address space and frees pages
back to the PMM. `BKL_PHASE7_AUDIT.md` §2.1/§2.1.1 already names the process table as
**still load-bearing** for exactly this shape: `unregister_process`'s `Box::drop` and the
peer-core `Process` teardown at `process/mod.rs:1116`/`:1209` have no lock beyond
`with_irqs_disabled` (single-core only), and the audit's explicit conclusion is "nothing
about removing the BKL from syscall entry should be attempted before both halves [of
process-table locking] land" (§5, 7e).

Piece 1's window never spans a scheduling point — it wraps one `poll()` call, nothing
else runs in between. Piece 2's window could span many loop iterations, and each
iteration can call `schedule_blocking()` — a real context switch, real scheduler
activity, on a core that (unlike every syscall that keeps the BKL) is no longer serializing
against a peer core's process teardown. That is the most likely mechanism: piece 2 gave a
peer core's `curl` process-exit teardown a genuine window to race against this core's
still-open, now-unserialized excursion, landing on the exact hazard §2.1/§2.1.1 already
named and explicitly said blocks *any* further BKL-removal in this area. Not proven with
a minimal repro — flagged here so whoever revisits piece 2 starts from process-table
locking (7e), not from re-deriving this.

**Do not re-attempt piece 2 before 7e (process-table locking) lands.** That was already
the audit's ordering rationale for unrelated reasons (§5); this session's A/B is now a
second, independent reason.

## 5. Verification (piece 1, what shipped)

- **Clippy**, all three configs, clean: `--release`; `--profile release-smp-shared
  --features smp-shared`; `--profile release-smp-shared --features
  devbox-smoltcp,no-tests,bkl-profile[,no-bkl-irq]`.
- **Host tests**: `cargo test -p akuma-exec` — 156 passed, 0 failed (unchanged;
  `poll.rs` is bin-crate-only, so this phase's regression coverage is the boot self-test
  below).
- **Boot self-test suite**, `release-smp-shared --features smp-shared`, `MEMORY=2048`:
  - **SMP=2**: 0 PANIC/WILD/SPURIOUS, `test_poll_bkl_drop` PASSED, the same 2
    pre-existing unrelated failures as every prior phase.
  - **SMP=4** (2 boots): 0 PANIC/WILD/SPURIOUS, `test_poll_bkl_drop` PASSED both times,
    `smp_shared_cores_online` PASSED (3/3 secondaries). `test_epoll_multi_poller_pipe`
    (a pre-existing test, unrelated to this phase — two threads racing a 5 s epoll
    timeout) failed once out of three SMP=4 boots across this session's testing
    (including boots with piece 2 present) and passed the other two; flagged as
    pre-existing SMP=4 scheduling-jitter flakiness, not a regression from this phase.
- **Same-binary A/B**, SMP=4, `bkl-profile`, unmodified regimen, comparing a pristine
  (pre-7b) `poll.rs` against piece-1-only:

  | | pristine (pre-7b) | piece 1 only |
  |---|---|---|
  | `ppoll` share | 4.3% (4.60M spins) | **0.1%** (14.3K spins) |
  | digests (4 net + 2 cp) | 6/6 exact | 6/6 exact |
  | `[BKL] stuck` / stale-window / PANIC / WILD / SPURIOUS | 0 (6 stuck, pre-workload — see below) | all 0 |

  `ppoll` drops out toward the noise floor — the same "drops out entirely" signature
  every successful carve in this campaign has produced. This is one A/B across two
  separate boots (not a single-binary source toggle — piece 1 has no runtime toggle,
  matching `netpoll_drain`'s own shape), so unrelated tags swing between the two runs
  (`read` 56.4%→absent, `netpoll_maint` present only in the piece-1 run) — normal
  cross-boot variance, not signal. `ppoll`'s collapse is the one consistent thread across
  *three* separate piece-1-enabled boots this session (this A/B, and both piece-1+piece-2
  runs above), which is why it's reported as real. Per the campaign's standing rule,
  re-measure before quoting this number elsewhere. The pristine run's 6 pre-workload
  `[BKL] stuck` + 1 `RECOVERED` both clustered at t≈243s, well before the auto-selected
  workload window (t=760–840s) — the same "not workload signal" pattern documented in
  every prior phase.

## 6. What's next

Per `BKL_PHASE7_AUDIT.md` §5: **7c** (re-audit the already-carved `openat`/`read`/`accept`
residual, 11.9% for converted syscalls' prologue/epilogue), then **7d**
(`THREAD_CONTEXTS` ownership proof), then **7e** (process-table locking — access pattern
extension + deferred reclamation for the free path). Piece 2 of this phase is not
abandoned, but per §4 above it should not be re-attempted until 7e lands a real lock (or
epoch/RCU scheme) for peer-core `Process` teardown — at that point the same
`PollBklGuard` design this session built can very likely be re-tried unchanged, since the
gap was never in the fd-type lock audit.

---

## Background

- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) — §3 named `ppoll` as "merely habit"
  work; §2.1/§2.1.1 is the process-table finding this session's A/B independently
  reconfirmed; §5 is the 7a–7f decomposition.
- [`BKL_VFS_CARVE_OUT.md`](BKL_VFS_CARVE_OUT.md) §19–20 — the `netpoll_drain` carve,
  the exact mechanism piece 1 reuses at a second call site.
- [`BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md`](BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md) — 7a,
  landed the same session as this phase's prompt was written; same playbook, same
  verification bar.
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) — the
  playbook and the dropped-window-ledger correctness rules; the "leaked depth" note this
  doc's §4 relies on.
- `bkl-phase7-workplan.md` (deleted — a workplan is not a runbook; the live
remaining-work list is [`BKL_PHASE7F_OPTOUT_LIST.md`](BKL_PHASE7F_OPTOUT_LIST.md) §11) — Prompt C,
  the prompt this session executed.
