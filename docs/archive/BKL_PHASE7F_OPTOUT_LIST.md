# Phase 7f: the per-syscall BKL opt-out list (milestone 0, tranche 1, tranche 2)

**Status (tranche 2)**: Landed 2026-08-02, uncommitted at time of writing. Adds the
`as_lock` pre-flight (§3), the blocking-window analysis that gates every further
blocking conversion (§4), two conversions — `rt_sigprocmask` and `nanosleep` (§5) —
and the dead-thread ledger-clear fix that `nanosleep` exposed (§5.1). First tranche
of this phase to move the contention needle: `nanosleep` 6.7% → absent (§5.3).
Sections 1–2 below are tranche 1's original text, unchanged; they were committed in
`761d147`.

**Status (milestone 0 + tranche 1)**: Landed 2026-08-01. Milestone 0 (the
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

## 3. Tranche 2 pre-flight: the `as_lock` gap in the demand-paging helpers

§2's flagged follow-up, closed before any tranche-2 conversion.
`ensure_user_pages_mapped` (`src/syscall/mod.rs`) and its sibling
`ensure_user_page_mapped` (`src/exceptions.rs`) installed PTEs (`map_user_page`) and
recorded frames (`track_user_frame`/`track_page_table_frame`) with **no `as_lock`**,
and both are reachable from BKL-free windows today — every whole-fn net/vfs guard
calls `validate_user_ptr`, and tranche 1 made `getrandom`'s prologue join that class.
Both now fold the PTE install + frame bookkeeping under the address space's
`as_lock` (`Process::with_as_locked`), the same fix 7e applied to the three
signal-path PTE sites.

What had to stay outside the hold, and why — this is the part that is not
interchangeable with the 7e sites:

- **Frame allocation.** `exceptions.rs`'s helper allocates with
  `pmm::alloc_page_zeroed_user`, whose PMM-pressure path calls
  `reclaim_clean_file_pages`, which takes `as_lock` once per swept page
  (`process/children.rs`). Allocating under the hold would re-enter a
  non-reentrant `Spinlock` on the same core — the *exact* failure shape 7e's
  `register_process` on-demand reclaim hit (§3 of the reclaim doc). The
  `syscall/mod.rs` helper uses the kernel `alloc_page_zeroed`, whose only
  fallback is `allocator::reclaim_to_pmm` (heap spans, no `as_lock`), so it is
  not exposed — but the alloc is kept outside the hold there too, so the two
  helpers keep one discipline.
- **The file fill.** `ensure_user_pages_mapped`'s `LazySource::File` arm reads
  through the VFS into the fresh page. Block I/O under an IRQ-masked hold is
  barred outright.
- **The frees.** `free_page` runs after the hold is released.

Why the install and the tracking must be *together* inside the hold (they were two
separate steps before): `reclaim_clean_file_pages`'s `try_evict_ro_page` clears a
live RO PTE and then declines to free a frame it does not find tracked. A peer
observing the mapped-but-untracked instant therefore unmaps our page, frees nothing,
and we then track a frame that is no longer mapped — a re-fault leak, and the
`user_frame_count`-far-exceeds-mapped-VA signature that `mmu/mod.rs`'s
`user_frame_count` doc already names as the leak tell.

**Deviation from the workplan's assumption, recorded per the standing rule:** the
workplan said "fold under `with_address_space`, or a per-page `AsLockHold`". Neither
was used verbatim. `with_address_space` hands out `&mut UserAddressSpace`, but
`map_user_page` is a free function that resolves the L0 from `TTBR0_EL1` rather than
from an `AddressSpace`, so the closure would take the lock and then not use its own
argument; `AsLockHold` is `cfg(kernel_smp_shared)`-only and would need a `#[cfg]` at
the call site. `Process::with_as_locked` is the accessor that fits both — it takes
the same lock with the same IRQ masking and compiles to a plain call off `smp-shared`.

Two further deviations found while reading, both fixed here rather than deferred:

1. **The lock must be the L0 owner's, not the caller's.** `syscall/mod.rs`'s helper
   resolved only `read_current_pid()`. A CLONE_VM sibling gets a *fresh* `as_lock`
   from `fork_process` (`process/bkl_guard.rs`'s rule 1), so holding the current
   thread's would have excluded nothing. The lock owner is now resolved once per
   call via `address_space_owner_pid_for_fault()`. Frame *tracking* deliberately
   still goes to `read_current_pid()`'s process — changing which process owns the
   frames is a separate behavioural change, out of scope here. **Flagged for
   whoever picks that up:** for a shared-AS sibling this tracks frames in a
   `new_shared` view whose frames are documented (`mmu/mod.rs`'s
   `remove_user_frame`) as owned by the L0 owner instead.
2. **A frame leak on the lost-CAS path.** The two helpers disagreed on when to
   return the data frame. `exceptions.rs` freed it iff `!installed || owner.is_none()`
   (correct); `syscall/mod.rs` freed it iff `installed && owner.is_none()` — so every
   time `map_user_page`'s PTE compare-exchange lost the race to a concurrent
   installer with a live owner, the frame was neither mapped nor tracked nor freed.
   Both now use the first rule. BKL-free windows make that CAS race strictly more
   likely, so this was worth fixing before converting anything else.

### 3.1 Verification (pre-flight)

Boot suite, `release-smp-shared --features devbox-smoltcp`, `DISK=devbox.img
MEMORY=4096 SMP=2 INSTANCE=60`, A/B against a same-session `git stash` HEAD baseline:

| | PASSED (`grep -ac`) | `[Test] … PASSED` | FAIL | stuck | PANIC/WILD/RECOVERED/SPURIOUS/stale-heals |
|---|---|---|---|---|---|
| HEAD baseline (stashed) | 250 | 242 | 2 | 18 | 0 |
| pre-flight | 251 | 243 | 2 | 18 | 0 |

Delta is exactly the one new self-test. The 2 FAILs are the standing pre-existing
pair. **Note for future runs:** §1.2/§2.1 quote 249 PASS at SMP=2 for this same tree;
today's same-methodology HEAD baseline is **250** (242 `[Test]`). Cross-boot variance
in the non-`[Test]` PASS lines is real — take a fresh stash baseline in the same
session rather than diffing against a number in this doc.

New boot self-test `test_ensure_user_pages_mapped_as_lock` (`src/process_tests.rs`)
covers the bail-out path: a VA with no lazy region must leave the PMM free count
untouched (the leak class above) and leave no `as_lock` hold behind. It deliberately
does **not** drive the install path — that needs a real user address space in TTBR0,
and faking it would install PTEs into, and track page-table frames from, the *boot*
address space, so the test process's teardown would free live boot page-table frames
back to the PMM. Same call the `test_drivers_bkl_drop` comment makes about the RNG
read path. The install half is covered for real by every fork/exec in the suite.

## 4. The blocking-window analysis

This is the §2-flagged gate for tranche 2: which families may open a BKL-free window
that spans `schedule_blocking()`. It was written before converting anything that
blocks, because `BKL_PHASE7B_PPOLL_CARVE_OUT.md` §3–4 is the precedent — the one
blocking-window carve in this campaign produced real, intermittent data corruption
and was reverted the same session.

### 4.1 The finding that reframes the question

**The BKL is not held across a blocking wait — not for converted syscalls, and not
for unconverted ones either.** This is a property of the mechanism, read out of the
code rather than assumed:

- `KERNEL_LOCK` is a **per-core** lock (`bkl::current_core_id()` is the owner
  identity), not a per-thread one.
- `bkl::reconcile_for_spsr(spsr)` computes `release = target_is_el0 ||
  in_dropped_window()` and reconciles **for the thread the core is about to run**.
  So when thread A blocks and the core switches to thread B, the epilogue makes the
  lock state right for B. A's "hold" simply evaporates; nothing hands it back to A
  until A is resumed and its own epilogue re-acquires.
- `threading::schedule_blocking` never calls `leave_kernel` — it doesn't need to.
  It marks the thread WAITING, fires the voluntary-reschedule SGI, and `wfi`s; the
  scheduler switch that follows is what re-points the lock.

Consequences, and they cut both ways:

1. A "BKL-held" blocking syscall is really *BKL-held during each runnable stretch* —
   before the first block, and after each wake. The wait itself is unserialized
   today, for every syscall in the kernel.
2. Therefore converting a blocking syscall does **not** newly expose the wait. It
   changes the serialization of the runnable stretches only — exactly what
   converting a non-blocking syscall does.
3. Therefore any `Process`-derived reference a syscall carries *across* its wait is
   already unprotected today. The BKL was never covering it. That is a pre-existing
   bug class, not something a conversion creates — but a conversion must not
   *inherit* it silently.

This also explains why tranche 1 was uneventful: `accept`/`accept4`/`connect` and
`resolve_host` already ran blocking BKL-free windows, and have since Phase 2's
whole-fn `NetBklGuard`. Stronger still — `schedule_blocking`'s own wait loop calls
`process::is_current_interrupted()` → `current_process_shared()` on every `wfi`
iteration. A bounded process-table lookup from inside a blocking BKL-free window is
therefore **already production-proven on every boot**, months before this phase.

### 4.2 The rule this yields

> A blocking-window conversion is safe when every `Process`-derived reference in the
> excursion is re-acquired after each wake and used only within a bounded span
> (≪ the 10 ms `PROCESS_RECLAIM_COOLDOWN_US`). No reference, raw pointer, or derived
> pointer may live across `schedule_blocking()`.

Bounded lookup-then-use is precisely what 7e's RETIRED + 10 ms cooldown covers: the
lookup filters on `ACTIVE`, and the cooldown outlasts the microseconds to the deref.
A reference held across an indefinite wait is what it explicitly does not cover, and
`locking.md`'s load-bearing table says so in as many words.

A second, independent gate applies to every conversion, blocking or not, and the
workplan did not name it: **every inner lock the excursion takes must mask local
IRQs**, or a nested IRQ's unconditional `enter_kernel()` hard-spin deadlocks AB-BA
against a peer that holds the BKL and wants that inner lock (`locking.md`,
"Correctness rules learned the hard way"; the reason `PreemptGuard` masks IRQs).

### 4.3 Per-candidate verdicts

| candidate | reference held across the wait? | inner locks mask IRQs? | verdict |
|---|---|---|---|
| `nanosleep` (101) | **No.** The loop body is `timer::uptime_us()`, `is_current_interrupted()`, `schedule_blocking(deadline)`. `is_current_interrupted` re-looks-up every iteration and clones an `Arc<ProcessChannel>` — refcounted, so its lifetime is independent of the `Process` slot. Nothing survives a wake. | n/a — no inner table lock; per-thread atomics only | **Convertible now** |
| `rt_sigprocmask` (135) | n/a — never blocks, and touches no process-table state at all: the mask is per-thread (`threading::thread_signal_mask`) | n/a | **Convertible now** (non-blocking; listed here because the audit sits with its siblings) |
| `read` (63) on a pipe fd | **Yes.** `sys_read` binds `proc = current_process_shared()` (`fs.rs:259`) *before* the arm match and uses it inside the `PipeRead`/`Stdin` loops, across `schedule_blocking(u64::MAX)` — an indefinite wait, orders of magnitude past the 10 ms cooldown. | `PIPES` — yes, every access is wrapped in `with_irqs_disabled` | **Not cleared.** Per §4.1(3) this hazard exists *today* and conversion would not widen it, but it must be fixed, not inherited: re-acquire `proc` after each wake (or capture the pipe id and drop `proc` before the loop). Convert only after that. |
| `futex` (98) | No — the wait loop holds only `key = (tgid, uaddr)` and `deadline`, all plain values; `futex_key_tgid` returns a `u32`. | **No.** `FUTEX_WAITERS` is a bare `Spinlock` and `src/syscall/sync.rs` contains **zero** `IrqGuard`/`with_irqs_disabled`. | **Blocked** on the second gate. Converting futex makes a live AB-BA: window holds `FUTEX_WAITERS`, nested IRQ hard-spins for the BKL, peer holds the BKL inside `futex_do_wake` spinning on `FUTEX_WAITERS`. Latent today only because nothing BKL-free reaches it. Fix = wrap every `FUTEX_WAITERS` access in `with_irqs_disabled`, then re-audit. |
| `ppoll` piece 2 (73/72/22) | No — `kernel_fds` is a kernel `Vec`, the waker is per-thread, and `epoll_check_fd_readiness` re-resolves each fd per sweep. | Mixed: `PIPES` yes; `EPOLL_TABLE` is a bare `Spinlock` with only 4 IRQ-masking sites in `poll.rs` — needs the same sweep as futex before the whole-syscall carve. | **Not cleared** — see §4.4. |

### 4.4 Why `ppoll` piece 2 stays parked

By the §4.2 rule alone `ppoll` would pass: it carries nothing across its wait. That
is exactly why it is not enough. 7b's root cause was **never pinned** — §4 of that
doc offers the peer-teardown hypothesis as best evidence, not as a diagnosis. 7e has
since landed the fix for that hypothesis, and 7b §6 anticipated that `PollBklGuard`
could then "very likely be re-tried unchanged". But:

- The observed failure rate was **1 run in 2**. A single clean regimen run is not
  evidence — 7b's own run 2 was clean *with the bug present*. Clearing it needs a
  run count chosen against that rate (≥4 clean regimen runs puts a ≥50% failure mode
  below ~6% likelihood), not the single A/B the other tranches use.
- `EPOLL_TABLE`'s IRQ-masking gap (§4.3) is an unrelated second defect that a
  whole-syscall carve would activate.

Both are concrete and cheap relative to the risk; neither was in scope for this
tranche. Parked with those two as the entry criteria, replacing 7b §6's "wait for
7e" (now satisfied).

### 4.5 Conclusion

Cleared and converted in tranche 2a/2b: `rt_sigprocmask`, `nanosleep`.
Cleared-in-principle but gated on a named prerequisite fix, in ascending cost:
`futex` (mask IRQs around `FUTEX_WAITERS`), `read` (re-acquire `proc` after wake),
`ppoll` piece 2 (mask IRQs around `EPOLL_TABLE`, then a ≥4-run regimen). None of the
three needs an epoch/RCU scheme — that remains reserved for the plain-`Process`-field
writers and the two `with_process_exclusive` sites, which are still the last across.

Two candidates the workplan listed as "easy auditables" were **rejected** on reading:

- **`getcwd` (17)** — `locking.md` calls it "no I/O, cached `proc.cwd`, no guard
  needed", which is true about the *VFS* and misleading about the BKL. `proc.cwd` is
  a plain `String` field on `Process`; `chdir`/`fchdir` write it BKL-held. Converting
  the reader would let a BKL-free `getcwd` read a `String` whose heap buffer a peer's
  `chdir` is reallocating — a torn read at best. This is precisely the
  plain-`Process`-field row the workplan's rule 5 puts out of scope; the rule applies
  to readers of those fields, not only writers.
- **`sendto`/`recvfrom`/`sendmsg`/`recvmsg`** — unchanged from tranche 1. The body
  split was not attempted: `DroppedWindowPause` holds the BKL, so the unix-socket arm
  it would wrap must not span the pipe blocking wait, and getting that boundary wrong
  reproduces the meow→LLM freeze. Each split syscall needs its own boot, which this
  tranche did not have room for.

## 5. Tranche 2: what landed

Converted, one at a time, each boot-verified at SMP=2 before the next:

| # | syscall | why it was clear |
|---|---|---|
| 135 | `rt_sigprocmask` | per-thread signal mask (plain atomics); zero process-table touch |
| 101 | `nanosleep` | blocking, but carries no `Process`-derived reference across the wait (§4.3) |

### 5.1 `nanosleep` found a real mechanism gap — the dead-thread ledger leak

Listing `nanosleep` turned `test_no_stale_window_heals` red **deterministically**: 3
heals, identical across two boots, each log line immediately preceded by
`[Cleanup] Thread 8 recycled after Nus cooldown`.

That is documented root cause #2 from `BKL_PHASE7B_PPOLL_CARVE_OUT.md` §4 ("a thread
getting recycled while a window was still open"), reproduced deterministically for
the first time. The dropped-window ledger is indexed by **thread id**. A thread killed
while parked inside a converted syscall never reaches its window close, so its slot
goes back to `FREE` with a stale depth, and the *next* occupant of that tid inherits
it — its EL1 excursions then run BKL-free until the EL0-entry tripwire heals them.

`nanosleep` is simply the first converted syscall that parks a thread long enough for
the kill-then-recycle to land inside the window. Nothing about the bug is specific to
it; every future blocking conversion would have hit it.

Fix: `bkl::clear_dropped_windows_for_dead_thread(tid)` — a ledger-only clear (no lock
operation, unlike `reset_dropped_windows`, because a TERMINATED thread will never
resume and there is no invariant to restore) called from
`threading::reclaim_terminated_slots` immediately before the slot goes `FREE`, i.e.
before any spawn can claim it. `DroppedWindowLedger::reset`'s own doc comment already
said "and for recycled thread slots"; the caller had simply never been wired up.

`test_syscall_bkl_optout` gained a case for it: stage a window on a foreign tid, clear
it, assert the returned prior depth, the zeroed residual, and that this core's BKL
state is untouched.

**This is the tranche's most portable result.** It is a prerequisite for `read`,
`futex`, and `ppoll` piece 2 as much as it was for `nanosleep`, and it retires one of
the two mechanisms 7b §4 offered for its unexplained corruption — which is
independently interesting, since 7b's window also spanned a scheduler and its thread
17 was recycled.

### 5.2 Verification

Boot suite, `release-smp-shared --features devbox-smoltcp`, `DISK=devbox.img
MEMORY=4096 INSTANCE=60`. `[Test] … PASSED` is the stable count; `grep -ac PASSED`
is given too for continuity with §1.2.

| | `[Test]` PASSED | `grep -ac PASSED` | non-flake FAIL | stuck | heals | PANIC/WILD/RECOVERED/SPURIOUS |
|---|---|---|---|---|---|---|
| HEAD baseline, SMP=2 (stash) | 242 | 250 | 2 known | 18 | 0 | 0 |
| tranche 2 complete, SMP=2 | 243 | 251 | 2 known | 18 | 0 | 0 |
| tranche 2 complete, SMP=1 | 235 (+15 SKIP) | 243 | 2 known | 0 | 0 | 0 |
| tranche 2 complete, SMP=4 | 240 (+9 SKIP) | 248 | 2 known | 46 | 0 | 0 |

All four suite-end tripwires PASSED on every boot above
(`no_spurious_svc_traps`, `no_bkl_ticket_recoveries`, `no_stale_window_heals`,
`syscall_bkl_optout`), plus the new `ensure_user_pages_mapped_as_lock`.

Host: full-workspace `cargo test` green (akuma-exec 158 passed, 0 failed anywhere);
clippy clean on `release-smp-shared --features devbox-smoltcp`, `--release`, and
`release-smp --features smp`; `size` and `extreme-size` both build.

### 5.3 Contention A/B — the first tranche with a real delta

Same-binary-configuration A/B, SMP=4, `release-smp-shared --features
devbox-smoltcp,no-tests,bkl-profile`, `MEMORY=4096`, `SNAPSHOT=1`, the unmodified
`net4 → read4 → cp2 → rm` regimen, toggled **in source** (seed entries commented in/
out, feature set byte-identical), summed over `analyze_workload.py --auto`'s
auto-selected workload windows:

| | side B (tranche-1 seed) | side A (tranche-2 seed) |
|---|---|---|
| total contended workload spins | 26.31M | 26.78M |
| **`nanosleep`** | **6.7%** (1.67M spins) | **absent** |
| `execve` | 21.6% | 28.1% |
| `clone` | 16.7% | 19.9% |
| `irq/sched` | 14.7% | 17.6% |
| `netpoll_maint` | 13.8% | 11.4% |
| `idle` | 19.9% | 10.1% |
| digests / stuck / PANIC / WILD / SPURIOUS / stale / RECOVERED | 6/6 exact, all 0 | 6/6 exact, all 0 |

**`nanosleep` drops out of the histogram entirely** — 6.7% → 0, the same "drops out"
signature every successful carve in this campaign has produced, and the first real
contention delta of Phase 7f (tranche 1 was all previously-guarded syscalls, so it
correctly showed none).

Read the rest of the table carefully, because it is easy to over-claim:

- **Total contended spins did not fall** (26.31M → 26.78M, within noise). The freed
  share is *redistributed* to `execve`/`clone`/`irq/sched`, not eliminated. This
  workload is lifecycle-bound, and those are exactly the syscalls Phase 7f is
  forbidden to touch until the plain-`Process`-field story lands. Removing a 6.7%
  holder from a saturated lock moves the queue along; it does not shrink it.
- **`read` (2.9% side B, absent side A) is NOT this tranche's doing** — `read` is
  unconverted on both sides. That is cross-boot variance of exactly the kind 7b
  documented (`read` 56.4% → absent between two of its runs). Ignore it.
- The regimen wall-clock was 92s (side A) vs 136s (side B). Suggestive, but n=1 per
  side and the harness timing includes host HTTP variance — **not** quotable as a
  speedup without repeated runs.

Per the campaign's standing rule: re-measure before quoting any of these numbers.

### 5.4 `test_epoll_multi_poller_pipe` is flaky at SMP=2, not only SMP=4

Worth recording because it cost a conversion decision. After listing
`rt_sigprocmask`, this test failed (`woken=1`, expected 2) on two consecutive SMP=2
boots — which looks exactly like a regression, and the workplan only documents the
flake at SMP=4. Backing the entry out and re-running produced **1 failure in 4** boots
*without* the change; going back in produced 2 more clean boots (5 with-change boots
total, 2 failures).

So: 2/5 with, 1/4 without — the same population, no separable effect, and
`rt_sigprocmask` touches neither epoll nor pipes. The test is a pre-existing SMP=2
flake too. **The lesson for the next tranche: at this failure rate a single boot
cannot distinguish a regression from the flake.** Budget the extra boots, or the
first suspicious result will either cost a good conversion or hide a bad one.

## 6. What this unblocks

The opt-out list is now the single place the remaining traversal happens: each of the
14 untouched syscall families (and the ~13 leftover `fs` syscalls) is one seed entry
+ one audit + one A/B away, with `execve`/`clone`/`wait4`/mm expressly **out of
scope** until the plain-`Process`-field story lands (the two
`with_process_exclusive` sites remain the last two across). `KernelLock`,
`reconcile_for_spsr`, the ledger, and the five guards must survive until the
un-converted set is empty — the ledger's invariant is precisely what makes the mixed
state safe.

After tranche 2 the traversal's remaining work is no longer "audit each syscall" but
four **named, shared prerequisites**, each unblocking a group at once:

1. **Mask IRQs around the bare `Spinlock` tables** — `FUTEX_WAITERS`
   (`src/syscall/sync.rs`, zero IRQ-masked sites today) and `EPOLL_TABLE`
   (`src/syscall/poll.rs`). Unblocks `futex` and `ppoll` piece 2. Mechanical; the
   pattern is `pipe.rs`'s `with_irqs_disabled` wrapping.
2. **Stop carrying `proc` across blocking waits** — `sys_read` binds it before the
   arm match (`fs.rs:259`) and derefs it after an indefinite park. Unblocks `read`
   (4.4% measured), and the same shape should be swept for in `write`/`recvfrom`.
   Note this is a live pre-existing bug, not merely a conversion blocker (§4.1).
3. **The plain-`Process`-field story** — still the gate for `execve`/`clone`/`wait4`/
   mm, and now also for *readers* of those fields such as `getcwd` (§4.5).
4. **Body splits for the four unix-socket-routing net syscalls**, with the
   `DroppedWindowPause`/blocking-wait boundary rule from §4.5.

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
