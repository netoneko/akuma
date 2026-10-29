# Taoxichuan stage 8, M4 session pi — PLAN (the residual stall race, then PK2's ALL PASS, then merge-readiness)

Session pi continues from ph. PH PROVED (carried; details in RESULT-ph.md, not re-derived):
- F1, PK1, P1-P5 green in ONE clean boot (ph-9, 2026-10-29 05:15-05:17; client bytes od -c'd
  this session: P1 `hi\n`, P2 `3\n`, P3 `2\n2\n`, P4 `hi\n`, P5 `done\n`, F1 `x\n2 /tmp/f`,
  PK1 `20000\n`), zero faults since logmark-ph9, REAL_FORK on, `[tmp]` stripped (7918 LOC).
- The five wait paths now halt (`net::wait_lap` = sti;hlt;cli, 60658556); the tick backstop
  pumps netpoll (aba585a1); the depth gate is {1,2,3} (a48ec165); both exec crossings zero
  fs.base (cde2247d) — grandfork RUNS and exits 0; only the stall ate its capture.
- The stall is intermittent: ph-7 clean 8/8, ph-8 stalled at exec 1, ph-9 clean 8/8.
  ph-8's shape: stall began BEFORE any exec crossing (no wrmsr, no blk), guest `hlt=0`,
  host `c4=100%`, vm exits ~2x tick, both relay directions frozen, `in_flight=0`,
  `rx_starved` climbing.

WHAT IS LEFT (this session): (1) the residual spin — some path still burns the pinned core
instead of yielding it to the host relay thread; (2) PK2's `grandfork: ALL PASS` in a clean
boot; (3) N consecutive clean batteries (target 5, stated honestly); (4) the runner's
fault-check mark; (5) merge-readiness.

## pi §1. What the other kernels do when idle (the idle contract)

Read from the crates and amd64/src (pulled, not re-derived):
- `amd64/src/kvm/linux/machine.rs:517` — "A halted processor wakes when something can be
  offered to it": when a vCPU halts, the HOST loop flips Halted->Runnable on `plat.offer()`.
- `machine.rs:460-465` — the host pumps the relay every 200 us (`next_relay_us`), and prints
  cpu0 state when a relayed connection waits on a halted cpu0 ("nothing is going to wake it").
- `crates/akuma-kernel-glue/src/lib.rs:1420-1427` — the BSP idle's `idle_halt`: "Without this
  the boot/idle thread is a pure [spin]... pin the host vCPU at 100% even at idle." scheduler.md:161
  lists `idle_halt` as the idle-lap bookkeeping.
- **The contract:** when no task is runnable, the IDLE THREAD HALTS (hlt/WFI) -> the vCPU
  EXITS -> the one pinned host core runs the relay thread (200 us poll laps) -> frames flow ->
  the host OFFERS and re-enters the guest. The halt is not an optimization; it is the only
  thing that ever gives the pinned core to the relay.
- **Taoxichuan violates it:** `taoxichuan/src/exec_twin.rs:178 idle_main` = `netpoll();
  tick_window(); if runnable { yield }; spin_loop()` in a loop — it NEVER halts. Every wait
  path ph converted (sleep_until, park_until, blocking_relax, yield_now, DHCP, connect_wait)
  halts, but the moment the system lands in idle_main with nothing runnable, the core burns
  again and the relay starves. Its own comment says the idle lap is "the everything-path's
  last resort" pump — but netpoll in idle CANNOT deliver frames: the HOST relay thread is
  what moves frames, and it shares this core.

## pi §2. The archive, for the symptom (grepped BEFORE reading source)

`grep -rn -i 'rx_starved|in_flight=0|pinned core|handshake watchdog|relay.*frozen'` over
docs/README.md, docs/runbooks/, docs/archive/ — matching rows:
- `docs/archive/KVM_RELAY_SYN_STALL.md:51,79` — counter semantics; healthy `rx_starved` ~20%.
  :72 — "Both vCPUs run on the one host core the service task is pinned to... the guest gains
  no compute and pays for the time-slicing" (the one-pinned-core note RESULT-ph names).
- `docs/archive/KVM_BKL_EXIT_HANDLING_AND_BULK_WEDGE.md:52` — same counter set but the OPPOSITE
  guest state (`cpu0 halted`, rx rising 4000/s). Ours is `hlt=0` spin: NOT that wedge.
