# The boot WFI probe kills every secondary core's timer (2026-08-18)

**Status: fixed in `src/timer.rs`; verified on `--release` SMP=1/4 (3 consecutive
clean SMP=4 gates) and on devbox-smoltcp via the Idle-CPU gate.**

Regressed commit: **`38345eb7` "extract akuma-time"** — the commit that
introduced the boot-time host-WFI probe
([`AKUMA_TIME_EXTRACTION.md`](AKUMA_TIME_EXTRACTION.md)).

Symptom: at `SMP=4` on `--release`, all three shared-kernel scheduler
self-tests fail and the boot suite then hangs and never reaches sshd.

```
[SMP-shared] ✓ 3 secondary core(s) online (shared kernel)
...
[Test] smp_shared_scheduler FAILED (only 1 core ran workers; core0=2192 core1=0)
[Test] smp_shared_userspace FAILED (userspace on only 1 core; core0=1080 core1=0)
[Test] smp_shared_migration FAILED (probe thread stayed on 1 core)
```

The secondaries come **online** and then never run anything, ever.

## The mechanism

`probe_irq_nop` permanently disarms the virtual timer on every secondary core.

1. **The probe runs long after the secondaries are live.**
   `smp_shared::bringup_secondaries()` is `main.rs:849`. `timer::probe_host_tick()`
   is `main.rs:955`. By the time the probe starts, all three secondaries have
   been online, armed (`smp_shared.rs:952`) and IRQ-unmasked for 100+ lines.

   `probe_host_tick`'s own doc comment asserted the opposite — *"Only the BSP
   calls this, before secondaries exist"*. That precondition was never true on
   an SMP build; nothing enforced it and nothing checked it.

2. **IRQ 27 is a per-CPU PPI, but its handler slot is global.**
   `irq::register_handler` writes one shared `IRQ_HANDLERS` table
   (`src/irq.rs:42`). Installing `probe_irq_nop` for the BSP's probe installs it
   for *every* core simultaneously. While the probe runs, each secondary's
   ordinary periodic tick dispatches into the probe's NOP handler.

3. **The NOP handler disarms whichever core took the IRQ.**
   `probe_irq_nop` → `akuma_timer::disarm()` → `msr cntv_ctl_el0, 0`. That is
   correct and necessary for the *probing* core: a fired one-shot keeps its
   level asserted (`CVAL <= counter`, enabled) and would re-forward forever
   after EOI. On a secondary it is fatal.

4. **Nothing ever re-arms a secondary.**
   A secondary arms its periodic tick exactly once, at bringup
   (`smp_shared.rs:952`). The only thing that would bring it back is
   `timer_irq_handler`'s defensive `cntv_ctl_el0 = 1` (`src/timer.rs:83`) — which
   requires a tick the core can now never take. The BSP re-arms itself right
   after the probe (`main.rs:957`, `enable_timer_interrupts(tick_us)`);
   the secondaries are not in that path.

Result: three cores sitting in WFI forever. Online, never preempted, never
entering the scheduler, never picking up a READY thread. All work stays on
core 0.

### Why it is deterministic, not a race

The probe sweeps `PROBE_CANDIDATES_US = [1_000, 2_000, 3_000, 5_000]` with
`SAMPLES = 8` (`crates/akuma-timer/src/lib.rs:161-171`), settling on 3000 µs on
an HVF host — roughly `8×1 + 8×2 + 8×3 ≈ 48 ms` of probing. The secondaries
tick every 3 ms, so each takes ~16 IRQs into the NOP handler during that
window. Every secondary, every boot. There is no timing margin to get lucky
with, which is why the failure numbers are stable across runs (`core0=2192`
solo vs `core0=2184` under load, `core1=0` in both).

### The evidence that names it

Occurrences of `core=[0-9]` across a full SMP=4 boot log:

| tree | core0 | core1 | core2 | core3 |
|---|---|---|---|---|
| `fdcc51be` (pre-probe) | 353 | 147 | 218 | 194 |
| `65b63bd3` (post-probe) | **88** | **0** | **0** | **0** |

The hang follows from the same cause: `smp_shared_cooperative_wait` passes,
the next test spawns four system threads (tid 4-7) expecting peer cores to run
them, and blocks forever.

## The fix

`src/timer.rs`: publish the probing core in a `PROBING_CORE` atomic before the
probe unmasks IRQs; `probe_irq_nop` disarms only when it is running on that
core. A secondary landing in the shared handler slot re-arms its periodic tick
and returns.

```rust
pub fn probe_irq_nop(_irq: u32) {
    if akuma_exec::bkl::current_core_id()
        != PROBING_CORE.load(core::sync::atomic::Ordering::Relaxed)
    {
        akuma_timer::arm_periodic_tick();
        return;
    }
    akuma_timer::disarm();
}
```

A secondary loses only the scheduler SGI of whatever ticks fall inside the
~48 ms probe window. It does not need to learn the probed tick from the probe:
`timer_irq_handler` re-arms from `current_tick_us()` (the shared
`akuma_timer::TICK_US`) on every tick, so it converges on the probed value at
its next tick regardless of what it armed with at bringup.

**Alternative considered and rejected:** move `probe_host_tick()` before
`bringup_secondaries()`, honouring the original contract. It works and would
additionally let secondaries arm with the probed tick from the start, but it
drags the GIC and IRQ-27 registration earlier in boot — a much larger
boot-order change for no behaviour the cheap fix does not already deliver.

## The A/B that got this misattributed

The regression was first reported against `65b63bd3` "unified scheduler",
A/B'd against `fdcc51be` described as *"its direct parent"*. It is not:

```
$ git rev-parse --short 65b63bd3^
44562a9c
$ git log --oneline fdcc51be..65b63bd3
65b63bd3 unified scheduler
44562a9c cleanup
38345eb7 extract akuma-time
```

The A/B straddled **three** commits — the entire akuma-time extraction — and
charged all of it to the last one. Booting the true parent settles it:

| commit | tick | probe | SMP=4 scheduler tests |
|---|---|---|---|
| `fdcc51be` | 1_000 µs | absent | **PASS** (3 cores) |
| `38345eb7` | 3_000 µs | **present** | not booted directly |
| `44562a9c` | 3_000 µs | present | **FAIL** (`core0=1879 core1=0`) |
| `65b63bd3` | 3_000 µs | present | **FAIL** (`core0=2192 core1=0`) |

`44562a9c` "cleanup" touches one file, `src/kernel_timer.rs` (+8/−37), and its
whole payload is replacing a duplicated CNTVCT read with
`akuma_timer::uptime_us()` — identical arithmetic, and nothing that could
reach cross-core scheduling. `65b63bd3` is a mechanical move of
`src/kernel_timer.rs` → `crates/akuma-exec/src/alarms.rs` plus AB-probe
deletion; the doc it adds
([`TRIMMING_FAT_SCHEDULER.md`](TRIMMING_FAT_SCHEDULER.md)) states in its own
header that the unification is *"proposal — no code moved."*
That leaves `38345eb7` by elimination. It was not booted directly — the
inference rests on `fdcc51be` PASS, `44562a9c` FAIL, and `44562a9c`'s diff
being incapable of it.

The boot logs name the difference in one line each:

```
fdcc51be:  Preemptive scheduling enabled (10ms timer -> SGI)     # stale print; actually 1_000 µs
65b63bd3:  [Timer] host WFI probe: tick = 3000 us
```

`fdcc51be`'s "10ms" is a hardcoded string that had drifted from
`TIMER_INTERVAL_US = 1_000`. It is worth knowing that the tick change and the
probe arrived in the same commit, because the 1 ms tick was independently
*masking* this bug: below the HVF WFI floor the guest's `wfi` returns
immediately, so a secondary with a dead timer still spun back into the
scheduler. Fixing the idle-CPU regression is what exposed the dead timers.

