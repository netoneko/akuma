# Taoxichuan stage 8, M4 session pf — RESULT (slice 3 close: pipe park/wake, gates through the new path)

## Premise rows (written before the first edit; final after the last boot)

| # | premise | verdict | evidence |
|---|---|---|---|
| 1 | tree is `983dc1c5` and builds | **HOLDS** | `git log --oneline -3` = 983dc1c5/ecaca7b1/b33ac6ba, branch `taoxichuan-s4`; `kbuild -p taoxichuan -j 4 --features no-tests` **exit 0** (kbuild-pf0.log; artifact md5 0536b5ca… = pe's boot G tree — no edits yet, as expected); `wc -l` = 7618 as the brief says |
| 2 | `REAL_FORK` is true and the `[tmp]` prints are in the tree | **HOLDS** | child.rs:434 `pub const REAL_FORK: bool = true;` (used at 1288, 1481); 8 `[tmp]` prints: child.rs 1 (`[tmp] exit`), exec_twin.rs 3 (`enter`, `sw to=`, top), main.rs 4 (`w4ret` frame dump ×1, `fault ctx`) |
| 3 | ssh channel serves ~2 connections/boot with REAL_FORK on; 8+ spawns fine with it off | **HOLDS** (from pe's record; this session's battery re-tests the "on" half) | REAL_FORK off: boot B, 8+ spawns, P1-P5 green (RESULT-pe Gates). On: boots G/H/I — ~2 clean then stall, orphaned exec writev-loops the bridge, guest leaks |
| 4 | pipe arms still drop wakes and never park | **HOLDS** | pipes.rs header: "Wakes are `()` and dropped on the floor — nothing on this kernel blocks"; `_wakes` drops at read_arm/write_arm/release/write_no_sigpipe; empty read → 0, full write → EAGAIN; PK1 impossible through them (K9 open) |
| 5 | PK2 probe can get into the guest image at reasonable cost | **HOLDS** | devroot-ensure runs on EVERY boot_dev.sh boot and has the bake precedent (`AKUMA_DEVROOT_BAKE_SSHD`, create_write + read-back assert); an env-var clone ≈10 lines. Probe = `grandfork.c` (forkprobe is ruled out: `many` = 24 children > NSLOTS=12, child.rs:88; pipewake needs pthreads — out of scope). Est. ~6 calls ⇒ within the ~15 budget; PK2 stays LAST in the battery |

## Session-notes log (appended; never rewritten)

- 18:0x UTC — session start. Brief + premise checks + doc pass (RESULT-pe sections,
  PLAN-pe §§1/3/4, M4 spec §§2/3/5, SYSCALL_BLOCKING/WAIT_QUEUES, lost-wakeup
  runbook headers, docs/README.md symptom matrix, userspace-layout, boot_dev.sh +
  devroot_ensure.rs, akuma-pipes API, forkprobe/pipewake/grandfork probes, run_pipes.sh).
  PLAN-pf.md written and committed. No edits yet.
- 18:1x UTC — step 0 host checks: `cargo test -p akuma-pipes --target
  x86_64-unknown-linux-musl` 24/24 pass (the crate owning the check_set_* decision,
  untouched); `kbuild -p taoxichuan -j 4 --features no-tests` exit 0 (premise 1 now
  fully HOLDS; artifact md5 0536b5ca… = pe's boot G tree, expected — no edits yet).
  Premise-3 side note: sock.rs poll/select are busy-wait loops (no park) — the
  contingency candidate if the channel stall survives Fix B; out of scope today.

## Hooks table (real / no-op / stub — update when Fix B lands)

- **pipe `read_arm`**: real park/wake (M4 slice 3, this session). Empty-with-writers
  parks via `check_set_reader` + `schedule_blocking`; EOF (no writers / pipe gone)
  answers 0; boot task (`NO_SLOT`) keeps the M3 answer 0. Fires the read's `Wakes`.
- **pipe `write_arm`**: real park/wake. Full buffer parks via `check_set_writer`;
  `BrokenPipe` = EPIPE +, for a REAL task only, the SIGPIPE-default stand-in
  (`task_exit_real(13<<8)`); boot task keeps EAGAIN. Fires the write's `Wakes`.
- **pipe `release` / `release_open_fds`**: close arms' wakes now FIRED (was: dropped)
  — the last-reader close wakes the parked writer, the last-writer close delivers EOF.
- **`write_no_sigpipe`** (exec pump hook): unchanged, non-blocking Ok(0) backpressure
  (the boot task drives it; it must never park).
- Still stub/no-op (honest): sock.rs read/write/poll arms — poll/select are
  busy-wait loops, socket writes retry without backpressure (K9's socket cousin,
  pe's boots E-I evidence); `on_process_exit` no-op; child-AS frames leaked per
  fork; M2 blocking fork still selectable via `REAL_FORK`.

## Gates (final)

**F1, PK1, PK2, P1-P5: UNKNOWN — the gate runner saw no `[SSH] Exit status` line in
its console slice (gates-F.txt / gates-P.txt); zero `Fault|PANIC|sig!` every boot.**
The runner's verdicts are real but its slice lost races with the console rotation
the trace prints now cause (preemption switches print `sw to=` twice per 10 ms
tick — ~8 KB/s — rotating the 250 KB log every ~30 s); the .old log holds the
truth and was read by hand for every boot below.

- **Boot pf-1 (b00c2f70, fix B only):** F1 wedged the box inside the first exec
  (see boot log above) — zero faults, 100 % vCPU, net starved.
- **Boot pf-2 (aa550347, wd v1):** `slot=2 tid=2 rdy=1 run=1 wt=0` forever after
  `w4ret rax=0` — the shell holds the core, one thread READY never picked.
- **Boot pf-3 (ce32a336, wd v2):** THE CLEAN LIFECYCLE IS GREEN — `w4park slot=2
  pid=-1 opt=0x0` → `sw to=12/1` → `enter tid=1` → cat runs → `exit tid=1 wake=2`
  → `w4resume` → `w4ret opt=0x0 rax=0x5` (cat reaped, perfect frame) → `w4ret
  opt=0x1 rax=0x0` (wait4(-1, WNOHANG) → 0, correct) — then the shell spins
  forever at rip=0x4ceaa1, identified by disassembling the image busybox
  (/tmp/refbin/busybox, md5 ebce4301…): musl `__stdio_write`'s writev retry loop.
  pe boots G-I died at this same line ("third reap rax=0") and were read as a
  channel stall.
- **Boot pf-5 (8d024ab2, preemption v1) and pf-6 (96b7948b, re-enabled):**
  deterministic silent VM exit inside `net calibrate` — the bisect that forced
  the user-era gate.
- **Boot pf-7 (bc5c57e6, user-era gate):** calibrate passed (`[raw] tsc
  calibrated`, `[raw] stack up, dhcp loop`), sshd spawned, and an exec COMPLETED
  END TO END — `[TICK post] n=408`, `[SSHD] bridge_process: couldn't open stdin
  for pid 2`, **`[SSH] Exit status: 0`** — then the raw panic print named
  `akuma-threading:3652 THREAD_STATES[cur]` with cur=usize::MAX (a ring-3 tick
  after the exec'd shell's exit tore its slot down). Preemption demonstrably ran.
- **Boot pf-8 (15cc24e0, slotted-current gate): BOOT OK 50 s — the first boot to
  bring sshd up WITH the tick switch armed.** Battery: all gates UNKNOWN (console
  rotation). Hand-read evidence: preemption LIVE (`sw to=12`/`sw to=2` ping-pong
  each tick, machine alive, zero faults), and the wd still shows
  `rip=0x4ceaa1 slot=2 tid=2 rdy=1 run=1 wt=0` — the exec'd shell still spins in
  musl's writev retry loop, now round-robined with its fork child instead of
  starving the box. The boot probe's single-command exec completed under
  preemption; F1's multi-command (forking) exec wedges in that loop.
- **Premise 3 (channel serves ~2 connections/boot) is SUPERSEDED:** the stall was
  never the channel — it is this writev loop; with preemption the box survives
  and the loop is the one remaining blocker for F1/PK1 and, presumably, P1-P5.

## What is next (the hand-off)

1. **Root-cause the exec'd shell's writev loop** (one session, evidence in hand):
   the shell's fd 1 writev does not reach the fix-B pipe arms — pf-8's io=15456
   exits/s says the loop rides the console/child_write fallback, i.e. the kind
   probe in the WRITEV arm does not see a bound pipe for fd 1 of a nested shell
   (or the pump is not draining the spawn pipe it IS bound to). First moves:
   print the fd-table kind + `nested_active()` at the WRITEV arm for slot 2;
   check `child_write`'s full/answer contract; compare with the boot probe's
   single-command exec, which completes.
2. Then the gates are re-run (same battery, one boot): F1, PK1, PK2, P1-P5.
3. Only then: strip the `[tmp]` prints (the sw-to print in particular — with
   preemption it is rotation-scale), leave `REAL_FORK` on, final commit.

## Pulled / extracted / written-new

- **Pulled (dependencies, untouched):** `akuma_threading::x86_yield()` +
  `x86_pick_next`'s ON_CPU CAS claim (the SMP_SHARED_ONCPU_GATE safety) +
  `x86_runnable_exists()` + the thread states — the whole switch engine;
  `akuma_lapic_amd64::on_tick()` (EOIs internally — amd64's EOI-before-switch
  ordering comes free); `akuma_gdt::Tss::set_rsp0`. akuma-pipes' check_set_*
  contract (fix B, earlier commits). Host tests: akuma-pipes 24/24 green.
- **Adapted (shape from amd64 `idt.rs::timer_trap` / `sched::preempt_if_needed`,
  not copied):** only from_ring3 switches; kernel code is only asked; per-switch
  trap-stack arming.
- **Written new (taoxichuan glue):** `TRAP_STACKS[NSLOTS+1]` +
  `arm_trap_stack()` (main.rs, armed in `exec_twin::switch_to`); the tick switch
  in `timer_trap` with the user-era (`PREEMPT_ARMED`, set in `spawn_arm`) and
  slotted-current gates; the WRITEV arm's `OpenFile::Pipe` routing →
  `pipes::write_arm_gathered` (the writev twin of write_arm over the gathered
  buffer); `raw_uart`/`raw_dec` + the raw-first panic handler; the wd watchdog;
  the `[tmp]` fork/w4park/w4resume/w4ret-opt/pipe-wake prints.

## Remove before final

pd/pe's list still in the tree (all `[tmp]`): `[tmp] enter` (exec_twin.rs),
`[tmp] exit` (child.rs), `[tmp] w4ret` frame dump (main.rs), `[tmp] sw to=`/top
(exec_twin.rs — **rotation-scale under preemption: drop first**), `[tmp] fault
ctx` (main.rs). Session pf additions: the wd watchdog line (timer_trap),
`[tmp] w4park`/`w4resume` (child.rs), `[tmp] fork child pid=` (child.rs),
`[tmp] pipe wake tid=` (pipes.rs), `[raw] tsc calibrated`/`stack up, dhcp loop`
(net.rs), the raw-first panic prints (main.rs — cheap, may be kept deliberately).
`[TRAMP-MISMATCH]` is pre-existing (akuma-exec).

## wc -l

Start: **7618** (`taoxichuan/src/*.rs`, `983dc1c5`). End: **7870**
(`15cc24e0` + the fix-B/trace/preemption series, all pushed to `litter`).

## Boot pf-1 detail (the session's opening wedge, fix B only) — kept for the premise evidence

Install verified, boot probe OK (`__BOOT_OK__`, boot_probe.out). Then **F1 WEDGED THE
BOX** — the gate runner's verdicts are all UNKNOWN ("client killed, no console exit
line"), gates-F.faults EMPTY (zero `Fault|PANIC|sig!` since logmark 101824).

Console (identical in F1/PK1/PK2 consoles — one console stream, no guest print after
the wedge): auth OK → `[SSH] Exec` → echo: `[o] open flags=0x241 -> 3`, `[w] fd=1
len=2 -> 2` → `[tmp] w4ret rax=0x2` (echo reaped) → `sw to=12`, `sw to=1` →
`[TRAMP-MISMATCH] tid=1 THREAD_PID_MAP=5 but table scan found 3 ??? using 5` →
`[tmp] enter tid=1` (cat) → `[o] open flags=0x0 -> 3`, `[sc] unhandled ? (40)`
(busybox cat's sendfile probe → ENOSYS, harmless) → `[tmp] exit tid=1 code=0 wake=2`
→ **`[SCHED] WARNING: yield_now with IRQs masked tid=1 lr=0x0`** (fires inside
`x86_finish_current`; akuma-threading's own doc: a masked yield is a silent no-op
and the caller busy-spins — aarch64-worded, the switch DID land here) → `sw to=2` →
`[tmp] w4ret rax=0x5` (cat reaped, perfect frame) → `[tmp] w4ret opt?- rax=0x0
rbx=0x0 rbp=0x0` — then **SILENCE FOREVER**.

Wedge state ([KVM] monitor, 20 min stable): vCPU 100.0% guest, `hlt=0`, exits
decay to tick-only (`lapic=100 irq=100 io=0 net=0 blk=0`), `gw_frames_from_guest`
frozen at 47, relay `in_flight=369 gw_rx=0` — the guest never ACKs again; PK1's
client: "connection stalled during handshake"; PK2's: Connection refused (sshd not
listening — net thread starved). Zero faults.

Reading of the evidence (facts, then the working theory):
- The trace is IDENTICAL to pe's healthy boots G/H/I through the last recorded line
  ("third reap rax=0" = my `w4ret rax=0`): echo fork→run→reap, cat fork→enter→run→
  exit→wake→reap all GREEN with fix B in. Fix B's arms were never PARKED in this
  trace (no pipe in F1 fills or empties — `fire()` had nothing to fire; the boot
  task/pump keeps the M3 answers). **The wedge is therefore not fix B's park path
  firing wrongly — it is whatever pe's "~2 connections then stall" already was,
  now seen from inside with the console still attached.**
- `w4ret rax=0` = `wait4_arm` RETURNED 0 = the WNOHANG path (`wait4_real` returns 0
  only there): ash polled a LIVE child (wc's fork had just been made READY by
  `spawn_child_thread_and_publish`'s `mark_thread_ready`) and got "still running".
  After that: no further syscall prints of ANY kind (a repoll would print another
  w4ret; print-budget truncation ate r14/r15 but a repeated poll would still show),
  no `[sc]`, no `sw to=` — **the last-current task never entered the kernel again:
  a pure ring-3 spin at 100%, and this kernel has no timer preemption yet
  (timer_trap "adds nothing yet"), so the whole box starves — network included.**
- Working theory (to be named by the watchdog build): after the WNOHANG-0 answer,
  ash spins in ring 3 (its dowait/repoll path never reaches the BLOCKING wait4 that
  parked successfully for echo and cat) — starve-by-design for a cooperative kernel;
  pe's boots died the same way minutes later and were read as "channel stall".
  Alternative: the wc fork child was readied but never picked (pick/map bug — the
  TRAMP-MISMATCH row keeps the map question open).
- H1 (fix B heals the ~2-connection channel cap) is **WRONG as stated** — the cap
  was never (only) the pipe arms; the F1 trace completed with the arms inert.

Next: trace build (all `[tmp]`, remove before final): a tick watchdog in
`timer_trap` every 256 ticks (`[tmp] wd n= slot= tid= rdy= run= wt=` via
`thread_stats_full` — names WHO holds the core and what is ready/waiting), wait4
arm's return print gains `opt=` (fr.arg4 — confirms WNOHANG) + `slot=`, fork
success print `[tmp] fork child pid=`, `fire()` prints non-empty wakes
(`[tmp] pipe wake tid=`). One boot answers: ring-3 spin (rdy=0 run=1 current=shell)
vs pick bug (rdy>=1, current=shell).

## Symptom-first docs pass (supervisor directive, 19:5x — before the preemption work)

grep'd `wedged / 100% CPU / hlt=0 / starved / busy-wait / spin / preempt` over
docs/README.md, docs/runbooks/*.md, and the docs/archive + docs/reference/subsystems
titles. Matching rows (or "no row") for the pf wedge (ring-3 loop, 100% vCPU,
tick-only exits, READY thread starved, network starved, zero faults):

- **docs/README.md:167 (FIXED, `2439994b`)** — SMP=1 second-listener freeze: "ticks
  ran the wake-pass but the switch was suppressed and no running thread remained to
  raise a voluntary reschedule." Same machine-shape as pf-3 (ticks cannot cause a
  switch ⇒ one holder starves everything); there the holder was a preempt-disabled
  halter, here it is ring 3 itself.
- **docs/README.md:261 (FIXED 2026-08-18, archive/SCHEDULING_INVESTIGATION.md)** —
  "eligibility was not execution"; the fix armed a run-next hint
  (`WAKE_DEADLINE_PREEMPT`) and dropped the tick to 1 ms — i.e. the crate's
  tick→switch machinery this session now pulls for taoxichuan.
- **docs/README.md:218 (OPEN)** — "[SGI] POOL contended… the box is *unscheduled*:
  POOL gates all preemption and a holder is waiting on something that needs another
  thread to run." The generic unschedulable-box row; pf-3's box is unscheduled by
  design absence (no timer switch at all), not by a held gate.
- **docs/README.md:213 (FIXED, archive/TERM_POLL_INPUT_PREEMPTION_FIX.md)** — a
  blocking loop monopolizing the core under `disable_preemption` wedged the VM at
  ~200% CPU with `[BKL] stuck` — the closest prior wedge-by-holder row.
- **docs/README.md:320 (PARTIALLY FIXED, archive/SMP_SHARED_ONCPU_GATE.md +
  runbooks/debug-smp.md)** — the ON_CPU claim gate is the SMP-switch safety this
  design inherits from the crate's `x86_pick_next` (0→1 CAS, two cores never resume
  one stack).
- **runbooks/debug-network.md:9** — "never `yield_now()` or block inside a
  preemption-disabled window": the timer switch is placed outside any such window
  (ring-3 ticks only; the LAPIC EOI lands before the switch).
- **reference/subsystems/scheduler.md §Preemption** — the spec this adapts:
  aarch64 preempts via tick→SGI (EL0/EL1 arms, sticky wakes, watchdog); x86_64 has
  no SGI, switches are function calls — which is exactly why amd64's `timer_trap`
  switches from the handler and ring-0 code is only *asked* (`need_resched`).
- **No row** carries the pf signature itself (guest ring-3 spin at a fixed rip,
  `rdy=1` never picked, `wt=0`, tick-only exits, no faults). Nearest rows above;
  the closest archive reading is SCHEDULING_INVESTIGATION.md §Resolution.

## Finding: the gates need timer-driven switching (supervisor/owner decision relayed)

**Yes — the evidence says slice 4's timer preemption is load-bearing for this
session's gates**, and per the owner's decision it is now IN this session's scope
(plan §5; built before the next boot). Evidence: pf-3's watchdog — after ash's
`wait4(WNOHANG)`→0 the shell spins forever at rip=0x4ceaa1 (musl `__stdio_write`'s
writev retry loop, identified by disassembly of the guest busybox) with **one
thread READY and never picked** (`rdy=1 run=1 wt=0` every 256 ticks) and nobody
WAITING. A cooperative-only kernel (switch only where a task parks) starves on any
ring-3 loop that needs another task to run: the READY wc child, the boot task's
pump/bridge, and the NIC all starve together — that is the whole box-wedge, and it
is also, with the console record now complete, what pe boots G-I hit as the
"~2-connection channel stall". Report + build ordered by the owner; ring-0
preemption stays out (every kernel path runs IF-masked, so ring-0 ticks land only
in the idle loop's window, which already self-yields).

## Pulled / extracted / written-new

(final list lives in the Gates-final block above)

## Remove before final

(pd's list, all still in the tree): `[tmp] enter` (exec_twin.rs), `[tmp] exit`
(child.rs), `[tmp] w4ret` frame dump (main.rs), `[tmp] sw to=`/top (exec_twin.rs),
`[tmp] fault ctx` (main.rs).

## wc -l

Start: **7618** (`taoxichuan/src/*.rs`).

- 19:1x — trace build committed (aa550347, pushed): wd watchdog in timer_trap,
  w4ret gains opt=/slot=, [tmp] fork child pid=, fire() wake print. Clippy
  (x86_64-unknown-none) clean; kbuild rc=0 (kbuild-pf3.log). Booting pf-2 with
  AKUMA_DEVROOT_BAKE_FORKPROBE for PK2's bake.
