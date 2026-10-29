# Taoxichuan stage 8, M4 session ph — RESULT (slice 3 close: the verdicts; PK1, the connection count, PK2's probe)

Session resumed on the supervisor's note (quota stop mid-strip). State found:
HEAD ff698f61 + the uncommitted strip in child.rs/main.rs/pipes.rs; ops left
dirty (vm-dev running, vm-serve-mc halted) — restored at the end, as usual.

## Premise rows (final)

| # | premise | verdict | evidence |
|---|---|---|---|
| 1 | tree is `8ac51685` and builds | **HOLDS** | session: 8ac51685 -> ff698f61 (found) -> cde2247d (HEAD); every commit kbuild exit 0 |
| 2 | the `/bin/yes` bake produces a working yes | **HOLDS** | ph-5 and ph-9 PK1 client bytes `20000\n`, exit 0 |
| 3 | PK2's cost: "no probe binary, no bake env" | **HOLDS, paid** | one musl-cc line -> grandfork (static-pie 106448 B); `AKUMA_DEVROOT_BAKE_FORKPROBE` -> devroot_ensure.rs:98 bakes `/bin/grandfork`; depth gate {1,2,3} (a48ec165). Probe now RUNS and exits 0 — its output is eaten by the stall race (below) |
| 4 | the `[tmp]` prints are in the tree | **HOLDS, then GONE** | strip f47e87be (13+/296-); `grep -c '\[tmp\]'` = 0 prints; 8029 -> 7918 lines |

## The gates (ph-9, the clean boot: 2026-10-29 ~05:2x, all verdicts from ONE boot, post-logmark-ph9)

