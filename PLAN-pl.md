# Taoxichuan stage 8, session pl — PLAN: the spawn-park stall

Defect: ~1 boot in 4, the guest parks mid-spawn and nothing ever wakes it. `[SSH] Exec:` →
`[TICK pre]` (the exec crossing) → cpu0 halted forever, tick alive (~200 exits/s), relay healthy
but the guest sends nothing new. Fix = a cause named from evidence, a change at that site, a stall
rate stated over many boots.

## 0. Evidence read before planning (all free — no boots spent)

- STALL-sup1/2/3.vm-dev.log (all three end in the stall): every one is
  `Exec: /bin/sh -c apk add git` → `[TICK pre] n≈2924` → one burst of real work (947 exits/s,
  io=334 blk=389 — the image loading and apk reading its index), then flatline: 200 exits/s,
  hlt=99, `cpu0=halted`, gw_frames_from_guest creeping ~1 frame/10 s (TCP keepalives), and later
  `relay slot 1 open` (the host client reconnecting; its SYN is SYN-ACKed by the guest TCP stack —
  `synack_in` increments — but no SSH banner ever flows, so the sshd userspace never runs again).
- `[TICK pre]`/`[TICK post]` are NOT timer prints: child.rs prints `[TICK pre]` inside
  `exec_replace` (the shell tail-call exec'ing the real command; `echo` is a builtin, which is why
  the boot-ok execs print no tick line) and `[TICK post]` in `child_exit`. GREEN execs complete
  pre→post within the SAME tick count (n=2926 → n=2926 in sup1's green battery); the apk exec is
  the ONLY multi-tick ring-3 stretch in a battery. All three saved boots: `[TICK pre]`, then no
  `[TICK post]` for 23,000+ ticks.
- Green-run shape (sup1 lines 7186-7254): pre/post/Exit-status all present, ~1600 lapic ticks
  between execs.
- The `EXEC_SAVE` comment (child.rs, session pg) documents the exact symptom as a known past body:
  "lost sshd's resume frame: the image's child_exit ran exit_cross into the exec's dead save and
  **sshd never resumed — no drain, no [SSH] Exit status, a hung session**". pg fixed the nested-exec
  overwrite of that save; the stall is the same symptom recurring intermittently.

## 1. The stall-rate harness first

`scripts/kvm/goose/run_stallrate.sh` (beside run_fork.sh, same conventions: TERM/HOME/env,
`/var/log/herd/vm-dev.log`, SSHPORT=2224, PACE=15, per-boot fresh `.orig` image via boot_dev.sh).

- `run_stallrate.sh N` — N boots × 3 gates: F1, PK1, P1 (the three cheapest gates; no mirror
  needed). Per-gate verdict window: the logmark is taken at THAT gate's start (never whole-boot —
  a whole-boot window sees gate 1's `Exit status` and misreads gates 2-3). CLEAN gate = this
  window's `[SSH] Exit status: 0` within 90 s while the client exits 0. STALL = no exit line in
  the window in 90 s. The boot is classified at its FIRST stall (the channel is dead for the rest
  of the battery; later gates on it would misreport as failures) and the stalled window's console
  slice is saved to `$OUT/STALLrate-bootK-gateJ.console`. Nothing is killed before the console
  line appears; after a stall verdict the client is killed (the boot is over for evidence
  purposes) and the next boot starts. Prints `clean/total` in one line.
- `run_stallrate.sh N apk` — the heavy probe: per boot, ONE exec `apk add git`
  (mirror.sh start before the run, stop after), same per-gate window and slice save.
- Baseline = its first output on the unmodified tree.

Cost ≈ 50 s boot + 3×(exec+PACE 15) ≈ 2 min/boot.

## 2. Where wakes come from (read from source, not guessed)

Parks in the spawn/exec path and their ONLY wake sources:

| park site | mechanism | wake source |
|---|---|---|
| `spawn_arm` → `child_enter` crossing (the "parked sshd task") | NOT a threading park: the caller's registers/CR3/stack live in the slot's `CrossSave`; the machine runs the child | ONLY the child's `exit_group` → exit arm → frame restore into `spawn_arm`'s tail. No timer, no relay, nothing else can resume it |
| `exec_replace` → nested `child_enter` (sh→apk) | same, through the static `EXEC_SAVE` scratch | ONLY the exec'd image's `exit_group` → `child_exit` → restore |
| `wait4_real` park | `schedule_blocking(u64::MAX)`, waiter stamped into `slot.waiter` before parking (IF-masked) | `task_exit_real`: `get_waker_for_thread(waiter).wake()` (crate's sticky-wake makes a pre-park wake safe) |
| pipe read/write (`pipes.rs` `park_once`) | `schedule_blocking(u64::MAX)` | the peer op's returned `Wakes`, fired via `fire()`; crate's one-step `check_set_*` anti-TOCTOU |
| sock wait laps (sock.rs:117,299) | `sleep_until(+1ms)` — deadline-bounded | the wake pass each tick/yield; cannot flatline |
| net serve loop / blocking_relax | `sleep_until` laps + `netpoll` | deadlines + netpoll from the tick |
| boot task (`NO_SLOT`) | never parks (`may_park` → None; `wait4_arm_blocking_scan`) | n/a |

The tick (`timer_trap`, main.rs:362) NEVER wakes anyone: it counts, EOIs, conditionally calls
`x86_yield()` (only when it interrupted RING 3, slot valid, `PREEMPT_ARMED`), then `netpoll()`.
So a lost crossing-resume is fatal and PERMANENT by design — nothing periodic re-checks it. That
is why this defect halts instead of retrying.

How the other kernels do it: amd64/aarch64 have no suspend-the-parent crossing — execve replaces
the image under the same Process row and the spawning thread stays a normal runnable; parks wake
through the same akuma-threading crate (wake keyed by (tid, generation) in `WakeHandle`, sticky
`WOKEN_STATES` checked before parking, WAITING→READY CAS — host-tested); aarch64 adds an
IRQ-driven scheduler (voluntary SGI + timer wake pass).

Lost-wake rows in the archive (the grep's matches, judged):

- `AKUMA_AMD64_RING3_SEAM_SLICE7.md` §2.3 "The futex namespace — a lost wakeup, 2 runs in 3":
  a wake keyed `(tgid, uaddr)` when the op should key by address space — WRONG-KEY wake.
- `AKUMA_AMD64_NO_SLOT_RECYCLER.md`:123 "parked — the lost-wakeup shape behind the -j4 wedge":
  same family — slot/queue reuse handing a wake to the wrong occupant.
- `AKUMA_AMD64_STREAM_END_STALL.md`:123: "'register the previous one', whose mechanism (a) is
  lost wakeup" — event registered against the previous connection.
- Symptom matrix row (AKUMA_AMD64_THREAD_LIFECYCLE.md): "**Waking is not interrupting**" —
  `sched::wake` only makes a task runnable; a parker whose loop re-checks its own condition parks
  again; the fix arms the real unblock alongside the wake.
- `AKUMA_NET_ISSUES.md`:554: "so a missed wake is never worse than the behaviour it replaced" —
  the design rule these fixes follow.
- `AKUMA_KVM_AKUMA_GUEST.md`:154: "Not the hypervisor's timers or a lost wakeup" — the
  host-relay/timer family explicitly exonerated there; matches our relay-alive evidence.

None of these rows IS this bug (ours has no futex, no slot recycle between boot-ok and the stall,
no stream-end registration); the FAMILY is "wake/resume keyed, armed, or delivered against the
wrong piece of state".

## 3. Hypotheses (cost per bit, cheapest first)

- **H1 — the tick's preemption switch, landing inside the only multi-tick ring-3 stretch (the
  exec'd child), breaks the crossing's resume state.** apk's crossing+run spans many ticks (every
  green exec else completes sub-tick); a tick preempts it, `x86_yield` switches coroutine contexts
  away from the tick's trap band, and the child's resume depends on the switch machinery
  re-entering the trap-frame path (the thing sessions pg/pi kept patching bodies out of). A lost
  resume = the observed forever-idle.
  - Confirm/kill: ONE diagnostic build, [tmp] const gating the `x86_yield()` call in timer_trap
    off (on_tick + netpoll stay). Run the harness 6 boots. 6/6 clean vs baseline ≈2/8 stalled →
    the trigger is the preemption switch; then instrument the switch (below) on the normal tree
    to catch the losing boot and name the exact field. Stalls persist → H1 killed as trigger.
  - Stop: 6/6 clean, or 2 stalls on the off-switch tree.
- **H2 — the exec'd child parks in a guest syscall (pipe/sock) whose wake is misrouted.**
  Kill-by-inspection so far: pipes are the host-tested crate with anti-TOCTOU check_set forms;
  apk's output (~100 B) cannot fill the 64 KiB FIFO; sock laps are 1 ms deadline-bounded (a parked
  sock lap would keep the exit rate high and the relay busy — the flatline says no). Confirm/kill
  in the field: a `[tmp]` print in `park_once` (`[tmp] PIPEPARK slot=…`) rides the instrument
  build; a stall whose last line is PIPEPARK names it.
- **H3 — exit misroute: the exec'd image's `exit_group` never reaches `child_exit`** (e.g. routed
  to `task_exit_real`/`x86_finish_current`, orphaning the suspended crossing) **or reaches it and
  the restore fails silently**. Confirm/kill: `[tmp]` prints at both arms' entry
  (`[tmp] EXIT slot=… real=… code=…` / `[tmp] TASKX slot=…`); a stall with NO exit-arm line at all
  means the child never exited (points back to H2/park); a TASKX line names the misroute; an EXIT
  line with no resume names the restore (H1's family, but exit-side).
- **H4 — host/relay starvation.** KILLED by the saved evidence: the relay stays healthy and keeps
  delivering (to_guest climbs, SYN-ACKs flow for the reconnect), the guest simply stops sending.
  Reopen only if H1-H3 all die with no new evidence.

Instrument build (one, all [tmp], riding together, rate-capped to the first ~40 per boot so the
apk window is covered): SW prints in timer_trap when the switch gate passes
(`[tmp] SW from=<slot> to=<idx?> n=<ticks>`), EXIT/TASKX arm prints, PIPEPARK. Plus per-gate
logmarks the harness already keeps.

## 4. Boot budget (honest arithmetic)

Baseline 4 boots + off-switch 6 + instrument 2 (only if H1 needs the live catch) + fix + 10
consecutive clean + 1 battery boot ≈ 21-23. The 10-consecutive stability gate and the final
battery are hard gates; the discovery phases are cut to the minimum that discriminates. If a step
yields no new evidence after 2 boots, say so here and re-plan from the archive + the other
kernels' paths before spending more.

## 5. Composition rule accounting

To be filled per piece as it lands: what was looked for where (`grep -rn <name> crates/ amd64/src/`),
pulled / extracted / written-new, amd64 build gate (`cargo check -p akuma-amd64 --target
x86_64-unknown-none --features no-tests`), aarch64 green, `wc -l taoxichuan/src/*.rs` start 8085.

## 6. Findings log (append as evidence lands)

- (baseline pending)