- `docs/runbooks/kvm-linux-guest.md:113` — `gave_up`/`stale_in`/`rx_starved` = "what the relay
  saw"; points back to KVM_RELAY_SYN_STALL.
- `docs/archive/TAOXICHUAN_M4_SLICE3_VERDICT_PLAN.md:35,61,93` — ph's own session rows (the
  frozen `gw_frames_from_guest=69` windows).
- `docs/archive/MULTIKERNEL_NETWORKING_EXPERIMENT.md:181` — herd tracks pinned cores (ops only).
- No row names an idle-loop-without-halt defect; the contract lives in code (§1), not the archive.

## pi §3. The measurement that separates the hypotheses

The stalled boot's host-side counters already printed:
- `hlt=0` + `c4=100%` — the vCPU never exits via hlt: rules out "all waiters parked in the
  converted waits" (those hlt by construction) — SOMEONE is executing on the guest's core.
- `exits ~2x tick` — timer exits continue: the guest is not IF-frozen dead; one-instruction
  IF windows (tick_window) still open, or the spinner is ring 3 (ticks preempt it freely).
- `rx_starved` climbing + `in_flight=0` + both directions frozen — the HOST relay is starved
  of core time, not dead (it prints these). Rules out a host relay bug: the relay cannot run
  because the vCPU never gives the core back.
