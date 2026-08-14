# Verify a trim-the-fat change (no-regression gate)

**Grade: A** — every command here was run end-to-end on 2026-08-13 for Phase 6
item 1 (the `channel.rs` FIFO merge).

For deduplication / extraction work from
[`../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md).
Those changes are supposed to be behaviour-preserving, so the gate is a
**comparison against a baseline**, not a green checkmark.

Work in tiers, each one gated on the last being clean: **Tier 1** is host-only and
takes ~2 minutes; **Tier 2** boots the VM for the self-test suite; **Tier 3** is
the live I/O and fork/CoW binaries; **Tier 4** is the redis memtest on the devbox,
for changes in the memory path. Tiers 3 and 4 are conditional — read their
headers before spending the time.

**Tiers 1–3 are automated: run [`../../scripts/verify_trim.py`](../../scripts/verify_trim.py)
rather than hand-assembling the commands below.** It runs the four clippy
configurations, the host-test count, both SMP levels and the Tier 3 binaries, and
prints one `=== SUMMARY ===` block designed to be diffed against a run on your
parent commit. Read its module docstring first: every measurement in it is there
because doing that measurement by hand gave a wrong answer at least once.

```bash
scripts/verify_trim.py --out mine.txt
git worktree add /tmp/base <parent-commit>
(cd /tmp/base && scripts/verify_trim.py --instance 1 --out /tmp/base.txt)
diff /tmp/base.txt mine.txt
```

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

> **Baseline as of 2026-08-14: 521** — after the Phase 5 user-copy sweep (+5
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
parallel execution. Background: `TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §6.1.

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
grep -ac '\[PASS\]' /tmp/mine.log                       # expect 94
grep -aoE '\[FAIL\] [a-z_0-9]+' /tmp/mine.log | sort -u  # expect exactly one line
```

**Baseline 2026-08-13: 94 `[PASS]`, and the failure set is exactly
`retired_reclaim_ab`** — that one fails on an unmodified tree (threshold too
tight, `TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §8.5 Phase 0).

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
| `cowstale` reports `UNEXPECTED` **at SMP=2** with `[Fault] … FAR=0x420908 ELR=0x403a90 ISS=0x4f` and `[WPF] … va=0x420000 cow_ref=0 … ap_rw=true` | **Pre-existing, ~40% per run, and not a regression in whatever you are testing — A/B'd 2026-08-14 at 2/5 on *both* arms** (changed tree and a worktree at its parent), with a byte-identical signature every time. `ap_rw=true` means the page table already grants the write: the fault was taken before a sibling repaired the page and judged after, i.e. the stale-write-fault class [`../archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](../archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md) §12 root-caused and `stale_write_fault_absorbed` fixed — its absorb has a residual hole at this width, and its own boot test passes in the same boot that then crashes. **Note SMP=2 is not in the gate's default `--smp 1,4`**, so nothing else samples it; if you add it, sample **five runs per arm** before believing either result |
| `smp4.fpcache` present in one summary and absent in another | Harness timing, not behaviour. `[FPCACHE]` is emitted periodically and the gate snapshots the boot log at the sshd marker, so the line lands before the snapshot in some runs and after it in others. Compare `entries=`/`misses=` (stable) and ignore `hits=` (a monotonic counter read at an arbitrary instant) |

---

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
| `bssfork` | `failures=0` … `bssfork PASS` |
| `bssfork 20 8 1` | `failures=0` … `bssfork PASS`. **Not `bssfork spread=1`** — the binary's CLI is positional (`bssfork [rounds] [threads] [spread]`), not `key=value`; running the literal string `spread=1` feeds it into `rounds`, `strtoul` parses that as `0`, and `spread` silently defaults to `0` too. `rounds=0` skips the fork loop entirely, so `g_stop` fires almost instantly and the liveness check flags threads `[never ran]` before they get scheduled at all — nothing to do with CoW or the kernel. **Corrected 2026-08-14**: the "BROKEN PRE-EXISTING" verdict recorded here on 2026-08-13 (`failures=7`/`8`, `ticks=0`, "unexplained regression") was this same mis-invocation on both `main` and the branch; the real control, invoked correctly, passed 8/8 clean runs at SMP=4 on first re-check. See `docs/archive/PMM_EXTRACT.md` §8 for the full correction |
| `cowstale` | `reader_faults=0 failures=0` … `cowstale PASS` |
| `madvshared` | `madvshared: ALL PASS`. `MADV_DONTNEED` on a CoW-shared frame must not touch the peer's page — the null-`Rc` mechanism (`../archive/CARGO_HEAP_NULL_RC.md`), fixed 2026-08-14. Deterministic, milliseconds, no allocator involved, and **calibrated**: the identical static binary PASSes all three phases on real Linux arm64 (`docker run --rm --platform linux/arm64 -v "$PWD:/w:ro" alpine /w/madvshared`), so a FAIL is the kernel, not the probe. Before the fix it reported `2 FAIL` at both SMP=1 and SMP=4 |
| `mmapsum <path>` | three digests (`madv:`/`mtA:`/`mtB:`); **needs a path argument** |
| `forktest_parent -duration=20s` | `All children processed via epoll. Parent exiting.` |

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
merge (`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §5.4's verification block, run
2026-08-13). The polling wrapper below has not been re-run as often as Tiers 1–3.

Run this tier when the change touches the PMM, the fault path, CoW, or the OOM /
reclaim escalation. Every binary in Tier 3 is a *fork/CoW* exercise; this is the
only one that puts sustained pressure on `alloc_page_zeroed_user` itself.
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
gate does not capture `cargo test`'s output, so there is no evidence to diagnose
after the fact. **If you see a short total with `host.failed: 1`, re-run before
believing it — and consider teaching `tier1_tests` to save the failing output**,
which is the change that would turn this from noise into a finding.

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

## What to report

Per that document's own lesson, line-count deltas are the wrong metric — a merge
whose point is to build a seam pays for the seam. CPD is also nearly blind here
(measured: 6% of a real 130-line reduction). Report:

- **definitions collapsed** and **dependency edges cut** (`cargo tree`, not
  `use` statements — an import-list grep over-counted by one whole crate once)
- **behavioural differences found between the copies**, and the decision for
  each. Every pair so far had 3–4, usually hiding in comments and observability

## Background

- [`../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md)
  — the survey, the phase list, and §6.1's host-testability finding
- [`find-duplicated-code.md`](find-duplicated-code.md) — running CPD and why its
  numbers are a lower bound
- [`../archive/DEVBOX_ISSUES.md`](../archive/DEVBOX_ISSUES.md) — Issue 11, the
  spurious boot-time `[STACK-OVERFLOW]` class
