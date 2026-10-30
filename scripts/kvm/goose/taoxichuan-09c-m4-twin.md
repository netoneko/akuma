# Taoxichuan stage 8, M4 session pc: slices 1-2 - register the `akuma-threading`/`akuma-exec` runtime twin, then move the crossing statics into per-slot state (NO task switch yet)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: `RESULT-pb.md`, origin `c37cdb79` (the box's litter head `c3a37692` plus a clippy fix: `git fetch litter` is
not needed, but `git log --oneline -3` first; if your tree lacks `NoApicBase` in `crates/akuma-lapic-amd64/src/lib.rs`, port that small fix: `Result<TimerInfo, NoApicBase>`, `#[must_use]` on `on_tick` and `pit_wait_us`).
**Keep your context small.** Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`.
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Build: `kbuild -p taoxichuan -j 4 --features no-tests` (the `-p` matters). Box log times are UTC.

## The key fact
Session pb gave the kernel a tick and linked `akuma-exec`/`akuma-threading`; nothing registers them or switches tasks. **The plan is `docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md`: read §§ 1, 3 and 5 (slices 1 and 2) in full - it is your spec.**
User's decisions stand: the kernel gets PREEMPTION (a later slice, 4) and the design is SMP-READY (slice 5): no `static` "current task" anywhere new - go through `current_slot()`/`set_current_slot()` stubs shaped as `fn()`.
This session = plan slice 1 (twin runtime: register `ThreadRuntime`, `ThreadConfig`, `ProcessHooks`, `X86ArchHooks`, `ExecRuntime` per the § 1 tables; `uptime_us` = ticks x 10 ms; pipe hooks real - M3's `pipes.rs` table exists) and slice 2
(per-slot kernel+trap stacks via `prepare_task_slot`, `write/read_user_context`; the crossing statics `RET_RSP`/`SAVE_*`/`SPAWN_OUT_SLOT`/`PARENT_*` and `NESTED_SLOT`/`NESTED_DEPTH` in `taoxichuan/src/child.rs` become per-slot state looked up through `current_slot()`).
**Behaviour must not change: the M2 blocking fork and M3 pipes still ship underneath.** `fork_process`, `wait4` over channels, and any switch are slice 3 (next session). The T-Y bug 3 lesson (stack-band arithmetic, `SYSCALL_STACK` 64 KiB, depth-2 exec band) is
in the agent log "Stage 8, sessions T-Y" and plan § 3: re-derive the band arithmetic on paper before moving a stack.

## Read first (in order; `sed -n` the named parts)
1. Plan §§ 1, 3, 5 (above). 2. `amd64/src/exec_runtime.rs` (the registration twin; 642 lines; read `runtime()` at ~189 and each hook's body only for the fields marked "real" in plan § 1).
3. `amd64/src/sched.rs` ~800-850 (hook registration, `X86ArchHooks` at 834-846) and `Machine` (the per-slot side table). 4. `taoxichuan/src/child.rs` lines 60-110 (the statics) and the `fork_arm`/`exec_replace`/`child_exit` users of them.
**Probes**: the M4 gate is `userspace/forkprobe` (slice 3); this session's gate is behavioural equivalence, so reuse `scripts/kvm/goose/run_pipes.sh` (P1-P5) and the F1 spot check. Do not write a new probe.

## Premises (re-run each check before your first edit; one line per row in RESULT-pc.md: HOLDS / WRONG (what you saw) / UNCHECKED)
| # | premise | read-only check | supervisor saw |
|---|---|---|---|
| 1 | origin `c37cdb79` builds and passes: P1-P5, F1, ticks advance | `git log --oneline -3`; `kbuild -p taoxichuan -j 4 --features no-tests` | supervisor ran Mac clippy + amd64 `no-tests` check only after the clippy fix; the x86 boot predates it - UNCHECKED on this box |
| 2 | `ThreadRuntime` has 6 fn fields, 4 of them IPI/IRQ-bound (pb's correction) | `sed -n 150,162p crates/akuma-threading/src/lib.rs` | pb: 4, not 2 |
| 3 | `akuma-exec` registers via a `runtime()` returning `ExecRuntime` (amd64: `exec_runtime.rs:189`) | `grep -n "fn runtime\|pub fn init" amd64/src/exec_runtime.rs crates/akuma-exec/src/runtime.rs` | UNCHECKED |
| 4 | the crossing statics are exactly those in child.rs:72-94 and no other file reads them | `grep -n "RET_RSP\|SAVE_R\|SPAWN_OUT_SLOT\|PARENT_\|NESTED_" taoxichuan/src/*.rs \| cut -c1-100` | UNCHECKED - the list in plan § 3 is from pb |
| 5 | one core, so a per-slot table indexed by `current_slot()` returning a fixed slot is behaviour-neutral | read how `fork_arm` picks its slot (child.rs ~720) | pa: forks reuse a dead slot, shell is slot 1 at depth 1 |
| 6 | `akuma-slot-table` is what `akuma-exec`/`-threading` index by | `grep -n "slot" crates/akuma-slot-table/src/lib.rs \| head -20` | UNCHECKED (pb did not read it) |
If a WRONG row changes the plan, write it to RESULT-pc.md and commit at once.

## Symptom first
When a gate fails, `grep -n -i "<error text>" docs/README.md docs/runbooks/*.md` before reading source; write the matching rows (or "no row") in RESULT-pc.md. Boot-1 lessons from pa/pb: an init that allocates must run after PMM/heap registration; musl uses the raw `SYS_pipe` (22), not only `pipe2`.

## Guest facts (verified; do not rediscover)
One core. No procfs and no `/dev` (no `dd`). Fork exists only inside the spawned shell (depth 1); depth-2 fork returns EPERM. `PACE=15` after every exec. The host forward degrades near ~30 connections per boot; `run_pipes.sh` uses 5.
Ticks: PIT-calibrated LAPIC, vector 32, about 10 ms; a tick returns to the same instruction (no switch).

## Composition rule (mandatory)
Look first (`grep -rn <name> crates/ amd64/src/`); pull by path dependency; extract from `amd64/src/` into a crate only if it lives only there (rule 3: new crates `forbid(unsafe_code)`, unsafe budget 0, `scripts/check_unsafe_budget.py --update` for unavoidable sites); write new only when it exists nowhere.
The twin's registration glue is taoxichuan's own (written new, say so); `amd64/` must keep building (`cargo check -p akuma-amd64 --target x86_64-unknown-none --features no-tests`). Report per hook: real / no-op / stub, and `wc -l taoxichuan/src/*.rs` start (6,275?) and end.
Side task, cheap: `crates/akuma-lapic-amd64` has NO host tests (pb moved the code; clippy `-D warnings` on the host target is what the pre-commit hook runs). Add host tests for the pure parts only (register offsets, `APIC_BASE_MASK` masking, `NoApicBase`), nothing that needs `MmioWindow` hardware.

## Gates
- S1: with the twin registered, `run_pipes.sh` P1-P5 pass and the F1 spot check is exact (`x\n2 /tmp/f\n`), ticks still advance during a ring-3 exec (`[TICK pre]`/`[TICK post]` lines, as pb's `LOOP` gate), zero new `Fault|PANIC|sig!` lines after your boot's start line (the shared log keeps older boots' lines: compare line numbers).
- S2: the same gates after the crossing statics moved; plus `grep -n "static mut" taoxichuan/src/child.rs` before/after in RESULT-pc.md (the count of "who am I" statics must drop; list what remains and why).
- S3: `cargo clippy -p taoxichuan -p akuma-lapic-amd64 --target x86_64-unknown-none` clean; host `cargo clippy -p akuma-lapic-amd64 --target $(rustc -vV | grep '^host:' | cut -d' ' -f2) -- -D warnings` clean; `cargo test -p akuma-lapic-amd64 --target <host>`.
- One boot per slice (2 boots, a 3rd only for a fix). Read client captures with `od -c`; verdicts are console `[SSH] Exit status`.
- Out of scope: any task switch, `fork_process`, `wait4` over channels, signals, threads, SMP, a timer-driven reschedule, changing the pipes.

## Ops traps
- Guest kernel installs only with the explicit destination `/boot/akuma-guest-dev`; never write `/boot/akuma-amd64*`.
- `vm-serve-mc` is running and kvm0 is exclusive: stop it before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs; confirm the `guest disk:` line in `/var/log/herd/vm-dev.log`; fresh `.orig` image copy per boot; **restore everything last** (vm-dev disabled, `vm-serve-mc` running).
- Never run python from stdin on this box; write a file and run `python3 /path/x.py < /dev/null`. Do not copy keys or edit ssh/sshd config.
- Debug prints carry `[tmp]` and are listed under "Remove before final" in RESULT-pc.md. The pre-commit hook runs clippy `-D warnings` on every crate for the HOST target: run the host clippy on any crate you touch before committing.

## Commit rule (you have no clock)
Commit before every build; write RESULT-pc.md after every build, boot and commit. `git push litter taoxichuan-s4` only if green (host tests pass, kernel builds, no `[tmp]`, no amends of pushed commits). Never push `origin`.

## Budget
About 70 tool calls, 2 boots (a third only for a fix). At 40 calls without a commit the supervisor nudges you. Past budget without S1: write what you have and stop.

## When to stop
S1-S3 done, or 3 boots with no new evidence, or a quota error. Write `/root/dev-cycle/taoxichuan-s7/RESULT-pc.md` (premise rows, hooks table real/no-op/stub, S1-S3 lines with console evidence, the `static mut` before/after, pulled/extracted/written-new, wc -l, what is next) and end the turn.