## Two contaminated-evidence traps in the same session

Both cost real time; both are cheap to avoid.

**1. `verify_trim.py`'s `pkill` is not instance-aware.** `boot_once()`
(`scripts/verify_trim.py:356`) opens with:

```python
subprocess.run(["pkill", "-f", "qemu-system-aarch64"], capture_output=True)
```

`--instance N` shifts ports and snapshots the disk, but does **not** scope this
kill. Two concurrent gate runs therefore shoot each other's VM mid-boot. In the
original report this surfaced as `qemu-system-aarch64: terminating on signal 15
from pid 92599` at the end of the SMP=4 log, read as the harness's own 480 s
timeout.

Tells that the runs overlapped:

- `mine_logs/verify_smp1.log` (2500 bytes) contained the *baseline worktree's*
  `cargo` compile output, not a boot log.
- `mine_logs/verify_host_tests.log` and `base_logs/verify_host_tests.log` were
  **byte-identical** under `cmp` — impossible for two independent `cargo test`
  runs, whose output carries per-suite timings.

**Use `VERIFY_LOGDIR`.** The script honours it (`scripts/verify_trim.py:656`)
for all three raw per-tier logs. The claim that only `--out` is instance-safe
and the raw paths are hardcoded to `/tmp` is wrong. Even so, never run two
gates concurrently — `VERIFY_LOGDIR` separates the logs, not the `pkill`.

**2. A cross-contaminated summary invented a second bug.** `mine.txt` reported
`preempt::tests::guard_balances_the_counter` failing and the host suite
truncated to 432 tests, theorised as a knock-on of the commit's `runtime.rs` /
`lib.rs` changes. It never failed:

```
$ cargo test -p akuma-primitives --target aarch64-apple-darwin
test result: ok. 51 passed; 0 failed; 0 ignored
```

The summary was self-refuting on its face — `host.failed: 0` printed directly
above `host.failed_names: preempt::tests::disable_enable_round_trips`. A
summary that contradicts itself in adjacent fields is a corrupt parse, not a
finding; check that before building a causal story on it. `preempt.rs` is
byte-identical between the two commits, which had already been noticed and was
argued *past* rather than treated as the answer.

## Verification

```bash
S=/path/to/scratch
VERIFY_LOGDIR=$S python3 scripts/verify_trim.py --tier all --out $S/fixed.txt
scripts/measure_idle_cpu.py --smp 4
```

The `core=[0-9]` histogram in the boot log is the fastest single check: cores
1-3 present means the tick survived the probe.

### Boot gate — `--tier all`, diffed against `fdcc51be`

| | `fdcc51be` | fixed |
|---|---|---|
| `smp1.booted` / `smp4.booted` | True / True | True / True |
| 17 Tier-3 exercises, both SMP levels | ok | ok |
| `smp1.fail_set` / `smp4.fail_set` | (empty) | (empty) |
| `smp4.pass_marker` | 95 | 95 |
| `smp4.passed_marker` | 291 | 292 |
| `smp4.bkl_stuck` | 93 | 97 |
| clippy × 4 configs | clean | clean |
| `host_timejumps` | 0 | 0 |

`bkl_stuck` is inside its documented run-to-run band (93 / 97 / 108 / 93 across
four SMP=4 boots here; the runbook records 93, 96, 109 on an unchanged tree).

### Stability — three consecutive SMP=4 gates

| run | `smp_shared_scheduler` | `smp_shared_userspace` | `migration` |
|---|---|---|---|
| 1 | PASSED, 4 cores, `core0=644 core1=568` | PASSED, 4 cores | PASSED, 4 cores |
| 2 | PASSED, 4 cores, `core0=605 core1=1215` | PASSED, 4 cores | PASSED, 4 cores |
| 3 | PASSED, 4 cores, `core0=108 core1=876` | PASSED, 4 cores | PASSED, 4 cores |

