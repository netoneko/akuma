# Verify a trim-the-fat change (no-regression gate)

**Grade: A** — every command here was run end-to-end on 2026-08-13 for Phase 6
item 1 (the `channel.rs` FIFO merge).

For deduplication / extraction work from
[`../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md).
Those changes are supposed to be behaviour-preserving, so the gate is a
**comparison against a baseline**, not a green checkmark.

Work in two tiers. Tier 1 is host-only, takes ~2 minutes, and catches most of
it. Tier 2 needs a VM and only runs when Tier 1 is clean.

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

> **Baseline as of 2026-08-13: 486** — was 455 when this runbook was written, 463
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
| `bssfork spread=1` | **BROKEN PRE-EXISTING — not usable as a control.** Measured 2026-08-13 at SMP=4: fails on `main` (b585aed) with `failures=7`, `thread=7 [never ran] ticks=0`, and fails *worse* on `trim-some-more-fat` (1a5a266) with `failures=8 ticks=0` — no thread runs at all. Reproduced on an unmodified `git worktree` of each. Plain `bssfork` (spread=0) PASSes on both and is the control to use. The 8/8-vs-7/8 gap between main and the branch is an unexplained regression in its own right |
| `cowstale` | `reader_faults=0 failures=0` … `cowstale PASS` |
| `mmapsum <path>` | three digests (`madv:`/`mtA:`/`mtB:`); **needs a path argument** |
| `forktest_parent -duration=20s` | `All children processed via epoll. Parent exiting.` |

`redis-server` is **not** on `disk.img` — do not treat its absence as a failure.

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

## Before calling anything a regression

Three of the "findings" during Phase 6 item 1 were the measurement, not the code:
a stale VM holding the ports, ssh's banner folded into a stdout parse, and a
`pgrep` matching its own command line. So:

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
