# Taoxichuan stage 8, M4 session pf — PLAN (slice 3 close: pipe park/wake, gates through the new path)

Session pe (`983dc1c5`, RESULT-pe.md / PLAN-pe.md) proved the NEW path's mechanism but
got no gate verdicts. This session closes slice 3: Fix B (pipe park/wake), then F1,
P1-P5, PK1 (+ PK2 = grandfork, if the image cost stays reasonable) in ONE clean boot
with `REAL_FORK` on, then strip the `[tmp]` prints and leave `REAL_FORK` on by default.

## 1. What is known (facts with evidence, kept apart from guesses)

From RESULT-pe.md ("Boots C-G", "Boots H-I close-out", "Hooks table"), PLAN-pe.md §§ 1/3/4,
and this session's source re-reads (file:line in tree `983dc1c5`):

- **F1-1** — The NEW path's full lifecycle is console-proven on three boots (G/H/I):
  fork → shell parks in `wait4_real` → child enters ring 3, runs, exits →
  `task_exit_real` wakes the waiter → `switch_to` lands back on the shell →
  `w4ret rax=0x5` (child's pid, perfect frame) → third reap rax=0. Zero faults.
- **F1-2** — The rip=0 class is closed: Fix A' (`SpawnSlot.real` bind bit, `ffb09617`)
  and Fix C (sched maps key on the live shell tid, `ecaca7b1`) landed; boots H/I ran
  the whole lifecycle with no rip=0.
- **F1-3** — Channel evidence (premise 3): `REAL_FORK` off ⇒ 8+ sequential spawns on
  one boot, no wedge (boot B, P1-P5 all green). `REAL_FORK` on ⇒ ~2 connections clean,
  then the channel stalls (`gw_rx=0`, `in_flight` grows, guest net thread starved,
  vCPU at tick rate only); an exec orphaned onto the dead channel makes the bridge
  writev-EAGAIN-loop (no backpressure — K9's socket cousin) and leaks the guest.
  pe's diagnosis: the sshd's own per-session event path rides the broken pipe arms.
- **F1-4** — The pipe arms still drop every `Wakes` and never park (premise 4, re-read
  this session): `pipes.rs` header ("Wakes are `()` and dropped on the floor — nothing
  on this kernel blocks"), `_wakes` at read_arm/write_arm/release/write_no_sigpipe;
  empty-pipe read answers 0, full-pipe write answers EAGAIN, `close_read`'s wakes
  dropped in `release`. PK1 is impossible through them; K9 still open.
- **F1-5** — The pulled crate owns the whole decision: `akuma-pipes`' one-step
  `check_set_reader`/`check_set_writer` (register-or-proceed; the anti-TOCTOU lesson),
  `Wakes::fire`, and poller draining on `write`/`read` success and on
  `close_read`'s last-reader close (lib.rs:277/295/355-400 host-tested).
- **F1-6** — The park pattern is proven in `wait4_real` (child.rs:1559-1561): save
  `ring3::user_rsp_stash()`, `schedule_blocking(u64::MAX)`, restore stash. The stub
  stash is a GLOBAL — the per-task save/restore is load-bearing.
- **F1-7** — The wake pattern is proven: `akuma_threading::get_waker_for_thread(tid).wake()`
  (`task_exit_real`, child.rs:569). Pipe poller tids can be the same tid space: the
  task slot (`exec_twin::current_slot()`), same as `waiter`.
- **F1-8** — Per-slot kernel bands landed in slice 2 (spec §3; pe boots): a task parked
  mid-syscall keeps its own stack, so parking inside read/write is safe the same way
  parking inside wait4 is.
- **F1-9** — Image injection is cheap (premise 5): every `boot_dev.sh` boot already runs
  `devroot-ensure` (host ext2 writer over the fresh image copy), which has an opt-in
  bake precedent (`AKUMA_DEVROOT_BAKE_SSHD` → `create_write("/bin/sshd", …)` +
  read-back assert, devroot_ensure.rs:74-81). An env-var clone is ~10 lines.
- **F1-10** — PK2 probe choice: **`userspace/forktest/c_stress/grandfork.c`**
  (musl-static; 5 fork-only rungs — control, gfork_nowait, gfork_wait, gfork_exec,
  gfork_echild; announces every step through write(2) BEFORE it runs; prints
  `grandfork: ALL PASS`, exits 0 / the failed rung). NOT forkprobe: its `many` rung
  forks 24 children > `NSLOTS=12` (child.rs:88) and cannot pass on this kernel; not
  pipewake.c (pthread reader — threads are out of scope); not killtree.c
  (signals/threads). grandfork's rungs 3/5 exercise child-side wait4 park + ECHILD —
  exactly the new path's semantics one level deeper than the shell.
- **F1-11** — Symptom matrix: `grep -i` for the stall/EAGAIN/writev shape returns no
  matching row (rows 52/99 are amd64 EPOLLET/socket rows — different mechanism;
  rows 98/111 are BKL/NIC). The K9 pipe shape has no matrix row on this box; the
  crate's `close_read` doc names it (`busybox yes | busybox head -n 1`).

## 2. Hypotheses (cheapest confirmation first; cost per bit, cheapest first)

| # | Hypothesis | Prediction | Test | Cost |
|---|---|---|---|---|
| H1 | The REAL_FORK-on channel stall rides the broken pipe arms (sshd's per-session pump EAGAIN-spins on a full pipe or misreads an empty-pipe 0 as EOF once fork returns concurrently) | Fix B makes the ssh channel serve 8+ execs per boot, like REAL_FORK off | The gate boot itself: 8 execs back-to-back | 1 build + 1 boot (planned anyway) |
| H2 | F1/P1-P5 pass through the new path once the arms park (pipeline members are real fork children; the arm logic is proven code otherwise) | Same boot: F1 bytes + P1-P5 PASS | Same boot | 0 extra |
| H3 | PK1 needs only park/wake + the close-wake firing (`yes` parks on full; `head`'s exit close wakes it; BrokenPipe then ends it; `wc` parks on empty, EOF at last close) | `20000\n`, exit 0 | Same boot | 0 extra |
| H4 | grandfork passes: all rungs are fork/_exit/wait4/write only; rung 3 parks a CHILD in wait4 while the shell is also parked — two parked tasks + wakes must compose | `grandfork: ALL PASS` exit 0 | Same boot (last exec) | 0 extra |
| H5 | If H1 is WRONG (channel still caps ~2): the slice can still close by re-ordering — new-path gates first (F1, PK1, grandfork = the verdicts), P-suite second boot if needed | 2-connection budget spent on the verdicts, not the regression | Contingency boot | ≤2 boots total contingency |

## 3. The fix sequence (order, stop conditions)

0. **Commit this plan + premise rows.** Host checks: `cargo test -p akuma-pipes
   --target $HOST` (the crate owns the check_set_* decision — must stay green,
   crate untouched); clippy on any crate touched; `kbuild -p taoxichuan -j 4
   --features no-tests` (premise 1). Stop: tests pass, build exits 0.
1. **Fix B — pipe park/wake** (`taoxichuan/src/pipes.rs`; every decision PULLED from
   `akuma-pipes`, the arms are glue written-new):
   - `park_once()` helper = the wait4 pattern (F1-6): stash save →
     `schedule_blocking(u64::MAX)` → stash restore.
   - `wake_one(tid)` = `get_waker_for_thread(tid).wake()` (F1-7); fire every `Wakes`
     each op returns, and STOP dropping them in `release()`/`release_open_fds()`.
   - `read_arm`: loop — `read()`; fire; bytes>0 ⇒ copy (short-read break kept);
     0 bytes ⇒ if not may-park (`NO_SLOT` boot task / out of range) return 0 as today;
     `check_set_reader(id, my_tid, ())` true ⇒ EOF/no-pipe ⇒ return 0; false ⇒ park, retry.
   - `write_arm`: loop — `write()`; fire; `Wrote(n)` advance; `Wrote(0)` ⇒ not
     may-park ⇒ EAGAIN; `check_set_writer` true (readers gone mid-check) ⇒ EPIPE;
     else park, retry. `BrokenPipe` ⇒ EPIPE, and **for a real task only** the
     documented SIGPIPE stand-in: `task_exit_real(13<<8)` (M2 path keeps EPIPE so
     premise 5/P5 is untouched).
   - `write_no_sigpipe` (exec pump hook) UNCHANGED: Ok(0) backpressure contract, the
     boot task drives it and must never park.
   Stop: clippy clean on taoxichuan (host target), akuma-pipes tests green, kbuild
   green, commit BEFORE the boot.
2. **PK2 plumbing** (before the boot so one boot carries everything): extend
   `devroot_ensure.rs` with `AKUMA_DEVROOT_BAKE_FORKPROBE` (the sshd-bake pattern →
   `/bin/grandfork`); build grandfork musl-static (x86_64-linux-musl-gcc per
   c_stress/README.md); build devroot-ensure musl. Stop: both binaries exist; host
   clippy on ext2probe green.
3. **Gate boot** — fresh boot via boot_dev.sh (default /root/ktarget artifact; check
   the install line's size/md5; fresh .orig copy; vm-serve-mc stopped, BOTH confs).
   ONE boot, exactly 8 execs: `run_fork.sh` (NEW, beside run_pipes.sh) = F1, PK1,
   grandfork; then `run_pipes.sh` = P1-P5. New-path verdicts FIRST so the
   2-connection budget (if H1 is wrong) is spent on them.
   Stop: F1 exit 0 + client bytes exactly `x\n2 /tmp/f\n` (od -c); PK1 exit 0 +
   `20000\n`; grandfork `ALL PASS` + exit 0; P1-P5 PASS; zero `Fault|PANIC|sig!`
   after this boot's start line (logmark first — the log rotates ~250 KB); ticks
   advancing. **No client retries on a stall** (pe's lesson: retries burn the
   forward ceiling and leak the guest).
   If a gate fails: `grep -n -i "<error text>" docs/README.md docs/runbooks/*.md`
   BEFORE reading source; write the matching row (or "no row") in RESULT-pf.md.
4. **Cleanup + final**: remove the 8 `[tmp]` prints (child.rs 1, exec_twin.rs 3,
   main.rs 4 — RESULT-pe "Remove before final"); `REAL_FORK` stays `true` (default
   on, M2 scaffold stays compiled and selectable); host clippy on touched crates;
   final `wc -l`; RESULT-pf.md complete; push `litter taoxichuan-s4`; ops restore
   (vm-dev stop, stage4 conf from s6 into BOTH confs, disable, vm-serve-mc start,
   mirror.sh stop, `herd status`). Stop: restored state verified before the last line.

Boot budget: 6 max. Boots planned: 1 (step 3) + ≤2 contingency (H5 re-order, fix
iterations). If a step yields no new evidence after 2 boots: say so here, re-plan
from what is known, continue (brief's rule).

## 4. What from the amd64 kernel's history applies

(Carried from PLAN-pe §4 — the same docs govern this session's one fix.)

- **SYSCALL_BLOCKING.md / WAIT_QUEUES.md** — the order wait-queue → waker → park,
  and `schedule_blocking`'s publish-WAITING-then-yield contract our arms reuse.
- **runbooks/debug-futex-lost-wakeup.md** — the lost-wakeup signature (waiter not
  queued vs wake never issued vs orphan wake) is exactly the `check_set_*` one-step
  question; if a gate hangs, walk that ladder before theorizing.
- **AKUMA_PIPES_EXTRACTION.md / PIPE_TABLE_UNIFICATION.md** — "wakes are returned,
  never performed" so the second kernel owns the firing; taoxichuan is that owner
  and today drops them — Fix B is just the owner doing its job.
- **AKUMA_AMD64_RING3_SEAM_SLICE2.md §3.1** — "wired but unreachable rots"; the gate
  boot IS the reachability test for park/wake on the new path.
- **CONTEXT_SWITCH_BUGS/FIX, AKUMA_THREADING_X86_SWITCH** — the switch contract the
  parked-task resume rides (callee-saved + stack + CR3 together; proven by F1-1).
- **docs/reference/subsystems/scheduler.md** § park/wake handshake + host tests —
  the invariant to re-check if a wake is lost; console.md § printing rules — any
  new debug print goes through `safe_print!` with a byte budget.

Composition rule accounting (say which for each piece): Fix B decisions = **pulled**
(`akuma-pipes`, already a path dependency — no crate change, no extraction); the arm
glue + stand-in = **written new** (composition root's job; amd64 has no twin of these
arms); grandfork = **pulled as-is** (existing userspace probe); devroot-ensure bake
env var = **written new** ~10 lines in an existing host tool. Nothing is extracted
from `amd64/src/` (nothing needed lives only there — park/wake are already
`akuma-threading` API). `amd64/` untouched ⇒ its check stays green; run once to
confirm. `wc -l taoxichuan/src/*.rs` at start (**7618**) and end.

## 5. Timer-driven switching (added after pf-3, per the owner's decision relayed by the supervisor)

pf-3's watchdog named the wedge: the shell (slot 2) spins in ring 3 at
rip=0x4ceaa1 — musl `__stdio_write`'s writev retry loop, identified by
disassembling the guest busybox (`/tmp/refbin/busybox`, the image's md5) — with
one thread READY and never picked (`rdy=1 run=1 wt=0`, 35+ watchdog samples) and
nobody WAITING. A kernel that only switches where a task parks cannot survive any
ring-3 loop that needs another task to run; the READY child, the pump/bridge and
the NIC all starve together. The owner's decision: plan and build timer-driven
switching NOW, SMP-ready; ring-0 preemption stays out.

**Pulled (dependencies already in the tree; zero copies):**
- `akuma_threading::x86_yield()` (lib.rs:3527, public, `-> bool`) — the SAME
  switch engine `schedule_blocking`'s x86 loop and `idle_main` already run. It
  saves the interrupted (handler) context into the FROM thread's record,
  `x86_pick_next` selects (READY/RUNNING, `ON_CPU` 0→1 CAS claim — the
  SMP_SHARED_ONCPU_GATE safety, two cores never resume one stack), fires the
  `switch_to` hook, and stack-switches. Nothing else is needed to preempt.
- The crate's thread states and `x86_runnable_exists()` (the idle loop already
  uses it — the idle self-yields, so ring-0 ticks need nothing new).
- `akuma_lapic_amd64::on_tick()` already EOIs internally (lib.rs:163) — so after
  it returns, the LAPIC can deliver the next tick to whichever task runs next
  (amd64's ordering: EOI **before** the preempting switch).
- `akuma_gdt::Tss::set_rsp0` — the per-switch trap-stack arming.

**Adapted from amd64 (shape, not code — amd64's `idt.rs::timer_trap` +
`sched::preempt_if_needed` are the reference):** the LAPIC vector body may switch
tasks "on the interrupted task's own trap stack"; from_ring3 (or idle) may be
switched away from, other kernel code is only asked (`need_resched`). taoxichuan
keeps the simpler half of that contract: IF is masked on every kernel path, so a
tick lands in ring 3 or in the idle window — only the ring-3 case switches.

**Written new (taoxichuan glue):**
1. **Per-slot ring-3 trap stacks.** Today `TSS.rsp0` names ONE global 16 KiB
   `TRAP_STACK` (main.rs:184): a preempting switch would abandon the interrupted
   frame there and the next ring-3 trap on any task would clobber it. New:
   `TRAP_STACKS: [[u8; 16 KiB]; NSLOTS+1]` (shell slots 0..NSLOTS; index
   NSLOTS = the idle), and `switch_to` arms `TSS.rsp0 = trap_stack_top(to)` per
   switch — the field amd64's X86ArchHooks doc names "the ring-3 trap stack in
   the TSS". `gdt_init` keeps arming the global stack first (pre-spawn traps).
2. **The tick switch.** `timer_trap`: after `on_tick()`, if `t.from_ring3()`,
   call `akuma_threading::x86_yield()`. If nothing else is READY it is a no-op
   (the crate's documented behavior — a lone thread makes progress). Ring 0 is
   never switched here: kernel paths run IF-masked, and the idle window's
   `x86_runnable_exists` + `x86_yield` lap already covers that case.
3. **The WRITEV pipe hole** (the pf-3 loop's feeder): the WRITEV arm's kind match
   has no `OpenFile::Pipe` case — the WRITE arm has had one since 6879eb47 — so a
   bound pipe fd's writev fell through to the nested-child/console fallback and
   never reached the fix-B pipe arms. New: `OpenFile::Pipe(id, true) =>
   pipes::write_arm_gathered(id, &data)` (write_arm's park/EAGAIN/SIGPIPE semantics
   over the already-gathered buffer — the shell, not being a REAL task, gets the
   M3 EAGAIN and musl sets F_ERR and progresses instead of looping).

**Stop condition:** clippy+kbuild green → commit → boot → F1 completes the FULL
lifecycle (wc forked, entered, run, exited, reaped; `[SSH] Exit status: 0`;
client bytes `x\n2 /tmp/f\n` via od -c; wd lines show slots changing across ticks
= preemptions happening; zero `Fault|PANIC|sig!` since the boot mark) → then
P1-P5 + PK1 + PK2 in the same boot (8 execs cap) → strip `[tmp]` prints, final
commit, ops restore.

**Risks named in advance:** (a) the M2 vfork-inline path runs inside a syscall —
IF-masked, never preempted, unchanged; (b) the boot task never parks and is
never preempted (kernel path); (c) `x86_yield` from the trap switches even when
the FROM task is the only user task but the boot task is READY — desired (the
pump/bridge/NIC get the core); (d) the wd print stays in timer_trap (rate-limited,
[tmp]) so the boot's own console shows the switches.

## Session-notes log (appended, never rewritten)