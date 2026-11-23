# The merge gate and the pre-commit hook took every core (2026-11-23)

## Symptom

Running `scripts/gate.py` on the 16-core dev box (ryzen laptop, desktop session open) drove the load average to
42 and made the machine unusable. Two identical `cloc_akuma.py --json src crates taoxichuan/src` processes ran for
~4 minutes each, and an `akuma_ext2` test binary sat at ~375% CPU. Orphans of a killed gate run kept going after
`SIGTERM` (cargo's children are reparented to the systemd user manager).

## When it started

It did not used to be like this; two changes landed together on **2026-11-19** and the standing commit rule made them
routine:

| date | commit | what changed |
|---|---|---|
| 2026-08-16 | `26644e05` | the pre-commit hook script is written (greps, clippy and tests over `crates/*`). It was **not installed** in clones (`core.hooksPath` unset), so it did not run; `aadd4a76` records the clippy rot that followed |
| 2026-11-01 | `d961f50d` | `check_budgets.py` is added beside `check_unsafe_budget.py`: both lex the whole tree through `cloc_akuma.py` |
| 2026-11-05 | (`CLAUDE.md`) | standing instruction: commit and push after every verified task, so every code task now meets the hook |
| 2026-11-19 | `aadd4a76`, `03f98ff6` | the hook gains `cargo check` of the three platform kernels on both architectures, `scripts/install-hooks.sh` makes it run in every clone, and `scripts/gate.py` is added |

`gate.py` was written in that one commit with the sizing it still has: build slots `min(cpus // 4, mem_gb // 3, 6)`,
boot slots `min(cpus // 2, 6)`, taken from the *whole machine*, no affinity, no niceness, and no cap on the threads
each child starts. Nothing before 2026-11-19 ran clippy over ~150 crates, checked six kernel/arch pairs and then ran
the whole gate on every commit and merge.

## Cause

Parallelism multiplied at three levels, none of which knew about the others:

1. **Slots.** 4 build slots on 16 cores, each running a cargo build whose `rustc` uses every core (~450% each).
2. **Child threads.** `cargo test -p <every crate>` (`static:host-tests`) starts each test binary with one thread per
   core (`RUST_TEST_THREADS` unset); `akuma-ext2` has 147 tests and used ~375%. `CARGO_BUILD_JOBS` was unset too.
3. **Duplicated work.** The gate's `static:check_unsafe_budget` and `static:check_budgets` each ran the same
   `cloc_akuma.py --json` over the same tree concurrently (255 s and 256 s in the first run). The pre-commit hook then
   repeats the clippy and test work the gate does (host tests), so one merge pays for it more than once.

## Fix (`scripts/gate.py`, `check_budgets.py`, `check_unsafe_budget.py`)

- `gate.py` **pins itself**: `pin_to_cores()` sets the affinity of the gate process to a fixed core set (children
  inherit it), lowers its priority (`nice 15`), and sizes the slots from the cores it owns, not the machine's.
  `GATE_CORES=10-15` picks the cores; the default is the upper half of the cores the process may use.
- It sets `CARGO_BUILD_JOBS` and `RUST_TEST_THREADS` from that core count (callers may still override them).
- `prep:cloc` runs the lexer **once** and writes `$GATE_DIR/cloc.json`; the two ratchets read it through
  `CLOC_JSON` and still compute it themselves when run by hand.

## Verified

- `pin_to_cores("12-13")` on the box: affinity `[12, 13]`, niceness 19 (inherited from the shell), 2 cargo jobs,
  2 test threads. `gate.py --list` runs. A full pinned gate run was started after the change; see its result in the
  commit that merged this.
- Not changed: the hook. It still runs clippy and the tests on every crate and is not pinned; run the commit under
  `taskset`/`nice` for now. It repeating the gate's host tests is open (`proposals/FUTURE_WORK.md`).

## Lessons

- A tool that sizes itself from `nproc` is a bug on a machine someone is sitting at. Bound it at the entry point
  (affinity, priority), not per child.
- Two checks that need the same expensive input should be given it once.
- A hook that was never installed hides its cost; when it is installed, measure what it takes on a laptop.

## Background

`docs/runbooks/merge-gate.md`, `docs/archive/HOW_MAIN_BROKE_2026_11.md`, `docs/archive/MERGE_GATE_FIRST_RUN.md`.
