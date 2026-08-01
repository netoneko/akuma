# Phase 7a: the alarm queue's real lock, and a BKL-free timer IRQ

**Status**: Landed 2026-08-01. Default-on in `smp-shared` since 2026-08-01,
after the A/B in §5 validated it.
**Feature**: `no-bkl-irq` → `cfg(kernel_no_bkl_irq)`
**Toggle**: `smp_shared::irq_bkl_drop_enabled()` / `set_irq_bkl_drop_enabled()`
(default **on**)

This is 7a of the decomposition in
[`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §5: "give `ALARM_QUEUE` a real
`Spinlock`, and make the `critical_section` impl per-core — or drop the
`critical_section` dependency entirely." Executed per the prompt in
[`../runbooks/bkl-phase7-workplan.md`](../runbooks/bkl-phase7-workplan.md).

## 1. What changed

1. **`src/kernel_timer.rs`**: `ALARM_QUEUE` was a
   `critical_section::Mutex<RefCell<[ScheduledWake; 8]>>`. The kernel's own
   `critical_section::Impl` (`CS_NESTING`/`CS_SAVED_DAIF`, a **process-global**
   nesting counter) gave no cross-core exclusion at all — under `smp-shared`,
   core A's `acquire` and core B's `release` shared the same counter, so a
   concurrent pair could restore DAIF while a critical section was still open
   elsewhere. The BKL hid this by serializing all of EL1; nothing before this
   phase exercised it concurrently enough to matter. Replaced with a real
   `spinning_top::Spinlock<[ScheduledWake; 8]>`: `schedule_wake` (called from
   ordinary EL1 code with IRQs enabled) wraps the hold in `crate::irq::IrqGuard`
   so a same-core timer tick can't self-deadlock on the lock; `on_timer_interrupt`
   (called from IRQ context, where the CPU has already masked IRQs since
   exception entry) needs no guard. The `critical_section` crate dependency is
   now unused and removed from `Cargo.toml` — it was otherwise dead in this
   binary (confirmed: `atomic-polyfill`, the only transitive user via
   `embedded-tls`, compiles its `reexport_core` path on this target since
   aarch64 isn't in its polyfill-target list, so it never called
   `critical_section::with` here either).
2. **`src/exceptions.rs`** (`rust_irq_handler_with_sp`): the timer is the only
   device IRQ this kernel registers (`irq::register_handler(27,
   timer::timer_irq_handler)`; confirmed in `BKL_DRIVERS_CARVE_OUT.md` §1). Its
   handler no longer touches anything the BKL alone protects: the alarm queue
   has its own `Spinlock` (above), the preemption watchdog and
   fork-in-progress check are per-thread/global atomics
   (`crates/akuma-exec/src/threading/mod.rs` `PREEMPTION_DISABLED*`), and
   `trigger_sgi_self`/GIC ack/EOI are raw MMIO (`src/gic.rs`, `src/gic_v3.rs`).
   Under `cfg(all(kernel_smp_shared, kernel_no_bkl_irq))`, a device IRQ (i.e.
   `irq_opt.is_some() && irq != SGI_SCHEDULER`) now dispatches and EOIs without
   ever calling `enter_kernel`/`reconcile_for_spsr`. This is a different shape
   from every prior carve-out: there is no dropped-BKL "window" to open/close,
   because there is no `enter_kernel` on this path to balance in the first
   place. No context switch happens here either, so the interrupted thread's
   BKL hold state (held or not, EL0 or EL1) is left completely untouched — the
   correctness argument is "nothing to reconcile," not "reconcile correctly."
3. **`src/smp_shared.rs`**: `irq_bkl_drop_enabled()`/`set_irq_bkl_drop_enabled()`,
   mirroring every other phase's runtime kill switch.
4. **`build.rs`/`Cargo.toml`**: `cfg(kernel_no_bkl_irq)` from
   `CARGO_FEATURE_NO_BKL_IRQ`, a `no-bkl-irq` feature. **Not** added to the
   default `smp-shared` bundle yet — same staging discipline as `no-bkl-mm`
   (§6 below).
5. **Tests removed**: `test_critical_section_daif_preserved` /
   `test_critical_section_nesting` (`src/tests.rs`) tested the now-deleted
   `critical_section::Impl` directly; `test_irqguard_nesting_preserves_state`
   and `test_with_irqs_disabled_nesting` (same file) already cover the same
   DAIF-nesting invariant for `IrqGuard`, so nothing was lost.
6. **Test added**: `test_timer_irq_preserves_bkl_state`
   (`src/process_tests.rs`, `cfg(kernel_smp_shared)`) — see §4.

## 2. Why this shape, not a dropped-window guard

Every prior carve-out (`VfsBklGuard`, `NetBklGuard`, `MmBklGuard`,
`DriverBklGuard`) wraps a **syscall's** BKL-held excursion and temporarily
drops the lock for a bounded sub-window, using
`akuma_exec::bkl::dropped_window_open`/`_close` so a timer IRQ landing inside
the window doesn't silently re-acquire the BKL for its remainder (the
`[BKL] stuck` regression `BKL_VFS_CARVE_OUT.md` §8 describes). The timer IRQ
path is not a syscall excursion at all — it's the *thing* that used to force
that reconciliation. There is no outer `enter_kernel` to be inside of, so the
ledger doesn't apply here; the guard is a single `if` in the IRQ dispatcher,
latched by the runtime toggle at each entry (no construct/drop discipline
needed, since there's no object).

## 3. Correctness argument, checked against source (not inferred)

Per-item verification before landing:

| touched by `timer_irq_handler` | lock-free? | where |
|---|---|---|
| `kernel_timer::on_timer_interrupt` (alarm queue) | yes, own `Spinlock` (this phase) | `src/kernel_timer.rs` |
| preemption watchdog reads/writes | yes, per-thread atomics | `crates/akuma-exec/src/threading/mod.rs:1232-1425` (`PREEMPTION_DISABLED*`) |
| `FORK_IN_PROGRESS` check | yes, `AtomicBool` | `akuma_exec::process` |
| GIC ack / EOI / `trigger_sgi_self` | yes, raw MMIO | `src/gic.rs`, `src/gic_v3.rs` — same calls the already-BKL-free M5c scheduler-SGI path uses |
| `IRQ_HANDLERS` dispatch table | yes, own `Spinlock`, independent of BKL | `src/irq.rs` |
| `safe_print!` | yes, heap-free, documented secondary-safe | `src/main.rs` |

Nothing else in `timer_irq_handler` was found. The only thing the BKL was
still protecting on this path was the alarm queue's *own* locking bug (§1.1),
which this phase fixes directly.

## 4. Boot self-test

`test_timer_irq_preserves_bkl_state` pins the invariant directly: this core's
`bkl::held_by_current()` must read identically before and after a stretch
spanning several real timer ticks (50 ms busy-wait, ticks are ~10 ms), whatever
it started at — held or not. That's the right invariant because the fast path
never touches the lock at all, so "unchanged" is the only correct outcome
regardless of ambient state; a leaked acquire, a spurious release, or any other
pairing break on the new path would flip it. A second pass repeats the check
with the runtime kill switch forced off, to confirm the fallback (BKL-held)
path preserves the same invariant on its own terms.

## 5. Verification

- **Clippy**, all three configs, clean: `--release`; `--profile
  release-smp-shared --features smp-shared`; `--profile release-smp-shared
  --features devbox-smoltcp,no-tests,bkl-profile[,no-bkl-irq]`.
- **Host tests**: `cargo test -p akuma-exec` — 156 passed, 0 failed (unchanged
  from the pre-existing baseline; `kernel_timer.rs` is a bin-only file, not
  host-testable, so this phase's own regression coverage is the boot
  self-test above).
- **Boot self-test suite**, `release-smp-shared --features
  smp-shared,no-bkl-irq`, `MEMORY=2048`:
  - **SMP=2**: 0 PANIC/WILD/SPURIOUS, `test_timer_irq_preserves_bkl_state`
    PASSED (`held=true`), the same 2 pre-existing unrelated failures as every
    prior phase (`PermissionDenied -> EPERM` errno mapping,
    `stp_xzr_ec15_handler_fires` — QEMU-dependent, self-documenting).
  - **SMP=4**: same result, `test_timer_irq_preserves_bkl_state` PASSED
    (`held=true`), `smp_shared_cores_online` PASSED (3/3 secondaries).
  - Both boots also showed **19** (SMP=2) / **60** (SMP=4) whole-boot
    `[BKL] stuck` log lines outside the workload windows. **Confirmed
    pre-existing and unrelated**: bisected by rebuilding pristine HEAD (before
    any of this phase's edits) at SMP=2 — identical count (19). Not
    investigated further here; out of scope for 7a. (Likely the
    cooperative-wait-loop test in `tests.rs`'s threading suite —
    "Waiting for threads to complete..." precedes the bursts — hitting the
    known class of issue `smp_shared.rs`'s `SCHED_BKLFREE_EL0_ENABLED`
    doc-comment already describes, but that is a guess, not verified here.)
- **Same-binary A/B**, SMP=4, `release-smp-shared --features
  devbox-smoltcp,no-tests,bkl-profile,no-bkl-irq`, `MEMORY=4096`, the
  unmodified `net4 → read4 → cp2 → rm` regimen
  (`scripts/bkl_smp_regimen/`), toggled **in source**
  (`IRQ_BKL_DROP_ENABLED`'s default, per `locking.md`'s playbook rule 5) so
  the feature set stayed byte-identical across both sides:

  | | OFF (BKL-held, matches pre-7a) | ON (`no-bkl-irq`) |
  |---|---|---|
  | `irq/sched` share | 24.7% | **10.2%** |
  | regimen wall-clock | 90s | 90s |
  | digests (4 net + 2 cp) | 6/6 exact | 6/6 exact |
  | `[BKL] stuck` / RECOVERED / PANIC / WILD / SPURIOUS / stale heals | all 0 | all 0 |
  | `[WATCHDOG] Preemption disabled` | 6 | 6 |

  OFF-side's 24.7% closely matches the audit's fresh HEAD baseline (23.5%,
  `BKL_PHASE7_AUDIT.md` §1.2) — confirming the OFF binary behaves like
  pre-Phase-7a. ON-side's 10.2% is a ~59% relative reduction in `irq/sched`'s
  share, the exact effect §5's "Deliverable: `dispatch_irq` for IRQ 27 runs
  BKL-free; `irq/sched` share drops in a same-binary A/B" called for. Per the
  campaign's own rule (never compare absolute spin counts across sessions,
  only shares within one run), the identical 6 watchdog warnings on both
  sides are further evidence the regimen ran the same workload both times.

## 6. Default-on

Folded into `smp-shared`'s default feature bundle the same session, alongside
`no-bkl-network`/`no-bkl-vfs`/`no-bkl-process`/`no-bkl-mm`/`no-bkl-drivers` —
unlike `no-bkl-mm`'s initial staging (`BKL_MM_CARVE_OUT.md` §5, opt-in for a
session before folding in), this phase *was* attribution-driven (picked
because `irq/sched` was the single largest tag in the audit's fresh baseline),
*did* move the needle in the A/B above, and both the boot self-test suite and
the A/B came back completely clean — no reason found to hold off. Removing
`no-bkl-irq` from the `smp-shared` list in `Cargo.toml` still A/Bs it against
the BKL-held path if that's ever needed again.

## 7. What's next (7b–7f)

`../runbooks/bkl-phase7-workplan.md`'s Prompt C is the ready-to-run prompt for
7b, scoping it to all three affected syscalls (`sys_ppoll`, `sys_pselect6`,
`sys_epoll_pwait` — not just `ppoll`) and separating the low-risk
`netpoll_drain`-style fix from the higher-risk full-syscall carve.

Per `BKL_PHASE7_AUDIT.md` §5: 7b (`ppoll`/`epoll_*` carve, including the
BKL-held `smoltcp_net::poll()` call inside `ppoll` — the same shape the §20
`netpoll_drain` carve already handled elsewhere), then 7c (re-audit the
already-carved `openat`/`read`/`accept` residual — 11.9% for converted
syscalls' prologue/epilogue is high enough to warrant a second look), 7d
(`THREAD_CONTEXTS` ownership proof), 7e (process-table locking — the real
blocker), 7f (invert the BKL's default rather than removing it, per
`BKL_FINE_GRAINED_LOCKING_PLAN.md` §7.3). `execve`/`clone` go last, after 7e,
per the audit's explicit ordering rationale (§5's final paragraph).

Re-measuring `irq/sched` after this phase would sharpen 7b's business case:
this session's A/B measured the *toggle's* effect (24.7%→10.2%), not a fresh
full-tag re-rank on top of Phase 7a landing — the audit's own corollary
applies here too (never quote a share across a profiler or campaign-state
change without re-measuring).

---

## Background

- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) — the audit this phase executes
  against; §2.3 named the alarm queue/`critical_section` bug, §5 is the 7a–7f
  decomposition.
- [`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) §7 —
  the replanned Phase 7; §7.3 is the inversion approach for 7f.
- [`../runbooks/bkl-phase7-workplan.md`](../runbooks/bkl-phase7-workplan.md) —
  the work plan and agent prompts this session executed (Prompt B, starting
  at 7a).
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md)
  — the playbook and syscall→lock map; updated alongside this doc to list
  `no-bkl-irq` as a sixth carve-out and correct the `irq/sched` figure again.
- [`BKL_DRIVERS_CARVE_OUT.md`](BKL_DRIVERS_CARVE_OUT.md) §2 — where the
  IRQ-handler goal was deferred from Phase 6 to Phase 7.
- [`BKL_MM_CARVE_OUT.md`](BKL_MM_CARVE_OUT.md) §5 — the staged-rollout
  precedent `no-bkl-mm` set (opt-in first, fold in once validated); this
  phase's evidence was strong enough to fold in the same session instead.