All three booted to sshd with `fail_set: (empty)`, `flaky_seen: (none)`,
`host_timejumps: 0`. Pre-fix these read `FAILED (only 1 core …; core1=0)` every
time. Note the fix beats the `fdcc51be` baseline, which reached only 3 cores
with `core1=0` — that baseline was itself degraded, reaching secondaries only
because its 1 ms tick sat under the HVF WFI floor and made `wfi` a no-op.

### Idle-CPU gate

| | pre-fix (`65b63bd3`) | fixed |
|---|---|---|
| SMP=4 | 9.4% | **3.7%**, repeat **3.8%** |
| SMP=1 | — | **1.1%** |

All with `time_jumps: 0`, `bkl_stuck: 0`. Idle cost went *down*: 9.4% was three
cores wedged with dead timers while the BSP carried the whole system alone.
3.7% is at/below the runbook's 4-8% band and 1.1% matches the 1.6% that
[`AKUMA_TIME_EXTRACTION.md`](AKUMA_TIME_EXTRACTION.md) measured for a 3 ms tick
at SMP=1.

Note that devbox-smoltcp reaches sshd fine at SMP=4 *even with the bug* — the
hang is confined to the `--release` boot-test suite. "SMP=4 never reaches a
stable running state" was not true, and the Idle-CPU gate was always obtainable.

## Not part of this bug: the flaky `preempt` host tests

The original report also charged this commit with breaking
`preempt::tests::guard_balances_the_counter`. It did not, and neither did the
other two commits in range: `git diff fdcc51be 65b63bd3 --
crates/akuma-primitives/` is **empty** — the branch never touches that crate,
and `preempt.rs` was last modified by `f54169f6` / `069f1f07`, both earlier.

The tests are genuinely racy, and have been all along. Their own comment says
why (`preempt.rs:356`): host builds report tid 0 for *every* thread, so all
eight tests operate on global slot 0, each calling `reset()` (`scrub_slot(0)`)
while the others are mid-flight — and `cargo test` runs them in parallel.

Reproduced on the fixed tree, 60 runs of `preempt::tests --test-threads=8`
under four spinning load generators: **2 failures**, three distinct signatures
that between them cover every name reported across both sessions.

```
guard_balances_the_counter                        left: 1    right: 0
disabled_at_names_the_zero_to_one_call_site_only  left: 414  right: 409
scrub_slot_clears_a_leaked_count                  left: 1    right: 2
```

It needs load to trip: `--test-threads=1` passed 5/5, the crate alone passed
10/10, the full workspace passed 3/3, and it appeared only in a `--tier all`
run where clippy had just saturated the machine. When it does fire, the panic
aborts that crate's run and truncates the suite total (`host.tests: 482`
instead of 592) — which is what made it look like a commit had disabled tests.

Fix (not applied here — it belongs in its own commit, in a crate this branch
does not touch): serialize the eight behind a mutex, or give each its own slot
instead of sharing 0.

## Background

- [`AKUMA_TIME_EXTRACTION.md`](AKUMA_TIME_EXTRACTION.md) — the extraction that
  introduced the probe, the HVF WFI floor, and the 1 ms → 3 ms tick.
- [`CPU_LOAD_REGRESSION_INVESTIGATION.md`](CPU_LOAD_REGRESSION_INVESTIGATION.md)
  — why the tick rate was lowered in the first place, and the WFI floor that
  made the 1 ms tick mask this bug.
- [`TRIMMING_FAT_SCHEDULER.md`](TRIMMING_FAT_SCHEDULER.md) — the unification
  proposal `65b63bd3` was wrongly blamed for implementing.
- `docs/runbooks/verify-trim-fat-change.md` — the A/B procedure, including the
  Idle-CPU gate.
- `docs/reference/subsystems/smp-shared.md`, `docs/reference/subsystems/irq.md`
  — the shared-handler-table and per-core-tick invariants this bug sat between.
