# Phase 7f milestone 0 + tranche 1: the per-syscall BKL opt-out list

**Status**: Landed 2026-08-01 (uncommitted at time of writing). Milestone 0 (the
mechanism, seeded empty) verified behaviour-identical against a same-day HEAD baseline
boot; tranche 1 (the `no-bkl-network` whole-fn-guard family + `getrandom`) converted
one step at a time on top of it. Executes
[`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) §7.3 — *don't
remove the BKL, invert its default* — which
[`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §5 (7f) made the canonical replacement
for the plan's original removal tasks.

## 1. The mechanism (milestone 0)

`rust_sync_el0_handler` (src/exceptions.rs) goes from *"always `enter_kernel`"* to
*"acquire unless this syscall's number is on the opt-out list"*:

- **The decision is latched once at entry** (a local, computed from the trap's EC and
  the frame's saved x8 before any lock op) and reused verbatim on every exit path —
  the guard-latching rule from locking.md, applied to the syscall wrapper itself. A
  runtime toggle flip mid-syscall cannot unbalance entry against exit.
- **An opted-out excursion is one open dropped-BKL window.** Entry calls
  `dropped_window_open()` *without* a prior acquire (its internal `leave_kernel` is
  the documented idempotent no-op, and the genuine release on the
  just-healed-tripwire corner); exit calls the new
  `bkl::dropped_window_close_no_reacquire()`. Everything in between — IRQ epilogues
  (`reconcile_for_spsr` consults the ledger and keeps the lock *released* instead of
  silently re-acquiring-and-holding), preemption, cross-core migration, blocking
  waits, and the body's now-redundant carve-out guard (which nests as an ordinary
  depth-2 window) — sees exactly the ledger state the six existing carve-outs already
  proved out. This is §7.3's "a converted syscall is precisely a permanently-open
  dropped window", implemented literally.
- **The EL0-entry stale-depth tripwire runs before the window opens**, so it can
  never mistake the excursion's own legitimate window for a leak, and it now bumps a
  public counter (`exceptions::stale_window_heal_count()`) asserted 0 by a suite-end
  boot test (`test_no_stale_window_heals`) — the "0 stale-depth heals" pass criterion
  as a self-test rather than a log grep.
- **The list** lives in `src/smp_shared.rs`: a 512-bit atomic bitmap
  (`SYSCALL_BKL_OPTOUT`), seeded at compile time from `SYSCALL_BKL_OPTOUT_SEED`
  (milestone 0 = empty), with `set_syscall_bkl_optout(nr, on)` as the per-syscall
  runtime kill switch / same-binary A/B handle. A structural deny list refuses
  `exit`/`exit_group`/`rt_sigreturn` (93/94/139) — never-return teardown and the
  sigreturn prologue must stay BKL-held by construction, not by convention.

### 1.1 Never-return and must-stay-held paths

Four shared-path consumers of "syscall entry holds the BKL" needed explicit handling
(this was the design core the ledger rules demanded):

| path | handling |
|---|---|
| `return_to_kernel` (process exit reached from *inside* a dispatched syscall — the `proc.exited` check, `exit`/`exit_group` arms) | resets the ledger + restores the held state at its top, mirroring `return_to_kernel_from_fault`'s existing reset. Depth-0 no-op for every non-opted-out caller. |
| the deferred thread-kill at the EL1→EL0 boundary | for an opted-out excursion, closes the window and takes the lock before `mark_thread_terminated` + the terminal yield loop — that path documents it must run BKL-held, and it never returns to EL0. |
| `handle_syscall`'s interrupted-process arm (plain `Process`-field writes via `with_current_process` — IRQ-mask is same-core-only exclusion; the field family locking.md's load-bearing table still assigns to the BKL) | new `bkl::DroppedWindowPause` RAII — closes all open windows and acquires (via `reset_dropped_windows`), reopens the latched count on drop. No-op when no window is open. |
| the phantom-SVC give-up path and the QEMU STP-XZR-misroute demand-page path (reachable with a garbage x8 that happens to be a listed number) | `DroppedWindowPause` at the top of each claim body — cold, QEMU-artifact-only paths; their lifecycle/PTE work keeps its pre-7f BKL-held posture. |

The remaining shared pre/post-dispatch path was audited piece by piece for the
BKL-free case: the spurious-SVC entry guard (user-memory reads + per-thread atomics),
the JIT `>500` guard (numbers ≥ 512 are never listable), the DC-ZVA misroute
emulation (pure user-memory write, same exposure class as any in-window
`copy_to_user`), holder-tag stamping (`set_holder_tag`, atomics — kept, so
`bkl-profile` attribution and `[BKL] stuck` holder tags stay correct for opted-out
syscalls), the TLS sync + trap-frame bookkeeping (own-thread state), the
`proc.exited` epilogue read (aligned scalar read; the miss-then-catch-next-boundary
race predates this phase), and end-of-syscall signal delivery (`try_deliver_signal` —
`signal_actions` has its own `Spinlock`, sigaltstack state is per-thread atomics, and
the PTE fix-ups were folded under `as_lock` by 7e).