Three spin sites survive: (a) idle_main itself (no halt — §1); (b) a ring-3 spinner whose
preemption hands the CPU straight back (pg's F3 shape: only runnable task); (c) a kernel loop
outside the five converted waits that spins with IF masked between tick windows.

**Cheapest instrument (one bounded print, one boot):** a `[tmp]` idle-lap counter in idle_main —
count consecutive laps with nothing runnable; every 256th lap print `[tmp] idle laps=N run=<tid>`.
One stalled boot then separates all three: counter climbs + prints continue => (a); counter
stays 0 + wd shows run=<task tid> with a syscall-frame rip => (b); ALL prints (including wd,
TICK) stop => (c), the IF-frozen kernel loop.

## pi hypotheses (ordered by cost per bit of information; all decided by ONE boot each)

- **HI1 (primary): idle_main never halts** — the one structural difference from every other
  kernel (§1). Any window where the last runnable task parks/exits and a net event is pending
  (or the yield_now-with-IF-masked warning lands the schedule in idle) leaves the core burning
  in idle. Intermittence = needs that exact window. Fix: idle lap -> `net::wait_lap()` when
  nothing is runnable (tick wakes it 100x/s; yield check stays AFTER the lap). ~3 lines,
  taoxichuan-local, reuses ph's `net::wait_lap` (net.rs:82) — nothing to pull, nothing new.
  Stop: stall never reproduces across >=5 consecutive full batteries.
- **HI2: a ring-3 spinner outside the converted waits** (an sshd/child loop that never
  syscalls). Fix shape: find the loop, give it the reference userspace's sleeping-poll shape
  (the other kernels' serve loop `sleep_ms(1)`s — ssh.md:49). Cost: needs attribution first.
  Stop: the named loop parks; battery clean x5.
- **HI3: a kernel loop with IF masked outside the five converted waits** (the unexplained
  `[SCHED] yield_now with IRQs masked` warnings are on the exec paths). Fix: convert that wait
  or drop the masked yield. Cost: needs attribution first; the instrument above decides (c).
  Stop: same as HI2.

## pi premise rows (final table lives in RESULT-pi.md)

1. tree `cde2247d` builds — git log done (HEAD cde2247d, 7918 LOC); kbuild exit after the
   plan commit is the evidence.
2. one clean battery still passes F1, PK1, P1-P5 — HOLDS on ph-9's captures, re-verified this
   session: client bytes od -c'd (above), mtimes one window 05:15:23-05:17:15 2026-10-29;
   console exit lines from ph-9's recorded slice (logmark-ph9). pi re-proves it fresh in
   its first battery (stability runs make this premise live again on pi's tree).
3. a stall can start before the first exec — HOLDS per ph's ph-8 note (no wrmsr/no blk,
   exits ~2x tick, c4=100%, hlt=0; RESULT-ph "What is next" 1). The raw capture rotated away
   with vm-dev.log; the recorded window stats are the evidence.
4. the runner's fault check uses a stale mark — HOLDS, and is WORSE than stale:
   `run_pipes.sh:96` greps the WHOLE `$LOG` (no mark at all — crosses boots outright);
   `run_fork.sh:36` writes the FIXED name `logmark-pf.txt` (ph's symptom: gates-P.faults held
   ph-7's grandfork line through ph-8/ph-9). Fix both to per-boot conslice marks.

## pi fix sequence (order, stop conditions)

- **Step 0 (no boot):** this plan + premise rows, commit, kbuild, push litter (WIP).
- **Step 1 (instrument, one boot):** the `[tmp]` idle-lap print in idle_main; build, boot,
  FULL battery (run_pipes.sh then run_fork.sh, 8 execs). Green => keep the instrument, boot
  counts toward stability, go to Step 3. Stalled => read the print, attribute (a)/(b)/(c),
  go to Step 2.
- **Step 2 (fix per attribution):** HI1's idle halt expected (~3 lines, reuses wait_lap);
  HI2/HI3 per the instrument. clippy n/a (bare-metal taoxichuan; pre-commit hook runs HOST
  clippy for touched crates — none planned), kbuild, commit, boot. Stop: battery green.
- **Step 3 (runner fix + stability):** run_pipes.sh + run_fork.sh fault checks from each
  boot's OWN mark (conslice, per-boot mark file) — scripts only, no kernel build; lands
  BEFORE the stability runs so every later gate verdict is self-contained. Then N consecutive
  full batteries on the fixed tree. Target 5, state honestly what was achieved. PK2's
  `ALL PASS` must appear in one of them (probe already in image, exits 0 under the fs fix).
- **Step 4 (merge-readiness):** strip every `[tmp]` print (listed in RESULT-pi "Remove before
  final"), REAL_FORK stays on, `cargo check -p akuma-amd64 --target x86_64-unknown-none
  --features no-tests` (amd64 untouched all session, verify anyway), host tests + clippy for
  touched crates (runners are scripts — n/a; any rust crate touched: its own `cargo test -p
  <crate> --target $HOST`), wc -l start/end, final battery if the strip changed the kernel,
  RESULT-pi.md, push litter, ops restore (herd stop vm-dev, stage4 conf BOTH confs, herd
  disable vm-dev, herd start vm-serve-mc, mirror.sh stop, `herd status`), verify, end.
- 2 boots with no new evidence => re-plan from §1/§2 and the other kernels' crates.

## pi budget map (~140 calls, 8 boots)

b1 instrument+battery; b2 fix+battery; b3-b6/b7 stability (each a full battery + PK2 chance);
b7/b8 spare. Calls: docs+plan ~20 (done ~18), instrument+build ~10, per-boot battery+analysis
~12, runner fix ~6, strip+final ~15, RESULT+restore ~10.

---

# Taoxichuan stage 8, M4 session ph — PLAN (close slice 3: F1, P1-P5, PK1 verdicts in one clean boot; PK2 if cheap; then the [tmp] strip)

Session pg found and fixed the two root causes (tick switch keeps TASK_SLOT in
step with CR3; the exec arm saves into a global EXEC_SAVE; the fork child's
out_slot is stamped) and F1 is green. What is NOT known: PK1's verdict after
the /bin/yes bake, P1-P5, PK2. This session is verdicts and cleanup.

## 0. What pg proved (carried, from RESULT-pg.md — not re-derived here)

- F1 green through the new path, twice (boots pg-6/pg-7): console
  `[SSH] Exit status: 0`, client bytes `x\n2 /tmp/f\n`, zero faults, REAL_FORK on.
- The writev spin was the tick switch leaving TASK_SLOT on another task's
  record; fixed in `switch_to`.
- Hung sessions were the M2 exec arm clobbering the spawn crossing's save;
  fixed with the global `EXEC_SAVE` scratch.
- The fork child's stdout went to slot 0's nobody-drains buffer; fixed by
  stamping `out_slot`/`depth` at bind.
- `/bin/yes -> busybox` bake committed (devroot-ensure); boot pg-7's PK1 ran
  with the link present but its verdict window was lost to console rotation.
- Runner verdicts were UNKNOWN on the P battery: the ~250 KB console log
  rotated under the battery.

## 1. A verdict you can trust, first (Phase 1 §1)

**The rotation driver is identified and it is one print.** pg's last battery
(captures in /root/dev-cycle/taoxichuan-s7/, boot pg-7, 2026-10-28 23:44-23:54):

- `gates-F.txt`: F1 PASS; PK1 FAIL with an EMPTY payload (client saw 38 bytes
  = the connect line only, then `NOTE client killed by pid`); PK2 FAIL
  (not baked); P1-P5 all `UNKNOWN (no console exit line)`.
- The P clients (`od -c`) are FULL of `[ssh] connect attempt 2 failed
  (Connection refused) — retrying` — the P-gate clients never connected.
- The P console slices show the guest ALIVE and SPINNING: `cpu0=run`, `hlt=0`,
  ticks advancing (`[tmp] wd` every 2.6 s), zero faults — and the host relay
  frozen at `gw_frames_from_guest=69` with `rx_starved` climbing (frames that
  waited for the guest to post a receive buffer never delivered).

So the battery died of TWO stacked things: (a) the console log rotated
(the verdict-loss mechanism), and (b) the guest's networking went deaf after
the PK1 exec (the connection-refused mechanism). Both are addressed BEFORE the
first battery boot:

1. **Drop the one rotation-scale print**: `pipes.rs` `[tmp] pipe wake tid=`
   fires per wake — under PK1's `yes | head -n 20000` that is 20000+ lines
   ≈ two full rotations of the 250 KB log inside one exec. Removed pre-boot
   (it is on the strip list anyway). Everything else that prints is bounded:
   `wtrace` (first 12 answers + first 12 zeros + every 65536th), the per-exec
   bind/cw/exit/w4 lines (~15 lines/exec), `wd` (every 2.6 s), TICK pre/post
   (once per exec). With the wake print gone the whole 8-exec battery fits in
   one log rotation, and the runner's `conslice` (.old+live from a logmark)
   holds every verdict.
2. **Watch the PK1 window**: boot 1 keeps the bounded instruments as diagnosis
   insurance; the full [tmp] strip happens AFTER the gates are green
   (prints-only, no boot needed — the PLAN-pg §7 precedent), so a boot is
   never spent twice.

## 2. The archive and the other kernels, for whatever fails (Phase 1 §2)

Rows found BEFORE reading source (grep on the symptom, not the guess):

- `docs/archive/KVM_RELAY_SYN_STALL.md:51,79` — the relay counters: `rx_starved`
  = "frames that waited for the guest to post a receive buffer"; healthy is
  ~20%. Our boot pg-7 P-window: ~100% starved AND `gw_frames_from_guest`
  frozen — the guest stopped posting buffers AND transmitting.
- `docs/archive/KVM_BKL_EXIT_HANDLING_AND_BULK_WEDGE.md:52` — same counter
  set but the OPPOSITE guest state (`cpu0 halted`). Ours spins: `cpu0=run,
  hlt=0`. Not that wedge.
- `docs/reference/subsystems/ssh.md:49` — "sshd's serve loop `sleep_ms(1)`
  ... lets a second connection progress": on the other kernels the serve loop
  is the pump; `docs/reference/subsystems/console.md` and scheduler.md describe
  the same tick/preemption contract taoxichuan now has.
- **How the other kernels serve the same case** (pg's F4, still the answer):
  amd64/aarch64 route write/writev through `akuma-syscalls-glue` (fs.rs:1015/
  1506) with a channel-bound Stdout arm that parks with backpressure
  (never 0, never EAGAIN); decision crates `akuma-syscalls-glue` +
  `akuma-exec`. They run "many ssh connections per boot" because their sshd
  serve loop + interrupt/EPOLLET net stack keep the NIC serviced without a
  cooperative pump — taoxichuan's net.rs:385 says it outright:
  **sshd's `sleep_ms(1)` loop "makes it, not any daemon, the thing that keeps
  TCP alive between events"** (`netpoll()` every lap). If anything stops that
  loop, TCP dies. `KVM_RELAY_SYN_STALL.md` is the row for the symptom.
- No row names a taoxichuan pipe writev spin; no row for `__stdio_write`.

## 3. The connection count (Phase 1 §3)

**The old symptom is back, and worse than "about two".** pg's boot: F1 (exec 1)
green; PK1 (exec 2) ran but delivered nothing; by P1 (exec 4) the host relay
logs `connect attempt N failed (Connection refused)` — the guest stopped
ACCEPTING connections entirely, not a two-session budget. The console at that
point: guest spinning (`cpu0=run hlt=0`, one wd slot=12 tid=12 constant,
rip=0xffffffff8030c704, `rdy=0 run=2 wt=1`), ticks advancing, zero faults,
relay `slot 1 connecting ... syn_tries=3 syn_retries=3 gave_up=0`,
`gw_frames_from_guest=69` frozen, `rx_starved` climbing ~1300/13 s.
**sshd's serve loop stopped pumping the NIC** — the network did not fail; its
only pump was starved.

## Facts (with evidence) vs hypotheses

F1. The P-gate refusals are host-side facts from pg's captures (§1/§3 above).
F2. `netpoll()` runs ONLY from: the NetRuntime hooks (`park_until`,
    `blocking_relax`, `yield_now` — i.e. socket waiters and net-stack yields),
    the DHCP init loop, and sshd's serve-loop `sleep_ms(1)` (`sleep_until`,
    net.rs:385-391). Nothing pumps the NIC from the scheduler or the tick
    (net.rs read whole region 265-393; main.rs grep for netpoll: none).
F3. sock.rs read/poll arms are carried busy-wait stubs (RESULT-pg hooks table).
    A session task parked in one of them spins in ring 3 forever; ticks
    preempt it, but with no other READY task (`rdy=0`) the switch hands the
    CPU straight back; sshd's listener/serve loop (the pump) never runs again.
    `hlt=0` + `cpu0=run` + one constant wd slot is exactly that shape.
F4. The wd rip=0xffffffff8030c704 is a KERNEL address while the vCPU runs:
    wd samples the slot's SAVED frame, not the live rip — consistent with a
    ring-3 spinner whose last syscall frame is parked at that rip.
    (Reading, not yet a boot-proven fact.)
F5. PK2's cost (premise 3): the musl cross-cc exists on this box
    (`/usr/bin/x86_64-alpine-linux-musl-cc`), the bake env is wired
    (devroot-ensure `AKUMA_DEVROOT_BAKE_FORKPROBE` -> `/bin/grandfork`, 0644
    is execable here — load_task never checks the x bit), and the ONLY kernel
    change is the depth gate (child.rs:815 admits shell_depth {1,2};
    grandfork's rungs 3-5 fork at depth 3). Estimated 5-8 calls if green.
    => cheap enough; attempted LAST, in its own boot.
F6. grandfork exits 0 with `grandfork: ALL PASS`, or with the failed rung
    number (c_stress/grandfork.c header); rung 1 is known good, rungs 3-5 are
    the depth-3 generations.

Hypotheses, cheapest confirmation first (all tested by ONE boot):

- **HP1 (the pump starvation, primary):** the PK1 session (or its killed
  client's session) leaves a task in a sock.rs busy-wait stub; sshd's pump
  starves; the NIC deafens; every later exec is refused. Distinguisher in
  boot 1: the console window around PK1 — a constant wd slot from PK1's
  session onward, TICK pre/post lines stop (no new execs served), no faults.
  Fix shape (taoxichuan glue, no new crate; net.rs/sock.rs are taoxichuan
  locals — pulled-by-path `akuma-net`, decision crate for the poll contract):
  (a) make the busy-wait arms pump `netpoll()` per lap (they are the ONLY
  wait paths that do not — F2), and/or (b) pump `netpoll()` on the tick
  backstop so NO ring-3 spinner can deafen TCP again. (a) first (one line per
  arm, matches the "wait loops poll" discipline already in net.rs); (b) only
  if (a) leaves a window. Stop: P-suite + F1 + PK1 in one boot all deliver
  verdicts; the connection budget stops being the binding constraint.
- **HP2 (pipe park/wake lost wakeup under 20000 laps):** PK1 wedges on its
  own — yes parks on the full 64 KiB buffer and no close/wake arrives, or wc
  parks on empty and EOF never delivers. Distinguisher: PK1's console shows
  the exec crossing but no `[SSH] Exit status`; wtrace/w4 lines show the park
  without the resume. Fix in pipes.rs fix-B arms. Stop: PK1 payload = `20000\n`.
- **HP3 (slot/reap exhaustion under the pipeline):** fork children pile up
  (NSLOTS small) and spawn starts failing — sshd would print, wd rdy/run/wt
  counts grow. Distinguisher: bind/spawn prints + wd counts. Fix: recycle
  finished slots. Stop: 8 execs served, slot numbers reused.
- **HP4 (rotation alone):** with the wake print dropped, if the P gates STILL
  lose their console lines, the runner's slice logic is wrong — re-read
  conslice against the real rotation count. (Cheap; no fix boot expected.)

## Premise rows (final table lives in RESULT-ph.md)

1. tree `8ac51685` builds — git log done (HEAD=8ac51685 on taoxichuan-s4,
   8029 LOC confirmed); kbuild exit code after the pre-boot edit (a
   prints-only edit; a green build after it is the premise's evidence).
2. the /bin/yes bake produces a working yes — MERGED into boot 1's PK1 gate
   (a dedicated `yes | head -n 3` boot spends a boot to learn what PK1's
   payload shows anyway: `sh: yes: not found` vs a wrong count vs `20000\n`).
3. PK2's cost — CHECKED (F5 above): musl-cc exists, bake env wired, one-line
   depth gate. Cheap => attempt last, own boot, after the required gates.
4. the [tmp] prints listed in RESULT-pg "Remove before final" are in the tree
   — CHECKED: grep shows child.rs x7 (cw/bind/exit/fork-pid/w4park/w4resume/
   rco), main.rs x4 (fault-ctx/wd/wtrace/w4ret), pipes.rs x1 (pipe wake — the
   rotation-scale one), exec_twin.rs comments only (prints already gone).
   TICK pre/post and [raw] are NOT [tmp]-tagged (per-exec / boot-once).

## Fix sequence (order, stop conditions)

- **Step 0 (no boot):** this plan + RESULT premise rows, commit. Remove the
  pipes.rs `[tmp] pipe wake` print (the rotation-scale one ONLY). Host clippy
  n/a for taoxichuan (bare-metal crate) — the pre-commit hook runs the HOST
  target; kbuild -p taoxichuan must exit 0. Commit. Push litter (WIP note).
  Stop: build green, files committed.
- **Step 1 (boot 1, the verdict boot):** install via boot_dev.sh (default
  ktarget path; verify the install line's size/md5 against the fresh build),
  vm-serve-mc stopped FIRST. Battery order PIPES FIRST: run_pipes.sh (P1-P5),
  then run_fork.sh (F1, PK1, PK2). This inverts the runner header's advisory
  order ("run_fork.sh FIRST") on pg's OWN evidence: PK1 is the suspected
  box-killer (HP1), and the 5 verdict-hungry gates must not sit behind it.
  8 execs = the per-boot cap. Verdicts: console `[SSH] Exit status` line AND
  client bytes (od -c) in the SAME boot, file times from this boot; faults
  judged from this boot's logmark only.
  Stop: all green => Step 3. PK1 red => Step 2 with the PK1 console window.
- **Step 2 (the HP1/HP2 fix, shaped by boot 1):** (a) netpoll per lap in the
  sock busy-wait arms, or (b) tick-backstop pump; or the pipes fix for HP2.
  amd64 keeps building (no amd64 files touched; `cargo check -p akuma-amd64
  --target x86_64-unknown-none --features no-tests` if anything shared
  changes — none is planned). clippy + kbuild + commit + boot 2.
  Stop: failed gates go green in boot 2. If 2 boots yield no new evidence:
  re-plan from the archive (§2) and the other kernels, per the budget rule.
- **Step 3 (the strip + PK2, boot 3):** remove EVERY remaining [tmp] print
  (the grep list in premise 4; keep non-[tmp] TICK pre/post + [raw]; keep
  REAL_FORK = true). Build grandfork (musl static, one cc line), flatten the
  depth gate to {1,2,3}, export AKUMA_DEVROOT_BAKE_FORKPROBE through
  boot_dev.sh's devroot-ensure call, clippy + kbuild + commit. Boot 3:
  run_fork.sh (F1, PK1, PK2) on the stripped binary — doubles as the strip's
  sanity boot AND PK2's verdict. Stop: F1+PK1 green (P1-P5 carried from the
  earlier boot, binary differs by prints only) and PK2 green => done; PK2 red
  with F1+PK1 green => done, PK2 documented (it was "if cheap").
- **Step 4 (final):** RESULT-ph.md final rows, wc -l start/end, push litter,
  ops restore (herd stop vm-dev, stage4 conf BOTH confs, herd disable vm-dev,
  herd start vm-serve-mc, mirror.sh stop, herd status), verify, end.

## Budget map (~120 calls, 5 boots)

Boots: 1 = the verdict battery (Step 1); 2 = fix verify if HP1/HP2 fires
(Step 2); 3 = strip sanity + PK2 (Step 3); +2 spare for re-plans. Calls:
docs reads done (~20), plan+strip+build ~15 pre-boot, battery ~15, analysis
~10/ boot, strip+PK2 ~15, final ~10.
