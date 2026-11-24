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

## The slow tests were unoptimized crypto, not too many fuzz tests

Per-test times (`cargo test -- -Z unstable-options --report-time`, 3 threads, 6 pinned cores, `dev` profile):

| test | before |
|---|---|
| `akuma-wpa` `key_parse_and_supplicant_survive_mutated_messages` | 1190 s |
| `akuma-wpa` `key_unwrap_survives_every_length_and_random_bytes` | 267 s |
| `akuma-wpa` `spoofed_message_1s_are_harmless`, `rekeys_converge_on_a_lossy_link` | 179 s, 175 s |
| `akuma-wpa` `the_handshake_converges_under_loss_duplication_reordering_and_corruption`, `a_long_bad_day` | 103 s, 65 s |
| `akuma-ext2` pin/unlink and rename tests (correctness, not fuzz) | 69-79 s each |

`akuma-wpa` alone was ~33 minutes of the hook. Its tests run SHA-1, PBKDF2 and AES 100,000 times per test in an
unoptimized build. `[profile.dev.package.akuma-wpa] opt-level = 3` in the root `Cargo.toml` (debug assertions and
overflow checks stay on) brings the crate to **~50 s** (the fuzz test 36 s, the next 9 s) with all 29 tests passing and
none disabled. Repo-wide there are 40 test functions named fuzz/survives/mutated/random_bytes in 24 crates and only 5
`#[ignore]`s (nvme, fbcon image writers); a per-test opt-in is the next step only if a crate is still slow after this.
Still open: the `akuma-ext2` tests at ~70 s each under load.

## Why the sixteen builds were slow (2026-11-24)

Each kernel build had its own `CARGO_TARGET_DIR` (`$GATE/target/<step>`), so the ~150 workspace crates every kernel
depends on were compiled sixteen times, at `opt-level=3` with thin LTO, on six pinned cores at nice 15, one build at
a time: 180-350 s per kernel on a cold directory. Builds now share **one target dir per target triple**
(`shared_target_dir` in `scripts/gate.py`); cargo keys artifacts by feature set, so kernels with identical dependency
features reuse each other's compiles. A shared dir holds only the last variant of a package built into it, so each
build's kernel is copied to `$GATE/kernels/<step>` and the boots read that copy. Expected, **not yet measured**: the
second and later kernels of an architecture build in the time of their own LTO link, not a full compile. Cold runs
(the first run after this change, or a new toolchain) still pay once per architecture. Also: the amd64 boot timeouts
went from 150 s to 420 s, because the self-test boot takes ~250 s on the pinned slice.

## Verified

- `pin_to_cores("12-13")` on the box: affinity `[12, 13]`, niceness 19 (inherited from the shell), 2 cargo jobs,
  2 test threads. `gate.py --list` runs. A full pinned gate run was started after the change; see its result in the
  commit that merged this.
- The hook now exits at once when no Rust-affecting path is staged (2026-11-24). For a Rust commit it still runs clippy and the tests on every crate and is not pinned; run the commit under
  `taskset`/`nice` for now. It repeating the gate's host tests is open (`proposals/FUTURE_WORK.md`).

## Lessons

- A tool that sizes itself from `nproc` is a bug on a machine someone is sitting at. Bound it at the entry point
  (affinity, priority), not per child.
- Two checks that need the same expensive input should be given it once.
- A hook that was never installed hides its cost; when it is installed, measure what it takes on a laptop.

## Background

`docs/runbooks/merge-gate.md`, `docs/archive/HOW_MAIN_BROKE_2026_11.md`, `docs/archive/MERGE_GATE_FIRST_RUN.md`.
