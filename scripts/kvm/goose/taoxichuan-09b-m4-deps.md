# Taoxichuan stage 8, M4 session pb: pull `akuma-exec`/`akuma-threading`, add a timer tick + preemption foundation, plan SMP-ready sched-lite

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: `RESULT-pa.md`, commit `942f4d5f`
on origin (M3 pipes: `run_pipes.sh` P1-P5 pass). **Keep your context small.** Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`
(first `git fetch litter` is NOT needed; your branch has pa's work; origin's `942f4d5f` holds the same `taoxichuan/` files).
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC. Build with `kbuild -p taoxichuan -j 4 --features no-tests`
(the missing `-p` cost session pa two boots).

## The key fact
M2's blocking fork and M3's pipes are throwaway-shaped: a fork child runs to completion inside `fork`, so a pipeline whose writer outlives one read, or `git clone`'s helper pump, can never work.
The real fix is plan M4 (`docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md` §§ 4-6): adopt `akuma-exec` (`fork_process`, `wait4` over child channels) on `akuma-threading`.
**Owner's decisions: the kernel gets PREEMPTION (no cooperative-only design: nothing in the git/cargo ladder works without it), and the design must be SMP-READY, with SMP itself a follow-on slice** (the tree has
`akuma-bkl`, `akuma-slot-table`; the amd64 per-cpu and AP bring-up code is `amd64/src/smp.rs`, NOT a crate (extract per rule 3 when the SMP slice comes); survey `ls crates/` before writing anything). So: no `static` current-slot, per-core state behind the hooks `akuma-threading` already asks for (`X86ArchHooks::current_slot`, `idle_slot`, `can_run`), BKL via `akuma-bkl`.
Booting extra cores is NOT this session; the plan document must say what it needs (AP startup path, per-cpu `%gs`, IPIs) and which crates already provide it.
`taoxichuan` has no timer today (`grep -n -i "lapic\|timer\|preempt" taoxichuan/src/*.rs` finds nothing relevant). amd64's tick is `amd64/src/lapic.rs` (1,121 lines, NOT a crate: `init`, `start_timer`,
`eoi`, `ticks`, `on_tick`, `calibrate`, TSC helpers) and `amd64/src/sched.rs::preempt_if_needed(from_user)` switches only ring 3 and the idle loop (a ring-0 thread preempted inside the allocator leaves its spinlock held: read the
module header; do not preempt ring 0). This session: (a) link `akuma-exec` + `akuma-threading` (plan U7 size); (b) **a tick source for taoxichuan**: per composition rule 3, extract the timer subset of
`amd64/src/lapic.rs` into a crate (`akuma-lapic-amd64`, `forbid(unsafe_code)` where it can; `scripts/check_unsafe_budget.py --update` for unavoidable `unsafe`; a new crate starts at budget 0), make `amd64/` consume it too (one implementation, two consumers; amd64 must keep building with `--features no-tests`),
and wire it into taoxichuan's IDT with `akuma-idt-amd64`/`akuma-trap-amd64` (`Kind` arm + `register` line; never `&mut *` a stub pointer). The KVM guest's LAPIC model is `akuma-svm-amd64`'s `lapic` (xAPIC page + timer) and the 8254 `pit`; (c) a tick counter visible on the console;
(d) the plan document for the rest of M4 (the twin of `exec_runtime.rs`/`sched.rs` registrations, SMP-ready). **Existing behaviour must not change**: a tick that fires while ring 3 runs returns to the same instruction (no switch yet: the switch lands with `fork_process` next session).

## Read first (in order; `sed -n` the named parts, never whole files)
1. `docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md` § 2 table, § 3, § 6 (U1, U4, U7) - what is claimed and what it left open.
2. `amd64/src/sched.rs` lines 1-75 (the seam) and the registration function (`grep -n "set_.*hooks\|register\|X86ArchHooks" amd64/src/sched.rs`) - the list of things a kernel registers.
3. `crates/akuma-threading/src/lib.rs`: `ThreadRuntime` (~153), `ThreadConfig` (~163), `ProcessHooks` (~184), `X86ArchHooks` (~2942) - the field lists; for each field write one line: needs a timer/LAPIC? yes/no.
4. `amd64/src/exec_runtime.rs` - what `akuma-exec` needs registered ("the file taoxichuan must write a twin of"); note which hooks are no-ops for a one-core, no-SMP kernel.
5. `taoxichuan/src/child.rs` - `fork_arm`, `exec_replace`, `child_exit`: what the current scaffold does that `fork_process` would replace; the **crossing statics** and `SYSCALL_STACK` layout (agent log "Stage 8, sessions T-Y", bugs 2-3: the stack-band arithmetic that bit us).
**Probes already exist**: `userspace/forktest/c_stress/` (`grandfork.c`, `spawnprobe.c`, `bssfork.c`, `killtree.c`), `userspace/forkprobe` (libakuma fork/cow/sockfd/many), `userspace/ncaprobe`. Name which one would be the M4 gate and whether it builds for x86_64 static (this is the "probe discovery" deliverable; do not build or run them this session).

## Premises (re-run each check before your first edit; one line per row in RESULT-pb.md: HOLDS / WRONG (what you saw) / UNCHECKED)
| # | premise | read-only check | supervisor saw |
|---|---|---|---|
| 1 | both crates resolve and check for the kernel target | `cargo check -p akuma-threading -p akuma-exec --target x86_64-unknown-none` | Finished, no errors (Mac, 2026-10-28) |
| 2 | taoxichuan has no timer/preemption code | `grep -n -i "lapic\|apic\|preempt\|ticks\|hlt" taoxichuan/src/*.rs \| head` | only unrelated words ("listing", "stickiness"); a real read is yours |
| 3 | `ThreadRuntime` has 6 fn fields, 2 of which (`trigger_sgi`, `end_of_interrupt`) look IPI/IRQ-bound | `sed -n 150,162p crates/akuma-threading/src/lib.rs` | confirmed 6 fields; whether they are called on the cooperative path is UNCHECKED |
| 4 | `amd64/src/exec_runtime.rs` is 642 lines and registers `ExecRuntime` hooks `alloc_process_info`, `fork_share_memory` among others | `grep -n "fn \|pub fn" amd64/src/exec_runtime.rs \| head -60` | UNCHECKED |
| 5 | taoxichuan's current image size | `kbuild -p taoxichuan -j 4 --features no-tests` then `ls -l` the ELF and the flattened image if one exists | UNCHECKED - this is the U7 baseline you record BEFORE adding deps |
| 6 | the slot/pid model `fork_process` assumes (`akuma-slot-table`, `Process`) can coexist with taoxichuan's `SpawnSlot` ring until the swap | `grep -n "SpawnSlot\|NSLOTS" taoxichuan/src/*.rs \| head -20` and `grep -n "pub struct Process" -r crates/akuma-exec/src \| head -3` | UNCHECKED |
If a WRONG row changes the plan, write it to RESULT-pb.md and commit at once.

## Symptom first
Not applicable until something fails. If the build fails with a link or `alloc` error, `grep -n -i "<error text>" docs/README.md docs/runbooks/*.md` before reading source, and write the matching rows (or "no row") in RESULT-pb.md.

## Guest facts (verified; do not rediscover)
One core. No procfs. Fork exists only inside the spawned shell at depth 1; the M2/M3 scaffold is what ships today and **must keep passing**. The host forward degrades near ~30 ssh connections per boot.
Static and static-PIE binaries run. `PACE=15` s after every exec is deliberate.

## Composition rule (mandatory)
Look first (`grep -rn <name> crates/ amd64/src/`), pull by path dependency, extract from `amd64/src/` into a crate only if it lives only there, write new only when it exists nowhere. This session writes
**no new kernel logic**: only `taoxichuan/Cargo.toml` dependency lines, at most a `use`/`extern crate` line to prove linkage, and the plan document. Report per dependency: pulled / extracted / written new. `wc -l taoxichuan/src/*.rs` at start and end (start: 6,123).

## Gates
- PB1: `cargo check -p taoxichuan --target x86_64-unknown-none` and the box's `kbuild -p taoxichuan -j 4 --features no-tests` both succeed with `akuma-exec` and `akuma-threading` in `taoxichuan/Cargo.toml` and the link actually referencing one symbol of each (otherwise the linker drops them and the size means nothing).
- PB2: image size before and after, in bytes, with the U7 question answered ("fits the microvm image budget": the Firecracker/KVM guest loads the kernel from `/boot/akuma-guest-dev`; state the size, do not guess a limit - read `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md` for any stated budget and quote it).
- PB3: with the deps linked, `run_pipes.sh` P1 and the F1 spot check (`sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` -> exit 0, client bytes `x\n2 /tmp/f\n`) still pass: the pull changed nothing. One boot, via the `taoxichuan-08q-curl-https.md` boot recipe and `scripts/kvm/goose/boot_dev.sh`; zero `Fault|PANIC|sig!`.
- PB4 (the document): write `docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md` with (1) a table of every `ThreadRuntime`/`ThreadConfig`/`ProcessHooks`/`X86ArchHooks`/`ExecRuntime` field: what amd64 registers, what the cooperative taoxichuan twin registers (value or no-op or "needs a tick"), line numbers; (2) the exact list of syscalls that would block and where the switch happens;
  (3) how a blocked task's kernel stack and the `SYSCALL_STACK` / crossing statics interact (the T-Y bug 3 lesson: stack-band arithmetic); (4) the SMP list: what booting the other cores needs and which crate provides each piece; (5) the proposed M4 slice list with boot counts. Mark every claim you did not read in source as UNCHECKED.
- PB5: a tick count that advances during a long ring-3 exec (e.g. `sh -c 'dd if=/dev/zero of=/dev/null bs=1 count=200000'`; console shows the count before and after), with P1 and F1 still green.
- Out of scope: switching tasks on a tick, `fork_process` wiring, booting extra cores, signals, threads, writing the twin.

## Ops traps
- Guest kernel installs only with the explicit destination `/boot/akuma-guest-dev`; never write `/boot/akuma-amd64*`.
- `vm-serve-mc` is running and kvm0 is exclusive: stop it before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs; confirm the `guest disk:` line in `/var/log/herd/vm-dev.log`; fresh `.orig` image copy per boot; **restore everything last** (vm-dev disabled, `vm-serve-mc` running).
- Never run python from stdin on this box (`python3 -`, heredoc python); write a file and run `python3 /path/x.py < /dev/null`. `gcc` only after `. /etc/akuma-dev.env`.
- Do not copy keys or edit sshd/ssh config on the box (session pa did, unasked); if a capture lacks output, report it.

## Commit rule (you have no clock)
Commit before every build. `git push litter taoxichuan-s4` only if green (host tests for any touched crate pass, the kernel builds, no `[tmp]` prints, no amends of pushed commits). Never push `origin`.
Write RESULT-pb.md after every build, boot and commit.

## Budget
About 60 tool calls, 3 boots (it is a bigger session than first drafted: the extraction is the cost). At 40 calls without a commit the supervisor nudges you. Past the budget without PB1: write what you have and stop.

## When to stop
PB1-PB4 done, or the build proves the dependency cannot link (then PB4 documents why, with the error), or a quota error. Write `/root/dev-cycle/taoxichuan-s7/RESULT-pb.md` (premise results, probe hit list, PB1-PB4 lines with evidence, pulled/extracted/written-new, wc -l, what is next) and end the turn.
