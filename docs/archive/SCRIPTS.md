# scripts/ cleanup audit — what's dead, what's kept, and why (2026-08-07)

Companion to [`../reference/scripts/`](../reference/scripts/README.md) (the
current-state index) and [`../reference/overlay/`](../reference/overlay/README.md)
(the devbox overlays). This is the detailed pass behind both: per-script
history, the evidence for each removal, and the reasoning for everything kept
as-is. Driven by `TRIM_FAT_REMOVAL_FEASIBILITY.md`'s Scripts section ("clean up the
scripts that are not relevant anymore… reference useful scripts in
docs/reference/ and delete useless ones" + "Move scripts/*_repro to
akuma-playground or remove outright, need to understand usefulness").

## Method

For every top-level file/dir under `scripts/` (excluding `__pycache__`,
already gitignored): read the header comment/docstring, `grep -rl` the
filename across `docs/`, `acceptance/`, and the rest of `scripts/` for
cross-references, and check `git log -1` for how old and how singular the
commit is. A script with zero cross-references, an old single-purpose commit
message, and nothing left for it to act on is a deletion candidate; a script
with cross-references or a still-live target is kept, documented if it
wasn't already.

`akuma-playground` — the destination CLEANUP.md names for repro scripts —
does not exist anywhere in this repo or its git history. Rather than create a
new sibling project for a single script, the repro-class script found here
was deleted outright (user decision, asked directly rather than guessed).

## Removed

### `scripts/ssh_stall_repro.sh` (last touched 2026-05-29, "more ssh fixes")

A thin wrapper: runs `ssh_harness.py parallel --count=4 --duration=15` against
a running VM, sleeps 12s, then greps the kernel log for `[SSH]`/`[NET]`
lines — built to catch the "Phase-1 instrumentation fingerprint" for the SSH
accept-loop stall described in `STABILITY_URGENT_ISSUES.md`. That bug is
long fixed — `BUG_FIX_LIST.md`'s `STABILITY_URGENT_ISSUES.md` entry lists
"Connect-storm stall root-caused and fixed (Phase 2)" — and the script has no
cross-references anywhere else in the tree. `ssh_harness.py` itself (the
generic parallel-SSH-connection driver it wraps) is kept; only the one-off
grep-for-a-fixed-bug wrapper around it is gone. This is the literal
`scripts/*_repro` case CLEANUP.md calls out; per the user, deleted outright
rather than moved to a new project.

### `scripts/test_sched_bklfree_sanity.sh` (last touched 2026-07-24, "first attempt to introduce network lock")

167 30-second-boot smoke test for the BKL fair-FIFO ticket-leak fix
(`sched_bklfree_el0`): builds `release-smp-shared`, boots SMP=4 in the
background, sleeps 25s, and just checks the QEMU process is still alive —
`kill -0 $QEMU_PID`. No log-content assertion, no pass/fail signature beyond
"didn't crash in 25 seconds." Zero cross-references. Superseded by
`scripts/test_sched_bklfree_ticket_fix.py` (140 lines, same bug, same M5c
step-2 fix, but actually SSHes in and checks for ticket-accounting drift
rather than just polling the process table) — kept, see
[`../reference/scripts/fork-smp-harnesses.md`](../reference/scripts/fork-smp-harnesses.md).
The underlying bug is fixed and has a real regression check elsewhere; this
was the earlier, weaker one-off, not a repro name but the same shape.

### `scripts/fix_format_macros.py` (last touched 2026-01-22, "it got worse")

A one-time AST-free regex migration tool: rewrites
`console::print(&format!(...))` / `crate::console::print(&alloc::format!(...))`
into `safe_print!(SIZE, ...)`, estimating the buffer `SIZE` from the format
string length. This is the automation behind the project-wide move to
`safe_print!` (heap-free, safe to call from a secondary core). Checked
whether it still has work to do: `grep -rn "console::print(&format!\|console::print(&alloc::format!" src/`
returns **zero matches**. The migration it automates is complete; there is
nothing left in the tree for it to rewrite, and it had zero cross-references
even before that check.

### `scripts/sqlite/schema.sql` (added 2026-01-26, "hell yeah sqld status works")

A lone `CREATE TABLE messages (id, message)` schema file, no README, no
script anywhere that reads or writes it, no doc that mentions it. The commit
message ("sqld status works") suggests a very early, since-abandoned
experiment (possibly evaluating an in-VM or host-side sqlite/`sqld` daemon
integration) that never grew beyond one schema file. Zero cross-references
in `docs/`, `scripts/`, or `src/`. Removed as orphaned cruft — if a sqlite
integration is revisited later, it should start from the current
requirements, not a two-line schema with no context.

## Kept, now documented (previously invisible to `docs/reference/`)

These had no README, no doc pointer, and no mention anywhere under
`docs/reference/` before this pass — CLEANUP.md's "reference useful scripts"
ask. Full tables are in [`../reference/scripts/`](../reference/scripts/README.md);
the detail below is what didn't fit there.

### Log & crash analysis

- **`analyze_crash.py`** (445 lines) — the largest of this group by far.
  Parses a `SwitchEvent` dataclass out of kernel crash/serial logs to spot
  context-switch irregularities. General-purpose enough that it's worth
  keeping even though its docstring names one specific use
  (`crash132.log`) — the parsing logic isn't tied to that one log.
- **`capture_serial_forktest_mmap.sh`** (30 lines) — despite the
  forktest/mmap-specific name, this is a generic "capture QEMU's `mon:stdio`
  serial to a file while driving something over SSH in another terminal"
  pattern; the grep pattern it prints at the end
  (`[mmap]|[DA-MISS]|[DA-DP]|[WILD-DA]|[Fault]|exit_group`) is what's actually
  scoped to mmap/forktest, not the capture mechanism itself.
- **`ext2read.py`** (97 lines) — a minimal read-only ext2 extractor
  (superblock + group descriptors + inode table walk, no mount, no kernel
  needed) for pulling one file out of `disk.img` when a VM is too wedged to
  SSH into. One cross-reference elsewhere in the tree; kept as a genuinely
  useful last-resort tool distinct from the full `akuma-ext2` crate.

### Multi-VM / hang hunting

- **`run_multiple.sh`** (167 lines) — launches N parallel boots, each with
  its own disk/port-band/log, plus a log-stall watchdog (`STALL_SECS`,
  default 20) that flags an instance as a suspected hang. Referenced four
  times from `docs/archive/STABILITY_URGENT_ISSUES.md` as the actual tool
  used to hunt the connect-storm stall — this is load-bearing history, not a
  stray mention.
- **`run_two_vms.sh`** (216 lines) — boots the two-VM agent demo (a `meow`
  client VM + a `llama.cpp` server VM on fixed, deterministic SLIRP ports:
  meow `ssh=2222 http=8080`, llama `ssh=2322 http=8180`). This is the one
  script in the whole audit with the most external load: it's invoked
  directly from `acceptance/03_two_vms_agent_workflow.md`, i.e. it's part of
  the numbered acceptance-playbook suite, not just a debug aid.

### Fork / SMP regression harnesses

Six scripts, 1,064 lines combined, all following the same shape: boot
`devbox-smoltcp` (or `release-smp-shared` directly) at a chosen `SMP=` level,
wait for SSH, run a specific stress pattern, grep the log for a specific
fault signature.

- **`validate_fork_smp.py`** (304 lines, the largest) — 16 concurrent SSH
  connections each fork-hammering via a busybox loop at SMP=4; success bar
  is literally zero fault lines across N boots × M rounds. This is the
  harness behind the fork/CoW/TLB corruption class documented across
  `SMP_SHARED.md`, `TRAMPOLINE_STALE_PROCESS_RELR.md`, and friends.
- **`quick_forktest.py`** (130 lines) / **`forktest_smp_matrix.py`**
  (183 lines) — the same underlying Go `forktest` binary, two granularities:
  quick is a 30s sanity pass at SMP=2/4, matrix runs it across five parameter
  sets (basic, mmap, file I/O, signal, goroutine stress) at the same SMP
  levels. Kept both rather than merging: quick is what you run after a small
  change, matrix is what you run before trusting a bigger one.
- **`test_memory_split.py`** (140 lines) — not a crash hunter but a
  characterization sweep: boots at a matrix of `MEMORY=` sizes, compiles
  `hello.c`/`hello.rs` at each (tcc for small sizes, rustc for larger),
  records pass/fail + fault `FAR` + free RAM, writes
  `logs/split_summary.txt`. This is the tool behind the kernel/user VA-split
  and identity-map-cap characterization referenced from the memory
  subsystem's history.
- **`sshd_crash_hunt.py`** (167 lines) — narrowly scoped repro for one named
  crash class (SMP=4 fork-hammer WILD-DA `FAR=0x0`), structurally identical
  to `validate_fork_smp.py` but smaller and single-purpose. Kept rather than
  folded into `validate_fork_smp.py` because the two target genuinely
  different fault signatures and merging them would make failures harder to
  attribute.
- **`test_sched_bklfree_ticket_fix.py`** (140 lines) — see the "Removed"
  section above; this is the harness that survived where its sibling
  (`test_sched_bklfree_sanity.sh`) didn't.

### Container / environment helpers

- **`alpine.sh`** (12 lines) — one `docker run --platform linux/arm64
  alpine:latest sh` with the repo mounted at `/akuma`. Trivial, but it's the
  fastest way to sanity-check a cross-arch command before wiring it into a
  real build script, and there was no existing pointer to it anywhere.
- **`build_static_curl.sh`** (17 lines) — builds curl statically against
  mbedTLS inside an Alpine builder (`./configure --disable-shared
  --enable-static --with-mbedtls`, `make LDFLAGS=-all-static`). Exists
  because bootstrap/devbox images sometimes need a real static `curl`
  binary rather than busybox's `wget`, which doesn't speak the same feature
  set.

## Kept as-is, no change needed

- **Core build/run/test scripts** named directly in the top-level
  `CLAUDE.md` (`create_disk.sh`, `populate_disk.sh`, `cargo_runner.sh`,
  `build_size.sh`, `build_extreme_size.sh`, `build_devbox.sh`,
  `build_devbox_smoltcp.sh`, `build_docker.sh`) — these are the build system
  itself, already documented in
  [`../reference/build-system.md`](../reference/build-system.md).
- **`ssh_harness.py`** — generic parallel-SSH-connection test driver, used
  directly by this session (see `count_archive_bugfixes.py` /
  `count_individual_fixes.py` / `extract_fix_evidence.py` below) and by the
  now-deleted `ssh_stall_repro.sh`. Kept; it was the wrapper that was dead
  weight, not the harness underneath it.
- **`symbol_sizes.py`, `cloc_akuma.py`** — already referenced from
  `build-profiles.md` and `LINE_COUNT_ANALYSIS.md` respectively; no gap to
  close.
- **`count_archive_bugfixes.py`, `count_individual_fixes.py`,
  `extract_fix_evidence.py`** — the heuristic bugfix-counting tools behind
  [`BUG_FIX_LIST.md`](BUG_FIX_LIST.md). Used in this same session to compute
  the 2026-08-07 second-pass update (+4 fixes / +4 docs) — see that file's
  Statistics section.
- **`selfhost_driver.py`, `run_selfhost_kernelbuild.py`,
  `loop_selfhost_kernelbuild.py`** — actively used self-hosting build
  drivers; already referenced from `docs/runbooks/selfhost-kernel-build.md`.
- **`bkl_rustc_bench/`, `bkl_smp_regimen/`** — self-contained campaign
  harnesses, each with its own `README.md` explaining its pieces; not
  duplicated here or in the reference index. Both are live tooling for the
  ongoing BKL Phase 7 work (`docs/archive/BKL_PHASE7F_OPTOUT_LIST.md` §11 — the
  workplan runbook that used to be cited here has been deleted).
- **`docker/`, `build_docker.sh`, `sqlite/`'s sibling directory `docker/`**
  (unrelated to the removed `sqlite/schema.sql`) — `build_docker.sh` copies
  the kernel binary into `scripts/docker/` and builds the `akuma-qemu`
  image from `scripts/docker/Dockerfile`; referenced from
  `docs/archive/DOCKER.md`. Functional, kept.
- **`bkl_rustc_bench/results_*_2026-08-01.txt`, `cargoproj.tar`** — untracked
  benchmark output/inputs already present before this audit; left alone,
  not this pass's to judge.

## Overlays

`overlays/devbox/` and `overlays/devbox-smoltcp/` are two *different*
directories that both boot something called "devbox-smoltcp," which is
confusing enough to be worth writing down precisely (see
[`../reference/overlay/`](../reference/overlay/README.md) for the concise
version):

- `overlays/devbox/run-smoltcp.sh` — committed 2026-07-19 13:46:48 +0300,
  message "smp m0". This is **the actual current default devbox**:
  `release-smp-shared` profile, real shared-kernel SMP (`SMP=` cores), the
  userspace `/bin/sshd` via herd, default features kept (smoltcp stays
  compiled in and is what box 0 actually uses).
- `overlays/devbox-smoltcp/run.sh` — committed 2026-07-19 12:59:13 +0300,
  message "smaller devbox" — **~47 minutes earlier the same day**. A
  single-core, `--no-default-features` build with an explicit feature list
  that drops every rump feature, built specifically to be a clean rump-free
  control against `overlays/devbox`'s rump path for the sysproxy latency
  measurement in `rump-stack.md` ("Rump tax vs native smoltcp," ~8.7× on
  HTTP GET, ~6× on HTTPS). SSH here is the **built-in in-kernel** server on
  `:2222`, not the userspace one.

Neither is dead: `run-smoltcp.sh` is the one CLAUDE.md's Build & Run section
points at, and `run.sh` under `devbox-smoltcp/` is still the tool that
produces the rump-tax number cited in the networking reference doc. They
were left as two directories rather than merged or renamed — that's a
naming decision for whoever owns the overlay tree next, not something this
audit changed.

## Background

- `TRIM_FAT_REMOVAL_FEASIBILITY.md` — the source of this whole pass (Userspace,
  Scripts, and Kernel sections; only Scripts + Userspace were done here).
- [`../reference/scripts/`](../reference/scripts/README.md) — the
  current-state index this doc backs.
- [`../reference/overlay/`](../reference/overlay/README.md) — same, for the
  devbox overlays.
- [`BUG_FIX_LIST.md`](BUG_FIX_LIST.md) — updated in the same session using
  `count_archive_bugfixes.py`/`count_individual_fixes.py`/
  `extract_fix_evidence.py`, all kept per this audit.