### 1.2 Verification (milestone 0, seeded empty)

Boot suite (`release-smp-shared --features devbox-smoltcp`, `DISK=devbox.img
MEMORY=4096 INSTANCE=60`), counted as `grep -ac PASSED/FAILED/"[BKL] stuck"`:

| | PASS | FAIL | stuck | PANIC/WILD/RECOVERED/stale-heals |
|---|---|---|---|---|
| HEAD baseline, SMP=2 (stash → boot → unstash) | 247 | 2 | 22 | 0 |
| milestone 0, SMP=2 | 249 (= 247 + the 2 new self-tests) | 2 | 23 | 0 |
| milestone 0, SMP=1 | 241 | 2 | 0 | 0 |
| milestone 0, SMP=4 | 247 (+2 by-design SMP=2-only SKIPs) | 2 | 63 | 0 |

The 2 FAILs are the standing pre-existing pair (`PermissionDenied -> EPERM`,
`stp_xzr_ec15_handler_fires`); the stuck counts match the documented pre-existing
HEAD levels (~22 at SMP=2, ~67 at SMP=4, `tag=511` — no profiler compiled in). (The
workplan quoted 341/349/346 for the same boots — that count included exactly 100
non-`[Test]` PASS-marker lines; the same-methodology stash-baseline A/B above is the
apples-to-apples comparison.)

New boot self-tests (`src/process_tests.rs`): `test_syscall_bkl_optout` (list
set/query + deny list; ledger balance across a real opted-out `handle_syscall`
dispatch whose body guard nests; the kill switch flipping mid-excursion against the
latched decision; `DroppedWindowPause` semantics; `kernel_lock_recoveries()` delta
0) and `test_no_stale_window_heals` (suite-end tripwire assertion).

## 2. Tranche 1 — the already-carved conversions

Converted (seeded), in landing order, each boot-verified at SMP=2 with the full
tripwire set and the counts above before the next moved:

`socket` (198) → `bind` (200) + `listen` (201) → `accept` (202) + `accept4` (242) →
`connect` (203) → `getsockname` (204) + `getpeername` (205) → `setsockopt` (208) +
`getsockopt` (209) → `resolve_host` (300) → `getrandom` (278).

Audit notes (the per-syscall "code outside the guard" pass):

- The net family's guards are **whole-fn** (`NetBklGuard` constructed on the first
  line), so conversion moves nothing new out from under the lock — only the
  dispatch-arm argument casts sit between `handle_syscall`'s match and the guard, and
  the shared prologue/epilogue is §1.1's audited surface. The `#[cfg(kernel_smp)]`
  cross-core forward arms do not co-compile with `smp-shared` (build.rs enforces the
  exclusion), so the multikernel bounce constraint is moot here.