- **P1 PASS, P2 PASS, P3 PASS, P4 PASS, P5 PASS** (run_pipes.sh exit 0) —
  client bytes read: P2 `3\n` (the gate that opened the session's hunt), P3
  `2\n2\n`.
- **F1 PASS** — console exit 0, client bytes `x\n2 /tmp/f\n`.
- **PK1 PASS** — `20000\n`, exit 0.
- **no faults**: `grep -c "Fault|PANIC|sig!"` since logmark-ph9 = **0**.
- console-alive PASS (ticks answer execs).
- **PK2 FAIL (documented, not green)**: the probe is IN the image, RUNS, and
  exits 0 (console exit line seen) — but its client capture is empty: the
  connection's output path stalled mid-exec (the standing race, below). The
  runner killed the client; payload 0 bytes.

Boot history this session (all cde2247d-lineage unless noted):
ph-5 (pre-session stop, ff698f61+instr): F1/P1/P3/P4/P5/PK1 PASS, P2 FAIL
(stall), PK2 exit 127 (no probe). ph-6 (aba585a1+strip): P2 fixed (the tick
backstop), P3 stalled. ph-7 (60658556, hlt laps): ALL 8 required execs PASS
(P1-P5+F1+PK1 — /tmp/rp2.log, /tmp/rf2.log), PK2 = grandfork TLS crash.
ph-8 (cde2247d): stalled at exec 1 — the crossing never ran, so the fs fix
was NOT the cause; the standing race, not a regression. ph-9 = the clean
battery above.

## What the session proved (evidence, not guesses)

1. **P2's failure was never the pipeline.** ph-5's P2 console slice: 3 forks,
   2 pipes, wc wrote `3\n` (br=12 dlen=2), all reaped, `Exit status: 0`,
   bridge drained — while the slice opened with an accepted connection that
   never authenticated: ~10 s of frozen `gw_frames_from_guest` / flat
   `rx_starved` / `host_recv_idle` climbing, the client's handshake watchdog
   (`connection stalled during handshake`, /bin/ssh strings) killed it, and
   the reconnect+re-exec is what the capture really shows.
2. **The stall's mechanism, in two layers.**
   - Guest side: every wait path was a pause()/spin_loop burn
     (sleep_until = sshd's own tick, park_until/blocking_relax/yield_now,
     the DHCP wait, connect_wait) — `hlt=0`, host `c4=100%` for whole
     windows. FIXED (60658556): `net::wait_lap` (sti;hlt;cli) in exactly
     those loops; the calibrator's PIT gate poll keeps the bare pause.
   - Host side: the relay thread shares the ONE pinned core with the vCPU
     (the archive's single-core note); a never-exiting vCPU starves it, and
     BOTH relay directions freeze (`in_flight=0`, `to_guest` frozen,
     `rx_starved` climbing — `KVM_RELAY_SYN_STALL.md`'s row). ph-8's P1
     window (20/21 samples c4=100%, hlt=0, one window hlt=44%) shows a
     residual spin state the converted loops do not cover: the race remains
     intermittent (ph-7 clean 8/8, ph-8 stalled at exec 1, ph-9 clean 8/8).
3. **The tick backstop (aba585a1)** — one drained netpoll lap per tick,
   self-guarded (NET_UP after `akuma_net::init`, IN_LAP against nesting) —
   holds regardless (P2 green in every boot since), but ph-6's P3 stall
   proved the pump was never the whole story: the guest stack was already
   pumped by the stub arms and the nanosleep arm.
4. **PK2's probe crash, root-caused and fixed (cde2247d).** PageFault
   rip=0x1000177d cr2=0x7131c0 code=0x4 = a user read at fs+0x28 (musl's
   stack guard) with fs still at 0x713198 — the EXEC'D SHELL's TLS base
   inside busybox's bss, unmapped in grandfork's fresh CR3. The MSR rides
   the child_enter crossing (sysret never touches it): a depth-2 exec (ash
   execs the pipeline's last command in place — PK2's is the battery's
   first) entered the new image with the outgoing task's TLS pointer. Both
   crossings now `set_fs_base(0)` (the fork path's inheritance stays
   deliberate). ph-9: grandfork runs, completes, exit 0 — the fix is good;
   only the stall race ate its output.
5. **The hook's [tmp] set is gone** (f47e87be): every bounded trace print of
   the P1/P2 hunts + nr_name + 2 orphan match bindings; the depth gate
   flatten (a48ec165) rides its own commit. REAL_FORK stays on by default.
   Not [tmp]-tagged and kept: TICK pre/post, [raw], `[cx]` (stage-7 probes),
   `[threads] high-water`, `[SCHED] yield_now` warnings, TRAMP-MISMATCH
   (fires on anomaly only).

## Hooks table (final)

- pipe read/write(+gathered)/release arms real park/wake; switch_to keeps
  TASK_SLOT in step; EXEC_SAVE scratch; bind stamps out_slot/depth;
  REAL_FORK on by default — all carried from pg.
- **netpoll: tick backstop + guarded laps** (aba585a1): NET_UP/IN_LAP.
- **wait laps halt** (60658556): sleep_until, park_until, blocking_relax,
  yield_now, DHCP wait, connect_wait -> net::wait_lap.
- **exec crossings zero fs** (cde2247d): exec_real_task's and the spawn
  crossing's child_enter; fork's inheritance untouched.
- sock.rs read/poll arms: carried busy-wait shape, now hlt-lapped via the
  sleep_until path (poll_arm); read/write arms stay one-shot syscalls.
- depth gate: {1,2,3} (a48ec165) for grandfork's rungs.

## Pulled / extracted / written-new (composition rule, per piece)

- `net::wait_lap` + the netpoll guards: **written-new, taoxichuan-local**
  (net.rs, main.rs); no crate touched; amd64 untouched all session.
- grandfork: **pre-existing probe** (userspace/forktest/c_stress), built
  with the box musl cross-cc; baked by the pre-existing devroot opt-in.
- Strip: deletions only (f47e87be). `wc -l`: start 8029 -> **end 7918**.

## Commits (all pushed to litter, taoxichuan-s4; origin never touched)

ee420a1d, 443122f3, 86d90a10, 161f55a1, ff698f61 (found), **f47e87be**
(strip), **aba585a1** (tick backstop), **a48ec165** (depth {1,2,3}),
**60658556** (hlt laps), **cde2247d** (exec crossings zero fs).

## What is next (the hand-off)

1. **The residual stall race is the one open defect.** ph-8 pinned its
   shape: it can begin BEFORE the first exec crossing of a battery (no
   wrmsr, no blk, exits ~2x tick, c4=100%, hlt=0) and it eats PK2's output
   path even when the exec completes. Next session: find the un-converted
   spin (suspects: a userspace sshd loop that never syscalls between
   events; a kernel spin path outside the five converted waits; the
   `[SCHED] yield_now with IRQs masked` warnings are on the exec paths and
   unexplained), with the amd64/aarch64 idle contract as the reference.
   The runner's fault check also needs its mark file switched from the
   stale logmark-pf.txt to the boot's own mark (gates-P.faults still held
   ph-7's grandfork line during ph-8/ph-9 runs).
2. PK2 is one clean boot away: the probe runs and exits 0 with the fs fix;
   its ALL PASS just needs a boot whose output path doesn't stall.
3. REAL_FORK on, M2 scaffold still compiled and selectable — unchanged.
