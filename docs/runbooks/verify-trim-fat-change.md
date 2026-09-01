# Verify a trim-the-fat change (no-regression gate)

**Grade: A** — every command here was run end-to-end on 2026-08-13 for Phase 6
item 1 (the `channel.rs` FIFO merge). **Probe inventory refreshed 2026-08-28**
(the `akuma-syscalls-linux` extraction): both stale baselines re-measured, and
the four probes built between 2026-08-19 and 2026-08-27 added to Tier 3 with
their real output — see "Probes built since this runbook was last revised".

> **The prose and the script are deliberately out of step right now.** The new
> Tier 3 probes are documented here but are **not** in `verify_trim.py`'s
> `EXERCISES`. That is not an oversight to fix blindly: the automated set is
> fork/CoW/fault-path only, and adding a *network* probe to it means the gate
> starts reporting network flakes as refactor regressions. Add them when you
> have A/B'd their stability across arms, not before — the `cowstale` entry in
> the known-benign table is what that mistake costs.

For deduplication / extraction work from
[`../archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](../archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md).
Those changes are supposed to be behaviour-preserving, so the gate is a
**comparison against a baseline**, not a green checkmark.

Work in tiers, each one gated on the last being clean: **Tier 1** is host-only and
takes ~2 minutes; **Tier 2** boots the VM for the self-test suite; **Tier 3** is
the live I/O and fork/CoW binaries; **Tier 4** is the redis memtest on the devbox,
for changes in the memory path; **Tier 5** is self-host clean-build trials, for
changes in the mmu / fault / file-page-cache path. Tiers 3–5 are conditional —
read their headers before spending the time.

**Tiers 1–3 are automated: run [`../../scripts/verify_trim.py`](../../scripts/verify_trim.py)
rather than hand-assembling the commands below.** It runs the four clippy
configurations, the host-test count, both SMP levels and the Tier 3 binaries, and
prints one `=== SUMMARY ===` block designed to be diffed against a run on your
parent commit. Read its module docstring first: every measurement in it is there
because doing that measurement by hand gave a wrong answer at least once.

```bash
VERIFY_LOGDIR=/tmp/v-mine scripts/verify_trim.py --out mine.txt
git worktree add /tmp/base <parent-commit>
cp scripts/verify_trim.py /tmp/base/scripts/          # see "the baseline's own gate" below
(cd /tmp/base && VERIFY_LOGDIR=/tmp/v-base scripts/verify_trim.py --instance 1 --out /tmp/base.txt)
diff /tmp/base.txt mine.txt
```

**Set `VERIFY_LOGDIR` per arm.** It defaults to `/tmp` for both, so the second
run overwrites `verify_smp1.log` / `verify_smp4.log` from the first — and those
are exactly the files the `passed_marker ±1` row below tells you to `diff` when
a row moves. Losing them turns a two-minute confirmation into another pair of
four-minute runs. (Added 2026-08-28, after doing precisely that.)

**Run the two arms sequentially, not in parallel.** Each boot kills stale QEMU
first; until 2026-08-28 that kill was a bare `pkill -f qemu-system-aarch64`,
which takes down the *other* arm's VM mid-run — and every other VM on the
machine, which CLAUDE.md § "Waiting for a VM" bans for that reason. It is now
matched on the arm's own `hostfwd` port, so parallel runs no longer kill each
other; sequential is still the safer default, because both arms contend for host
CPU and Tier 3 is timing-sensitive.

**The baseline's own gate may be broken.** `verify_trim.py` is a *tool*, not the
thing under test, so copy your fixed copy into the baseline worktree rather than
running whatever shipped at that commit. Concretely: at `dacbe557` and earlier,
`boot_once` called `wait_for_marker(log_path, port=port, proc=qemu)` with
**neither name bound** — an `UnboundLocalError` that aborts the run before the
first boot. Fixed 2026-08-28; any baseline older than that needs the copy.

`--instance 1` shifts the forwarded ports and opens the **main** worktree's
`disk.img` in snapshot mode (writes discarded), so the baseline worktree can boot
without touching it. A linked worktree has no `disk.img` of its own — it is 3 GB
and gitignored — and until 2026-08-14 the script pointed `DISK` at the worktree's
own path, so every baseline run reported `smp1.booted: False` / `smp4.booted:
False` / `pass_marker: 0` while Tier 1 passed. **That is a missing file, not a
broken baseline commit**; the script now fails with `ERROR no disk image at …`
instead. If you see the old symptom on an older checkout, this is why. The exit
status is **not** a
verdict — only the diff is. The prose below is what the script automates, kept
because it says *why* each step is shaped the way it is, and because Tier 4 is not
automated.

---

## Tier 1 — host only (~2 min, no VM)

Run all of it. Feature-gated code is invisible to a single `cargo clippy`, and
three of the four configurations below compile files the default one does not.

```bash
cd "$(git rev-parse --show-toplevel)"
HOST=$(rustc -vV | grep '^host:' | cut -d' ' -f2)

cargo clippy --release
cargo clippy --profile extreme-size --no-default-features \
      --features no-tests,smoltcp,extreme,userspace-sshd
cargo clippy --release --features devbox-smoltcp,no-tests
cargo clippy --release --no-default-features --features \
      "$(grep -oE 'DEVBOX_FEATURES="[^"]*"' scripts/build_devbox.sh | head -1 | sed 's/.*="//;s/"//')"

cargo test --target "$HOST"
```

Userspace crates are a separate workspace and are **not** covered above. Run
them when the change touches `userspace/`:

```bash
(cd userspace && cargo test -p akuma-ssh-crypto --target "$HOST")
(cd userspace && cargo test -p sshd --lib --no-default-features --target "$HOST")
(cd userspace && cargo test -p box  --lib --no-default-features --target "$HOST")
```

**Verify:** all four clippy runs end in `Finished`, with no `warning:` or
`error:` lines. Test count is **≥** your baseline — a merge should add tests, and
must never remove them.

Count the total reliably; do not eyeball the per-binary lines:

```bash
cargo test --target "$HOST" 2>&1 \
  | grep -E '^test result:' \
  | sed -E 's/^test result: [a-z.]+ ([0-9]+) passed.*/\1/' \
  | paste -sd+ - | bc
```

> **Baseline as of 2026-08-28: 858, 0 failed** (`869928e6`, the
> `akuma-syscalls-linux` extraction). The number has moved by a factor of 1.6 in
> two weeks and every step was a crate arriving, not tests being written into
> `src/`: `akuma-firecracker` (2026-08-21, DTB fixtures), `akuma-net-yarn`
> (2026-08-24), `akuma-time` (renamed `akuma-syscalls-time` 2026-08-28) and
> `akuma-boot` (2026-08-25),
> `akuma-syscalls-linux` (+34, 2026-08-28). **That is the mechanism to expect:**
> an extraction's whole point is to make a body of logic host-testable, so the
> count jumps by the new crate's own test count on the commit that lands it, and
> a +34 that matches the new crate exactly is the healthy shape — not a
> suspicious one. What must never happen is the count going *down*.
>
> **Superseded: 521 as of 2026-08-14** — after the Phase 5 user-copy sweep (+5
> `user_range_ok` tests in `akuma-exec`). Was **516** after the §5.7 errno-table
> merge (+4 in `akuma-primitives`), and **512** earlier the same day: the arm-2 count of that
> day's DA/IA
> demand-paging body merge (`COW_PILE_AUDIT.md` §12.3), whose baseline arm measured
> **508** on the same tree an hour earlier. Two moves on one day: 506 → 508 → 512, so
> re-measure. 506 was measured on both arms of that day's `MADV_DONTNEED` A/B —
> identical on both arms, so it was the count on a clean tree
> and not something that change moved. Was **486** on 2026-08-13, before the
> `akuma-pmm` extraction landed. Previously 455 when this runbook was written, 463
> before the Phase 6 item 5 guard merge, 467 after it (+4 `FaultSlot::reclaim_report`),
> then 486 after the memory-math move (+8 `fork_copy_math_tests`, +11
> `memmath::tests`). Re-measure rather than trusting this line; it has gone stale
> three times already. Original composition below.
>
> Note the two boot-log marker formats: `[PASS]` (what the Tier 2 gate counts) and
> `[Test] <name> PASSED`. Deleting a boot test may move only the second — the
> memory-math move took `PASSED` from 273 to 268 while `[PASS]` stayed at 94.
> Check both when you remove or add boot tests.
>
> **Superseded: 455** (akuma-exec 210, ext2 52, isolation 43,
> net 25, primitives 28, rump 37, terminal 21, vfs 39). Do not sum these by
> hand with `awk -F'[ ;]' '{s+=$4}'` — the consecutive separators shift fields
> and it silently under-counts. That mis-measurement happened twice while
> writing this runbook.

### Can this be a host test rather than a boot test?

Ask before reaching for `src/process_tests.rs`. The bar is lower than it looks:
`akuma-exec` already runs 210 host tests, `with_irqs_disabled` is a no-op off
`target_os = "none"`, `current_thread_id` is `akuma_primitives::preempt::current_tid`,
and the wake path is atomic-array bookkeeping — **registering and signalling a
waiter are host-testable; only a thread actually stopping and resuming is not.**

If the blocker is a missing global, **inject it** rather than adding a
production branch that tolerates its absence:

```rust
// in the tests module
fn setup() { crate::runtime::register_config_for_test(); }
```

`OnceCopy::set` is idempotent, so every test can call it unconditionally despite
parallel execution. Background: `TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §6.1.

---

## Tier 2 — boot suite (~3 min)

**Kill any VM you already have running first.** A stale QEMU holds the
forwarded ports and the new one dies with `Could not set up host forwarding
rule` — which reads exactly like a boot failure:

```bash
pkill -f qemu-system-aarch64; sleep 2
```

```bash
MEMORY=2048 cargo run --release > /tmp/mine.log 2>&1 &
until grep -aqE "Started sshd|sshd started" /tmp/mine.log; do sleep 3; done
```

`-a` is mandatory on every grep of a boot log: QEMU emits a control byte that
makes plain `grep` treat the file as binary and print nothing.

### Verify

```bash
grep -ac '\[PASS\]' /tmp/mine.log                       # expect 99
grep -aoE '\[FAIL\] [a-z_0-9]+' /tmp/mine.log | sort -u  # expect an empty set
```

**Baseline 2026-08-28 (`869928e6`): 99 `[PASS]` at both SMP=1 and SMP=4, and the
failure set is EMPTY at both** — `host_timejumps: 0` on both boots, so the host
was quiet enough for the reading to mean something. `passed_marker` was 305 at
SMP=1 and 313 at SMP=4; the two levels are not expected to agree, because
several tests SKIP or report INCONCLUSIVE depending on core count (see the
`passed_marker` row in the known-benign table).

**Superseded baseline 2026-08-13: 94 `[PASS]`, failure set exactly
`retired_reclaim_ab`** — that one used to fail on an unmodified tree (threshold
too tight, `TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §8.5 Phase 0). It did not
appear in either arm on 2026-08-28. **Do not read that as fixed**: the note
below records it flipping run to run, and two clean boots cannot distinguish a
repaired threshold from two lucky samples.

> **`retired_reclaim_ab` flips run to run — so `94` is really `94 or 95`.**
> Measured at SMP=4 on 2026-08-13, five boots: the working tree scored
> 94/95/94 (failure set `{retired_reclaim_ab}`, then **empty**, then
> `{retired_reclaim_ab}`) and an unmodified `git worktree` at the same commit
> scored 95 with an **empty** failure set. Both trees produce both outcomes, so
> a 95-with-no-failures run is not evidence of a fix and a 94 run is not evidence
> of a regression. Treat `{}` and `{retired_reclaim_ab}` as the same result, and
> only investigate a failure set containing anything *else*. `main` (b585aed) at
> SMP=4 scored 93 + `{retired_reclaim_ab}`, so the PASS total also moves with the
> branch — compare sets, and compare against a worktree at *your* parent commit,
> never against this number.

Compare failure **sets**, never counts:

```bash
diff <(grep -aoE '\[FAIL\] [a-z_0-9]+' base.log | sort -u) \
     <(grep -aoE '\[FAIL\] [a-z_0-9]+' mine.log | sort -u)
```

### Idle-CPU gate (scheduler/tick/wake/poll changes: mandatory)

Every functional gate above can pass while an idle VM burns whole host cores
— nothing it measures is *wrong*, there is just 100x more of it. The
scheduler-tick regression of 2026-08-18 landed exactly this way (1 ms tick →
100% host CPU per guest core under HVF; `archive/CPU_LOAD_REGRESSION_INVESTIGATION.md`).

```bash
scripts/measure_idle_cpu.py --smp 4    # boots its own VM, samples post-boot
```

- Expect single digits (`idle_cpu_pct` ≈ 4–8 on devbox-smoltcp SMP=4 with the
  self-tuning tick; the log should show `[Timer] host WFI probe: tick = 3000 us`
  on an HVF host).
- `ps -o %cpu` is meaningless here (macOS averages over process lifetime) —
  that is why the script differences `ps -o time=` over a post-boot window.
- Exit status is not a verdict: A/B against a worktree at your parent commit,
  same SMP. A reading >2x the parent's with `time_jumps: 0` is a real
  regression even if the suite is green.

Run this for any change touching: the scheduler tick, wake/preempt paths,
idle loops (`idle_halt`, netpoll WFI), poll intervals, or the timer crate.

### Known-benign — do not chase these

| In the log | Why it is fine |
|---|---|
| `[STACK-OVERFLOW] tid=1 … 512KB kernel stack` immediately before `[Test] stack_canary_overrun_is_reported PASSED` | That test **deliberately** smashes a canary. `spurious=0 exercised=true detected=1` is the healthy line. Distinct from the `SMP-1`-at-boot spurious class (`DEVBOX_ISSUES.md` Issue 11) |
| `[Exception] Sync from EL1: EC=0x25` ×2 | Pre-existing; present in every archived baseline in `logs/` |
| A `grep -c SIGSEGV` hit | The boot banner **prints an OOM-signature help block containing the literal text** `[Fault] Process N (name) SIGSEGV after Xs`. Match `^\[.*\] \[Fault\]`, not bare `SIGSEGV` |
| `[BKL] stuck tag=511` | Load-driven. A real storm is thousands of lines, not tens |
| The suite stops mid-run: the boot log **stops growing**, QEMU pegs one core, and the remaining exercises report TIMEOUT | **Known intermittent wedge — an EL1 sync-fault loop in `sgi_scheduler_handler_with_sp`, still open** (`COW_PILE_AUDIT.md` §10, and [`scripts/f8_wedge_repro.py`](../../scripts/f8_wedge_repro.py) to catch it under lldb). ~1 in 7 SMP=1 suite runs. It hits at the transition *between* exercises, where sshd forks the next one, and a wedged run can have **zero** time-jump lines on a completely idle host — so it is a spin with IRQs masked, not host load. Reproduced 2026-08-14 across two different trees, which is how it was cleared of being either F1 or F2. **Consequence for verification: a single SMP=1 exercise run proves nothing.** Re-run, and treat 1-of-1 results as noise — this flake was mis-attributed to a code change twice in one session before it was A/B'd properly |
| `[WATCHDOG] Time jump detected: ~100ms (host sleep/wake)`, repeated | Lost guest time. The parenthetical is the kernel's *guess*, not a measurement, and both causes are real: **(a)** the host descheduling QEMU — a background `cargo`/rust-analyzer rebuild during the boot is enough, and the gate's own Tier 1 immediately before Tier 2 does it; **(b)** the guest itself losing time. Treat a high count as "this run's timing is untrustworthy", not as a diagnosis, and note the wedge above can occur with a count of **zero**. Measured 2026-08-13: a run with 2866 of these had `cowstale` TIMEOUT at SMP=1 and UNEXPECTED at SMP=4 on a tree where both pass, and the same tree re-run quiet scored `ok` at both — but a clean tree also scored 741 and passed everything, so the count bounds trust, it does not explain a failure. Count them (`grep -ac 'Time jump detected'`) before believing any exercise result, and re-run rather than debug |
| `passed_marker` differs by exactly 1 at SMP=4 while `[PASS]` is unchanged | Usually `thread_slot_reclaim_on_spawn_initializing`, which has a **third self-reported outcome** besides PASSED/SKIPPED: `INCONCLUSIVE: N slots already free at spawn, nothing forced a reclaim`. It needs the slot pool actually exhausted, and at SMP=4 that is a race (measured 2026-08-13: PASSED in one run, INCONCLUSIVE in another on the same tree, 276 vs 275). Confirm by name before treating a ±1 `passed_marker` move as a deleted test: `diff` the `[Test] <name> (PASSED\|SKIPPED)` sets between the two logs |
| `[SGI-S FATAL] new_sp=0x0 invalid!` mid-suite, boot never reaches sshd | **Fixed 2026-08-14** for its dominant source: `test_unregister_skips_recycled_thread_slot` seeded bare claimed slots (zeroed context, `sp=0`) as `READY` across several UART prints — dispatchable by a timer tick (any SMP) or a peer core (SMP>1); it fired 3 times in 9 boots on a loaded host that day. The test now seeds `WAITING` with no deadline, which nothing dispatches or wakes. If this line still appears, the remaining fabricated-slot suspect is `test_kill_thread_group_reaps_futex_blocked_sibling` (WAITING seed + futex wake); the gate reports `smpN.halt` and `smpN.pass_marker` even when `booted: False`, so you can see how far the suite got |
| `cowstale` reports `UNEXPECTED` **at SMP=2** with `[Fault] … FAR=0x420908 ELR=0x403a90 ISS=0x4f` and `[WPF] … va=0x420000 cow_ref=0 … ap_rw=true` | **Fix landed 2026-08-30 — see the SMP=1 row below for the current state and the one remaining survivor class.** Pre-fix history: ~40% per run, A/B'd 2/5 on both arms, byte-identical signature; the absorb's premise held (the PTE granted the write) but the absorb ran only at arm entry, before the loser waited on the fault slot. Original notes follow. ~~**Note SMP=2 is not in the gate's default `--smp 1,4`**, so nothing else samples it~~ — **wrong, corrected 2026-08-19: it fires at SMP=1, which the default gate does sample.** Sample **five runs per arm** at whichever level you are judging, before believing either result |
| `cowstale` reports `UNEXPECTED` **at SMP=1** with `FAR=0x420260 ELR=0x400868 ISS=0x4f` and the same `[WPF] … va=0x420000 cow_ref=0 … ap_rw=true` tell | **Fixed 2026-08-30** (`COWSTALE_FORK_THREAD_SEGV.md` header): the absorb now runs again after the fault-slot wait and once more at the moment of killing, both keyed on the live PTE. Validated 2026-08-30 at SMP=4 with the new `cowstale hammer` storm: 1/15 per run (was 4/10 pre-fix, same in-boot method), classic 0/8, `bssfork 20 8 1` clean — **but the one hammer survivor still printed `ap_rw=true cow_ref=0`, so a rare route remains open**; SMP=1 and SMP=2 are untested post-fix. Until that survivor is explained, treat a `cowstale: UNEXPECTED` here as a real finding worth one re-run and a look at the `[WPF]` line, not as automatic noise. The pre-fix notes below remain useful for recognising the shape. **(1)** It was not obviously stochastic — byte-identical virtual coordinates across two kernels; only `pa=`/`free=` moved. **(2)** The multi-core framing never explained it — `cowstale` runs **3 reader threads** (`pid=139/140/141`, all `tgid=138`), and the repair only needs a second *thread* plus a preemption point inside the fault handler, which SMP=1 has. The `Time jump` explanation for an earlier SMP=1 failure (2866 jumps) is unavailable to runs with 0 jumps |
| `smp4.fpcache` present in one summary and absent in another | Harness timing, not behaviour. `[FPCACHE]` is emitted periodically and the gate snapshots the boot log at the sshd marker, so the line lands before the snapshot in some runs and after it in others. Compare `entries=`/`misses=` (stable) and ignore `hits=` (a monotonic counter read at an arbitrary instant) |

---

### Two ways of sampling a flaky probe, and why they disagree

Recorded 2026-08-28, after two independent A/B runs of `cowstale`/`bssfork`
produced **different pass rates and the same verdict**:

| method | `cowstale` | `bssfork` |
|---|---|---|
| one boot, probe re-invoked 5× over ssh | 0/5 both arms | 2/5 both arms |
| fresh boot per run, via `verify_trim.py --tier 2` | 2/5 both arms | 5/5 both arms |

**Re-invoking inside one boot is the harsher test**, and it is not the thing the
gate measures. Each `cowstale` run builds and breaks CoW mappings; the residue —
fragmentation, page-cache state, whatever the previous run left retired — is
still there when the next one starts, so later runs in a boot fail more often
than the first. The gate boots fresh for each sample and therefore reports a
kinder number.

Neither is wrong. They answer different questions: in-boot repetition asks "does
this survive repeated use", the gate asks "does one run pass on a clean system".
The mistake is **comparing a rate from one method against a rate from the other**
— which is what makes a change look like a regression when the only thing that
changed was the harness.

Two rules follow:

1. **Sample both arms with the same method**, and say which method next to the
   number. A bare "2/5" is not a result.
2. **Trust the arm-vs-arm comparison, not the absolute rate.** Both methods above
   agreed the two arms were identical, which is the only claim either supports.

Related: host load moves these numbers too, and the `Time jump detected` row in
the failure table above is the tell. Do not run two arms concurrently on one
host — Tier 3 is timing-sensitive, and a second QEMU pinning several cores is
easily enough to change a verdict.

## Tier 3 — live paths (only if the change touches I/O)

Skip unless the change is in the stdio / VFS / net path. `ssh` is blocked by
policy; drive it from Python.

Byte-faithfulness matters more than "it responded" — a FIFO bug corrupts the
*middle* of a stream and still exits 0:

```python
import subprocess, hashlib
def sh(cmd, inp=None, t=300):
    r = subprocess.run(["ssh","-q","-o","StrictHostKeyChecking=no","-p","2222",
                        "root@localhost",cmd], input=inp, capture_output=True, timeout=t)
    return r.returncode, r.stdout

# stdout across the exec channel: 8 MiB is 8x MAX_BUFFER_SIZE, so backpressure engages
rc, out = sh("busybox dd if=/dev/zero bs=65536 count=128 2>/dev/null")
assert hashlib.md5(out).hexdigest() == hashlib.md5(b"\0"*8388608).hexdigest()

# stdin across the channel: exercises write_stdin's short-write contract
payload = bytes((i*7+3) & 0xFF for i in range(4*1024*1024))
rc, out = sh("busybox md5sum", inp=payload)
assert out.split()[0].decode() == hashlib.md5(payload).hexdigest()
```

Memory / fork / CoW binaries already on `disk.img` — all self-reporting:

| Command | Healthy output |
|---|---|
| `elftest` | `elftest: ALL tests PASSED` (**exit code 42 is success**, by design) |
| `forkprobe` | `forkprobe: ALL PASS` |
| `stackstress` | `stackstress: PASSED after …` |
| `bssfork` | `failures=0` … `bssfork PASS`. **Its pass rate at SMP=4 depends on how you sample it — see "Two ways of sampling" below.** Two measurements the same day disagreed: 2/5 on both arms when the probe was re-invoked repeatedly inside one long-lived boot, and 5/5 on both arms when each run got a fresh boot through the gate. Both agreed the arms were identical, which is the part to trust. Treat a `bssfork` difference between arms as unresolved until sampled five times per arm by the *same* method on both sides. The `bssfork 20 8 1` control was 5/5 everywhere, which is what separates "this probe is flaky" from "fork is broken" |
| `bssfork 20 8 1` | `failures=0` … `bssfork PASS`. **Not `bssfork spread=1`** — the binary's CLI is positional (`bssfork [rounds] [threads] [spread]`), not `key=value`; running the literal string `spread=1` feeds it into `rounds`, `strtoul` parses that as `0`, and `spread` silently defaults to `0` too. `rounds=0` skips the fork loop entirely, so `g_stop` fires almost instantly and the liveness check flags threads `[never ran]` before they get scheduled at all — nothing to do with CoW or the kernel. **Corrected 2026-08-14**: the "BROKEN PRE-EXISTING" verdict recorded here on 2026-08-13 (`failures=7`/`8`, `ticks=0`, "unexplained regression") was this same mis-invocation on both `main` and the branch; the real control, invoked correctly, passed 8/8 clean runs at SMP=4 on first re-check. See `docs/archive/PMM_EXTRACT.md` §8 for the full correction |
| `cowstale` | `reader_faults=0 failures=0` … `cowstale PASS`. **The long-known stale-write-fault class got a second fix stage on 2026-08-30** (`COWSTALE_FORK_THREAD_SEGV.md` header): the absorb now re-checks after the fault-slot wait and at the moment of killing. Pre-fix it failed at SMP=1/2/4 at rates that depended on sampling method — 0/5 in one boot vs 2/5 fresh-boot per run on the same trees; post-fix at SMP=4 in-boot: hammer 1/15, classic 0/8, `bssfork 20 8 1` clean. **One hammer survivor still printed `ap_rw=true cow_ref=0`**, so a `cowstale: UNEXPECTED` is worth a re-run plus a `[WPF]` look rather than automatic dismissal. **`cowstale hammer [rounds] [pages] [threads]`** (defaults 200/4/8) is the amplifier: workers whose only work is incrementing adjacent `.bss` counters on one page, so a whole fork-storm generation loses the race as a group — pre-fix it killed **four workers in the same tick** (four `[Fault]`/`[WPF]` pairs, FARs across `g_hammer[0..3]`) where classic killed one thread per kill. Use it to make the class observable fast; at SMP=1 keep threads ≤ 3 (CPU-bound workers starve the box) |
| `madvshared` | `madvshared: ALL PASS`. `MADV_DONTNEED` on a CoW-shared frame must not touch the peer's page — the null-`Rc` mechanism (`../archive/CARGO_HEAP_NULL_RC.md`), fixed 2026-08-14. Deterministic, milliseconds, no allocator involved, and **calibrated**: the identical static binary PASSes all three phases on real Linux arm64 (`docker run --rm --platform linux/arm64 -v "$PWD:/w:ro" alpine /w/madvshared`), so a FAIL is the kernel, not the probe. Before the fix it reported `2 FAIL` at both SMP=1 and SMP=4 |
| `mmapsum <path>` | six digests; **needs a path argument**. `read:`/`mmap1:`/`mmap2:`/`madv:` must all be **the same value** — `madv:` is the regression check for the 2026-07-25 `MADV_WILLNEED`-installs-zeroed-frames bug. `mtA:`/`mtB:` hash **one half each** and are *supposed* to differ from that value and from each other; only their stability across runs means anything |
| `forktest_parent -duration=20s` | `All children processed via epoll. Parent exiting.` |
| `mprotectlb` | `=== MPROTECTLB DONE — 0 divergence(s) from Linux ===`. Self-calibrating: it counts its own divergences, so the healthy marker is the **count**, not a PASS |
| `pthread_kill_eintr` | `RESULT: PASS`. Its `PHASE2 INFO` line about Akuma deferring handler delivery to syscall return is a documented divergence, **not** a failure |
| `fpfault <path>` | `fpfault: done, 0/N faults corrupted FP state` — match the `0/`, not just `done,` |
| `neonfault <path>` | `neonfault: done, 0/N crossing loads wrong` — likewise |
| `mmap_file <path>` | `mmap_file: touched all pages` |
| `allocstress` | `allocstress: reached 2,000,000 allocations without failure!` |
| `eager_mprotect_probe` | `RESULT: PASS` — but it reports **`RESULT: FAIL` on an unmodified tree today** (both phases, "write succeeded, no SIGSEGV — mprotect was defeated", measured 2026-08-15 on `24f7e1c1` at SMP=1 and SMP=4). `verify_trim.py` carries it in `KNOWN_FAIL_EXERCISES` so it reads `KNOWN-FAIL (expected)` instead of masquerading as a regression. See `../archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md` §3, §6a |

### Probes built since this runbook was last revised (added 2026-08-28)

Everything in the table above is a **fork / CoW / fault-path** probe, because
that is what the gate was built to guard. Between 2026-08-19 and 2026-08-27 four
more probes landed against *other* subsystems — AF_UNIX, the socket ioctls, the
ext2 read path — and none of them were in this runbook or in
`verify_trim.py`'s `EXERCISES`. Each row below was **run on a booted VM on
2026-08-28** (`869928e6`, SMP=1, `MEMORY=2048`) and its marker copied from that
run's actual output, per the selection rules in `verify_trim.py`'s module
comment.

| Command | Healthy output | Cost | What it guards |
|---|---|---|---|
| `nettest-unix all` | 10 `[probe] RESULT <mode> verdict=…` lines; **8 `OK` + 2 `UNSUPPORTED`** (`passfd`, `syslog`), **exit 0** | 0.2 s | The AF_UNIX object added 2026-08-23. Two of the defects its audit found were *silent* — `SOCK_SEQPACKET` merging messages and `sendmsg` sending only the first iovec — so this is a data-corruption probe, not a liveness one. **`UNSUPPORTED` is an acceptable verdict and is counted as such by the binary's own exit status**: `passfd` (no `SCM_RIGHTS`) and `syslog` (nothing bound to `/dev/log`) are known gaps, not regressions. Only `TRUNCATED`/`LEAK`/`READINESS`/`FAIL` are findings. Calibrated: same static binary runs on Linux arm64, so **run the Linux arm first** — a mode that fails there is a probe bug |
| `nettest-connect ifconfig` | `[probe] SUMMARY ifconfig checks=29 failures=0` | 0.1 s | 29 `SIOCGIF*` / `SIOCGIFCONF` checks against `lo` and `eth0`. **Needs no host and no network**, unlike every other mode of this binary — which is what makes it gate-safe. It is also the only probe anywhere that reads back `struct ifreq` **packing**: `ifc_len is a multiple of sizeof(ifreq)=40` fails loudly if a `repr(C)` socket type drifts |
| `ext2probe [files_per_dir] [dirs]` | `ext2probe: NO REGRESSION` (the alternative verdict is `ext2probe: REGRESSION`) | **3.4 s at `25 4`** (measured). The `200 16` default is 32x the stress tree and was **not** timed — do not assume it is 32x the wall clock either, since the fixed `base_n = 300` before/after passes dominate this run | Whether ordinary ext2 create/write/read/list measurably degrade *after* a bulk delete ([`../archive/EXT2_PERFORMANCE_AUDIT.md`](../archive/EXT2_PERFORMANCE_AUDIT.md)). **Its verdict is a timing comparison** (>20 % degradation on any single op), so it is the one probe here that a loaded host can flip on its own — check `Time jump detected` before believing a `REGRESSION`. It also **writes ~12.5 MB and deletes it again**; on a disk already at 82 % (measured 2026-08-28) prefer the small argument form |
| `read_syscall_cost` | see "Performance guards" below — this one is a **measurement, not a verdict** | — | **Not staged on `disk.img`.** It was built 2026-08-27 into `bootstrap/bin/`, and the image was last populated 2026-08-26, so `/bin/read_syscall_cost` does not exist in the guest. Re-run `scripts/populate_disk.sh` before reaching for it |

Two host-side probes landed in the same window and are driven from the host, not
over ssh, so they sit outside the exercise table:

| Probe | Use it when |
|---|---|
| [`scripts/probes/listener_backlog_churn.py`](../../scripts/probes/listener_backlog_churn.py) | The change touches `accept`/`listen`/socket teardown. It escalates connect+RST churn and reports the first count that permanently kills a listener, which is what separates "the server leaked its connection pool" (ceiling ~512) from "Akuma's listener pool eroded" (ceiling `MAX_BACKLOG` = 32). Read the `BACKLOG` column of `/proc/net/tcp` alongside it: `0/0/32` is a dead listener |
| [`scripts/probes/redis_write_probe.py`](../../scripts/probes/redis_write_probe.py) | You suspect **partial writes under host load** on a forwarded port. 20 clients × 2000 PINGs, logging actual `send()`/`recv()` sizes — ground truth without tcpdump or sudo |

Three more multi-mode probes sit in the same gap (built 2026-08-17, after the
exercise list above was last extended on 2026-08-15). They are **not measured
here** — each takes a subcommand and, for two of them, a network peer, so there
is no single marker to quote. Reach for them by symptom:

| Probe | Reach for it when |
|---|---|
| `ncaprobe <mode>` (`userspace/ncaprobe/`, build with its `build-musl.sh`) | The change touches **epoll/ET edges, pipes, pidfd, `waitid` or pty**. Modes: `tokio`, `eofedge`, `ptyedge`, `epoll`, `cross`, `fds`, `waitid`, `timeoutleak`, `raw`. Written for [`../archive/TOKIO_PIPE_EPOLL_HANG.md`](../archive/TOKIO_PIPE_EPOLL_HANG.md); every mode is built to run **unchanged under Docker on real Linux**, and that A/B is the whole point |
| `nettest-std`, `nettest-reqwest` (`userspace/nettest/rust/{stdlib,reqwest}/`) | A guest TCP client hangs when the server's **first response byte is delayed**. The pair cuts a four-layer client (tokio + hyper + reqwest + rustls) into one axis at a time: `nettest-std` is the dependency-free half, `nettest-reqwest` is nca's exact stack. Same command grammar and same `[probe]` line vocabulary, so the two runs diff directly. Background: [`../archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md`](../archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md) |

### Performance guards (syscall-dispatch and read-path changes)

None of the tiers above measure *cost*, and two of this repo's optimisation
efforts have a floor recorded that a refactor can silently give back —
`handle_syscall` at **150 ns** (down from 410 ns) with the ~120 ns `wrap` layer
still to come (`../archive/AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`), and the ext2
read path (`../archive/EXT2_READ_PATH_STAGE_PROFILE.md`). Three tools at three
altitudes, all added 2026-08-25..27:

| Tool | Altitude |
|---|---|
| `userspace/ext2probe/c/read_syscall_cost.c` (build: `userspace/ext2probe/c/build.sh`) | **One `read(2)`, split into fixed and per-byte cost.** Three arms — `zero` (`/dev/zero`, no filesystem), `file` (warm `pread`), `null` (zero-length read = the fixed cost, *measured* rather than fitted). Built musl-static from one source for **both** kernels, so an Akuma number and a Linux number differ by the kernel and nothing else |
| [`scripts/benchmarks/read_path_ab.py`](../../scripts/benchmarks/read_path_ab.py) | The same split **inferred** from outside, by fitting a line through two block sizes (`--sweep`). The two disagreeing is itself a finding |
| [`scripts/benchmarks/read_stage_profile.py`](../../scripts/benchmarks/read_stage_profile.py) + `--features read-profile` (`src/syscall/utils/read_profile.rs`) | Splits that one syscall into **kernel-side stages** |

Run these only when the change is in the dispatch or the read path, and A/B them
the same way as everything else — a single number is not a result. They are
**not** part of `--tier all`: they need a quiet host, and the gate deliberately
runs `cargo` immediately before booting.

Omitted from the automated suite **because they do not terminate**, not because
they are uninteresting — measured 2026-08-15, all still running with nothing but
their banner printed: `spawnalias` (>155 s even at `spawnalias 300`), `tidflags`
(>300 s), `clonearg` (>240 s). `termtest` blocks on terminal input. Worth a look
on their own; each is 420 s of `TIMEOUT` per SMP level inside the gate.

**Capture probe output with `rm -f f; … >> f`, never `> f`.** In
`{ probe; echo SENTINEL; } > f` the two child processes share one inherited fd
and must share one file offset; Akuma gives each its own, so the sentinel lands
at offset 0 and eats the first bytes of the probe's output. Measured 2026-08-15:
`{ /bin/echo AAAAAAAAAAAAAAAAAAAAAAAA; /bin/echo BBB; } > f` produces
`BBB\nAAAA…`; with `>>` it produces the correct `AAAA…\nBBB` (O_APPEND writes go
to EOF, so the shared-offset path is never used). A shell **builtin** `echo` is
unaffected — same process, same offset. This silently truncated the head of every
Tier 3 log until `verify_trim.py` was fixed; it went unnoticed because every
marker in the original list sits at the *end* of the output.

`redis-server` is **not** on `disk.img` — do not treat its absence as a failure.
It lives on the devbox image instead; see Tier 4.

**Run anything long under `nohup`, output to a file.** sshd's keepalive kills a
long-lived exec channel and the client reports `Timeout, server localhost not
responding` — which looks identical to a hung VM:

```bash
ssh … "nohup forktest_parent -duration=20s > /tmp/ft.log 2>&1 &"
# poll separately, then: cat /tmp/ft.log
```

Do **not** poll with `pgrep <name>` over ssh: the ssh command line contains the
name, so pgrep matches itself and the job looks eternal.

---

## Tier 4 — `redis-server --test-memory` on devbox-smoltcp (memory-path changes only)

**Grade: B** — the command and its expected output are from the Phase 3 driver
merge (`TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §5.4's verification block, run
2026-08-13). The polling wrapper below has not been re-run as often as Tiers 1–3.

Run this tier when the change touches the PMM, the fault path, CoW, or the OOM /
reclaim escalation. Tier 3 is mostly *fork/CoW* and fault-path exercises — the
closest it comes is `allocstress`, and that leans on the heap rather than on the
page allocator — so this is still the only one that puts sustained pressure on
`alloc_page_zeroed_user` itself.
`--test-memory` allocates the requested MiB and runs a write/read/verify sweep
over all of it, so it walks anonymous demand-paging, `USER_PAGE_RESERVE` and the
reclaim escalation as one continuous workload, and it **verifies the bytes** —
a page the fault path filled from the wrong frame is a reported error, not a
silent pass.

Two scoping caveats, both important:

- **This does not validate redis as a server.** `--test-memory` runs the memtest
  and exits without reaching normal startup, which is why it works while
  `redis-server` proper still blocks (`/proc/<pid>/` is empty, so `/proc/self/smaps`
  is missing — `LONG_ROAD_TO_REDIS.md`). Do not read a passing memtest as "redis
  works now".
- **It needs the devbox image, not `disk.img`.** Redis arrives via `apk add`, and
  the devbox needs a working DNS/HTTP path to do that — so a failure here can be
  a *networking* failure. Establish that `apk add` itself worked before blaming
  the memtest.

```bash
scripts/build_devbox_smoltcp.sh
pkill -f qemu-system-aarch64; sleep 2
overlays/devbox/run-smoltcp.sh > /tmp/devbox.log 2>&1 &
until grep -aqE "Started sshd|sshd started" /tmp/devbox.log; do sleep 3; done
```

`ssh` is blocked by policy; drive it from Python. The memtest is long — run it
detached with a sentinel, exactly like Tier 3's binaries, or sshd's keepalive
kills the channel and the result reads as a hung VM:

```python
import subprocess, time
def sh(cmd, t=180):
    r = subprocess.run(["ssh","-q","-o","StrictHostKeyChecking=no","-p","2222",
                        "root@localhost",cmd], capture_output=True, timeout=t)
    return r.returncode, r.stdout.decode(errors="replace")

# devbox.img fills up across sessions, and ENOSPC surfaces as an `apk add`
# network error. Check before installing, not after it fails.
print(sh("busybox df -h /")[1])
if sh("command -v redis-server")[0] != 0:
    print(sh("apk add redis", t=300)[1])

sh("nohup sh -c '{ redis-server --test-memory 512; echo __EX_DONE__; } "
   "> /tmp/memtest.log 2>&1' > /dev/null 2>&1 &")
out = ""
for _ in range(60):
    time.sleep(10)
    out = sh("cat /tmp/memtest.log 2>/dev/null")[1]
    if "__EX_DONE__" in out:
        break
print(out)
assert "Your memory passed this test" in out, out[-400:]
```

### Verify

`Your memory passed this test`. Anything else is a finding, and the two failure
shapes mean different things:

| Output | Read it as |
|---|---|
| `*** MEMORY ERROR DETECTED ***` with an address | The fault path served a wrong or stale frame — a **data** bug. This is the outcome this tier exists to catch; capture the log and A/B it |
| The process dies with SIGSEGV, or `[Fault] Process N (redis-server) SIGSEGV` in the boot log | OOM, not corruption: the escalation gave up and killed the process. Compare `MEMORY=` and the free-page count against the baseline before calling it a regression |
| `apk add` fails, or `redis-server: not found` | Networking or a full disk (`df` above), not the memory path |

### The redis A/B harnesses (added 2026-08-20..24)

`--test-memory` is the *memory* arm of this tier. Since it was written, a set of
host-side redis harnesses landed for the **network/server** arm — use them when
the change is in the socket table, the poll path or the scheduler rather than
the PMM. All are A/B tools: they take two arms and report the difference, so
none of them has a "healthy number" to quote here.

| Harness | Question it answers |
|---|---|
| [`scripts/benchmarks/run_redis_arm.py`](../../scripts/benchmarks/run_redis_arm.py) | Runs one arm end to end; the building block for the rest |
| [`scripts/benchmarks/redis_bulk_ab.py`](../../scripts/benchmarks/redis_bulk_ab.py) (+ `redis_bulk_check.sh`) | Bulk throughput, arm vs arm |
| [`scripts/benchmarks/redis_conc_sweep.py`](../../scripts/benchmarks/redis_conc_sweep.py) | Does it fall over as concurrency climbs? |
| [`scripts/benchmarks/redis_smp_sweep.py`](../../scripts/benchmarks/redis_smp_sweep.py) | Does adding cores help or hurt? |
| [`scripts/benchmarks/redis_feature_ab.py`](../../scripts/benchmarks/redis_feature_ab.py) | Attributes a delta to one feature flag |
| [`scripts/benchmarks/redis_socket_table_scaling.py`](../../scripts/benchmarks/redis_socket_table_scaling.py) | Whether cost grows with the *socket table*, not the load |
| [`scripts/benchmarks/rtt_load.py`](../../scripts/benchmarks/rtt_load.py), [`nicstat_breakdown.py`](../../scripts/benchmarks/nicstat_breakdown.py) | Round-trip latency under load; where NIC time goes |
| [`scripts/benchmarks/gssh.py`](../../scripts/benchmarks/gssh.py) | Drives the guest over ssh from Python — `ssh` is blocked by policy for the agent, and this is the shared wrapper the others use |

Also run it at a size that does **not** fit, once, when the change touches the
escalation: `--test-memory 8192` in a 4 GiB box must SIGSEGV the process and
leave the VM alive and sshable. An invented OOM and a real one look identical in
the log, so the check here is that the box survives and the *next* boot's
`[PASS]` count is unchanged — not the memtest's own output.

### One reading this gate produced that nobody could reproduce

On the Phase 5 sweep's arm (2026-08-14) a single run reported
`host.failed: 1` / `host.tests: 418` — a total 103 short of the 521 the same tree
scores, which is the shape of **one test binary aborting partway** rather than a
test failing. Five subsequent runs on the identical tree (three bare
`cargo test`, two full Tier 1) all scored 521/0, and per-crate runs of
`akuma-exec` (the only suite large enough to account for the gap) scored 236/0
three times.

It is recorded because it is unexplained, not because it was explained away: the
gate did not capture `cargo test`'s output, so there was no evidence to diagnose
after the fact. **If you see a short total with `host.failed: 1`, re-run before
believing it.**

> **It recurred on 2026-08-14, and the delta is the finding: 103, both times.**
> Second occurrence was on the `lto = "thin"` arm — `host.failed: 1`,
> `host.tests: 430` against the 533 the same tree scores, i.e. **exactly the same
> 103-test gap** as the first occurrence's 418-against-521, on a different tree,
> a different commit and a different profile. Four re-runs scored 533/0.
>
> That the gap is *identical* while the tree's total grew by 12 rules out a whole
> test binary being lost — a binary's own count moved between the two dates, and
> 103 did not. So it is the same ~103 tests going missing from the same point,
> which is a much narrower hypothesis than "something aborted".
>
> It is **not** LTO: the first occurrence predates that change entirely (Phase 5
> sweep, no `lto` key anywhere).
>
> `tier1_tests` now **writes `verify_host_tests.log` into the gate's log dir on
> every run** and reports `host.failed_names` plus `host.output` when anything
> fails — the change this section used to ask for. A passing run's file is the
> baseline to diff the next failure against, so on the third occurrence this
> should be answerable in one command instead of a session.

## Tier 5 — self-host clean-build trials (page-table / mmu / fault-path changes)

For changes inside `crates/akuma-exec/src/mmu/` or the fault/CoW/file-page-cache
path, add clean-build kernel-compile trials: nothing else in this gate puts
rustc-scale fork/mmap/demand-paging load through those exact walks. Procedure
and Verify block: [`selfhost-kernel-build.md`](selfhost-kernel-build.md)
§ "Run a build trial". The three rules that matter here:

- **`cargo clean` before every trial** — a green incremental build proves
  nothing, and no script issues the clean for you.
- **A/B it**: same number of trials on a worktree at the parent commit. The
  2026-08-15 baseline of 10/10 clean builds **no longer reproduces** — re-measured
  2026-09-01, both arms score **8/10**, because the kernel heap leaks
  monotonically across trials (13 → ~758 MB) until the OOM killer takes rustc at
  trial 9 and the VM stops answering at trial 10. That is pre-existing and
  A/B-confirmed, so **cap the campaign at ~8 trials per boot** and do not read a
  trial-9 or trial-10 death as your change:
  [`../archive/SELFHOST_KERNEL_HEAP_LEAK.md`](../archive/SELFHOST_KERNEL_HEAP_LEAK.md).
  Within those 8 a single red trial on your arm is still a real finding — but
  confirm the baseline on *your* image before concluding, and check the tripwire
  greps (`[PMM-RESURRECT]` etc.) even on green runs.
- **Do not retry past a failure** — capture both logs and match against that
  runbook's Common failures table.

**A trial is ~2 minutes, not ~10** (re-measured 2026-08-16: five consecutive
trials at 131/132/131/132 s wall clock, boot + `cargo clean` + build inclusive,
on `devbox.img` at `MEMORY=8192 SMP=4` with `-j4 --offline`). The
[selfhost runbook](selfhost-kernel-build.md) § "How long a trial takes" has the
configuration the number belongs to, and the one-time `cargo fetch` that
`--offline` needs before the first trial.

So five trials per arm costs ~11 min, not an hour — **run more than five.** Ten
per arm is ~22 min unattended, and against a stochastic class that is the
difference between a result and an anecdote: this gate's own `cowstale` entry
records a flake that shows up ~2-in-5, which five samples cannot separate from
noise on either arm.

## Before calling anything a regression

Three of the "findings" during Phase 6 item 1 were the measurement, not the code:
a stale VM holding the ports, ssh's banner folded into a stdout parse, and a
`pgrep` matching its own command line. So:

0. **Check whether the host was starving QEMU**, before anything else:
   `grep -ac 'Time jump detected' <log>`. A healthy run is 0. This is the cheapest
   check here and it invalidates a whole run on its own — see the known-benign table.
   Do not edit files while the gate is booting; that alone can cause it.
1. **A/B against a `git worktree` at the parent commit.** Logs in `logs/` are
   weeks old and predate current tests — `STACK-OVERFLOW` is absent from all of
   them purely because the test that emits it did not exist yet.
2. **Separate stdout from stderr** in any ssh harness (`-q` plus
   `capture_output=True`, and read `.stdout` alone).
3. **Pick controls that exist.** `/proc/uptime` and `/etc/hostname` are absent
   on `disk.img`; `/hello.c` and `/bin/busybox` are present.
4. **Never assume a console string arrives contiguously at SMP>1.** The cores
   interleave, and a line can land torn in half. Measured 2026-08-16 on a
   devbox-smoltcp boot at SMP=4: `[herd] Started sshd (pid= 2)` came out as

   ```
   [herd] Starting service: sshd
   sshd (pid= 2)
   ```

   so neither `Started sshd` nor `sshd started` appeared anywhere, on a VM that
   was entirely healthy — herd at PID 1, sshd at PID 2 accepting at 640
   syscalls/s, a session handler already forked at PID 3. A harness gated on
   that string waits out its whole budget and then reports a boot failure. Cost
   that day: one 12-minute Tier 5 trial, scored `BOOT_FAIL` against a kernel
   that had booted fine. `wait_for_marker` now also matches the surviving tail
   `sshd (pid=`; **if you write your own harness, gate on an ssh round-trip
   instead** — it tests the precondition you actually need and no other core's
   printf can tear it.

## What to report

Per that document's own lesson, line-count deltas are the wrong metric — a merge
whose point is to build a seam pays for the seam. CPD is also nearly blind here
(measured: 6% of a real 130-line reduction). Report:

- **definitions collapsed** and **dependency edges cut** (`cargo tree`, not
  `use` statements — an import-list grep over-counted by one whole crate once)
- **behavioural differences found between the copies**, and the decision for
  each. Every pair so far had 3–4, usually hiding in comments and observability

## Background

- [`../archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](../archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
  — the survey, the phase list, and §6.1's host-testability finding
- [`find-duplicated-code.md`](find-duplicated-code.md) — running CPD and why its
  numbers are a lower bound
- [`../archive/DEVBOX_ISSUES.md`](../archive/DEVBOX_ISSUES.md) — Issue 11, the
  spurious boot-time `[STACK-OVERFLOW]` class