- `accept`/`accept4`/`connect` (and `resolve_host`'s DNS wait) block via
  `blocking_relax()`/`schedule_blocking` *inside their existing whole-fn windows
  today* — conversion is behaviour-preserving for the process-table hazard the 7b
  revert documented (`BKL_PHASE7B_PPOLL_CARVE_OUT.md` §4): the window's span over the
  scheduler is unchanged, no window was widened. The 7e Free-half (RETIRED + 10ms
  cooldown) covers the bounded windows; the blocking-window exposure is exactly what
  it was yesterday.
- **`sendto`/`recvfrom`/`sendmsg`/`recvmsg` are deliberately NOT converted**: their
  unix-socket (pipe-backed) routing arm runs before the guard and must stay BKL-held
  (the nested-IRQ AB-BA note in locking.md's syscall→lock map). Converting them
  wholesale would move that arm BKL-free; they need a body split first.
- `getrandom` is the one entry whose conversion moves real code out from under the
  BKL: `validate_user_ptr` ran *before* its `DriverBklGuard`. That helper can
  demand-page lazy regions (`ensure_user_pages_mapped` → `map_user_page` + frame
  tracking, **no `as_lock`**). This is a pre-existing exposure, not a new one — the
  identical helper already runs inside every whole-fn net/vfs window today — but the
  conversion makes getrandom join that class. **Follow-up flagged**: fold
  `ensure_user_pages_mapped` (and its exceptions.rs sibling `ensure_user_page_mapped`)
  under `with_address_space`, the same fix 7e applied to the three signal-path PTE
  sites.
- **The now-redundant body guards stay in place.** For a listed syscall they
  self-neutralize to a nested depth bump (open: +1 and an idempotent no-op release;
  close: −1, not outermost, no re-acquire) — and they are what makes the runtime
  kill switch complete: removing a syscall from the list restores today's exact
  guard-scoped behaviour without a rebuild. They become deletable only at the end of
  the traversal, together with the ledger (§7.3's endgame).

### 2.1 Verification (tranche 1)

- Boot suite per conversion step at SMP=2: identical counts (249 PASS / 2 known
  FAILs / ~22 stuck / 0 PANIC / 0 WILD / 0 RECOVERED / 0 stale heals / 0
  SPURIOUS-SVC), `test_syscall_bkl_optout` + `test_no_stale_window_heals` PASSED
  every boot. The boots themselves exercise the converted family for real: the
  devbox's userspace sshd serves the suite's SSH tests over converted
  `socket`/`bind`/`listen`/`accept4`/`setsockopt`.
- Full-matrix re-verification with the complete tranche: SMP=1/2/4 counts match
  milestone 0's exactly (241 / 249 / 247+2SKIP, same 2 FAILs, stuck 0 / ~22 / ~63,
  all tripwires 0).
- Live-SSH data integrity at SMP=4 (not just "didn't crash"): interactive session
  over converted syscalls; 8 MiB random-file write → readback `md5sum` equality
  in-VM and across the wire; concurrent in-VM HTTP downloads (converted
  socket/connect/DNS path) with byte-identical repeated fetches.
- Host: full workspace `cargo test` green; clippy clean on
  `release-smp-shared --features devbox-smoltcp`; `release`, `release-smp
  --features smp`, `size`, and `extreme-size` profiles all build (the two
  pre-existing dead-code breaks in size/extreme — `pipe_write_all_blocking`,
  `any_fd_wants_rump_poll_interval` — fixed as pre-flight in this session).

### 2.2 Contention A/B (SMP=4, standing regimen)

Same-binary `bkl-profile` A/B on the standing `net4 → read4 → cp2 → rm` regimen
(`scripts/bkl_smp_regimen/`), toggled in source (seed emptied vs tranche-1 seed,
feature set byte-identical), summed over the auto-detected workload windows
(`analyze_workload.py --auto`):

| | side B (seed emptied) | side A (tranche-1 seed) |
|---|---|---|
| total workload spins | 21.25M | 23.33M |
| top holders | execve 27.2%, netpoll_maint 19.2%, idle 18.1%, clone 15.8%, irq/sched 12.8% | execve 28.6%, idle 15.7%, clone 14.2%, irq/sched 12.0%, netpoll_maint 10.8% |
| any tranche-1 syscall in the histogram | none | none |
| digests / stuck / PANIC / WILD / stale / RECOVERED | 6/6 exact, all 0 | 6/6 exact, all 0 |

As the workplan predicted: **no contention delta above the noise floor** — the
converted syscalls were already whole-fn-carved, so the only thing conversion removes
is the entry/exit lock round-trip, which never showed up as a contended holder in the
first place (the ~10% total-spin difference is single-run cross-boot variance, carried
by `netpoll_maint`/`nanosleep` swings, not by any converted syscall). The A/B's value
here is the correctness half: identical digests and zero tripwires with the mechanism
live under the standing contention regimen. Re-measure before quoting.

One additional data point from the live-SSH integrity pass: large (8 MiB) output over
a single userspace-sshd exec channel truncates at a varying ~2.6–3.8 MB point. This
was stash-baselined against HEAD the moment it was seen and **reproduces identically
there** — a pre-existing sshd exec-channel drain gap (the known
keepalive/channel-teardown family), not a Phase 7f regression. In-VM md5s (write,
`cp`, readback) and concurrent-download digests are exact on the tranche build.

## 3. What this unblocks

The opt-out list is now the single place the remaining traversal happens: each of the
14 untouched syscall families (and the ~13 leftover `fs` syscalls) is one seed entry
+ one audit + one A/B away, with `execve`/`clone`/`wait4`/mm expressly **out of
scope** until the plain-`Process`-field story lands (the two
`with_process_exclusive` sites remain the last two across). `KernelLock`,
`reconcile_for_spsr`, the ledger, and the five guards must survive until the
un-converted set is empty — the ledger's invariant is precisely what makes the mixed
state safe.

## Background

- [`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) §7.3 — the
  canonical statement this phase implements.
- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §5 — the 7a–7f decomposition and the
  7f entry's ledger-survival constraint.
- [`BKL_PHASE7E_ACCESS_HALF.md`](BKL_PHASE7E_ACCESS_HALF.md) /
  [`BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md`](BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md) —
  the preconditions this phase was gated on.
- [`BKL_PHASE7B_PPOLL_CARVE_OUT.md`](BKL_PHASE7B_PPOLL_CARVE_OUT.md) §3–4 — the
  blocked-window corruption that bounds what tranche 1 may contain.
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) — the
  ledger correctness rules the design was reviewed against; updated alongside this
  doc.
