# Taoxichuan stage 8, M4 session pe — RESULT (slice 3 again: debug + gates)

Plan: `/root/dev-cycle/taoxichuan-s7/PLAN-pe.md` (committed as `9ca14fd7` before
any build or boot). Branch `taoxichuan-s4`, start head `33378e5d`.

## Box-reset finding

Box was hard-reset ~12:30 UTC before this session (uptime 59 m at 13:29; no
commit, no stale build locks from the earlier attempt). Its ~56 calls are lost
— the plan-first discipline is the mitigation, and it held: this session's plan
was committed before its first build.

## Premise rows (re-run at session start; final after the last boot)

| # | premise | verdict | evidence |
|---|---|---|---|
| 1 | tree ends at pd's `33378e5d` and builds | tree **HOLDS** (`git log` ends `33378e5d`); build: **HOLDS** — kbuild exit 0 (kbuild-pe3.log/pe4.log) | `git log`; kbuild |
| 2 | `REAL_FORK` is `true` and two `[tmp]` prints exist | **HOLDS** (was `false` at attempt-3 start — the committed boot-1 flip; the session's own flip commit puts it back) | `child.rs:422`; `[tmp] exit` `child.rs:539`; `[tmp] enter` `exec_twin.rs:442` |
| 3 | `yes`, `head`, `wc` exist in the guest image | **HOLDS** — `busybox --list \| grep -cx -e yes -e head -e wc` → `3`; 402 applets; exit 0 (B1.client, boot B) | boot B console |
| 4 | the chosen probe speaks this kernel's ABI | **HOLDS on paper** — PK2 = `userspace/forkprobe` (libakuma, raw Linux x86_64 syscalls, static; `docs/reference/userspace-layout.md`); fallback `c_stress/spawnprobe` | source header + layout doc |
| 5 | M2 (blocking fork) still green when `REAL_FORK` is off | **HOLDS** — attempt-2's boot 1 was VOID (stale kernel); the redo (boot B, Fix A' tree `ffb09617`, flag off, artifact c574e2d0): F1 exit 0 + exact `x\n2 /tmp/f\n`, P1-P5 PASS, no-faults PASS, 8+ spawns in one boot (the Fix A' spawn-#2 wedge is gone) | gates-P.txt; F1-pe2.client od -c |

## Session-notes log (appended; never rewritten)

- 2026-10-28 ~13:55 UTC: plan committed (`9ca14fd7`). H2/H3 (pd's enter_user /
  wait4 hypotheses) killed on paper — see PLAN §1 K6-K8. Root-cause candidate
  H1: EXIT arm routes a real child into M2 `child_exit` (`nested_active()` is
  true for the child), whose `exit_cross` pops a zero return address from a
  never-initialized `CrossSave` = pd's deterministic `rip=0`.
- 2026-10-28 ~15:0x UTC (attempt 3): step-0 host checks. `cargo test -p
  akuma-threading --target x86_64-unknown-linux-musl` DID NOT COMPILE — the
  two context-stamp tests use `Context.x19` (aarch64 callee-saved stamp); the
  x86_64 `Context` is `{magic, rsp}` → 4× E0609, suite never ran on this box.
  Minimal test-only fix (`stamp_slot()` arch-selected canary, x86_64 borrows
  `rsp`; no real switch in a host test) → **30/30 pass**, including the K10
  park/wake publish-race invariant → **H4 host leg: machinery sound**. Lib
  clippy `-D warnings` clean (19 pre-existing `decimal_bitwise_operands` lints
  in sigframe.rs TEST code under `--all-targets` — present before this edit,
  left alone; no active pre-commit hook in this clone, discipline is manual).
  Commits: `7d4e4d4d` (pd's Cargo.lock residue — the four new taoxichuan deps
  in Cargo.toml since bb36ab3a, lock entry never committed; found as the
  unexplained dirty lock at session start) and `f3567d46` (the test fix).
  `wc -l taoxichuan/src/*.rs` start-of-attempt-3: 7530 total.
- (this file updated after every build / boot / commit)
- 2026-10-28 ~15:5x UTC (attempt 3): **the boot-A wedge root-caused and fixed
  (Fix A', `ffb09617`)**. Boot A's console: first `__BOOT_OK__` probe green
  (`[TICK post]` → `Exit status: 0` — child_exit, M2 path), second probe
  stalled at `[SSH] Exec` with `[SCHED] WARNING: yield_now with IRQs masked
  tid=1 lr=0x0` then 100% idle spin — identical to the 13:58 boot's stall.
  Chain: spawn_arm stamps `used+running+current_slot(idx)` for M2 children
  too (child.rs:798-802/861) and `NEXT_SLOT` starts at **0** — the first
  login spawn lands on slot 0 (excluded by is_real_task's `cur==0` guard →
  probe 1 passed), the second lands on slot 1 where Fix A's unqualified test
  read TRUE and routed the M2 child's exit_group into `task_exit_real` →
  `x86_finish_current` abandoned sshd mid-spawn-syscall → idle-only spin.
  Fix A's premise ("the reorder is inert with REAL_FORK off") was WRONG —
  the flip was irrelevant; the reorder alone breaks both flag settings at
  spawn #2. Fix A': `SpawnSlot.real` bind bit (only `bind_child_task` sets
  it; `new()` clears on reuse); `is_real_task()` requires it; EXIT/EXECVE
  arms unchanged. H1's mechanism (real child → M2 child_exit → rip=0) stands;
  Fix A's test was the flaw. kbuild exit 0, artifact md5 c574e2d0 (size+md5
  changed vs the booted c574e2d0-predecessor 8e8fde5d ✓). Premise 1 HOLDS.
  (boot_dev.sh's "BOOT FAIL no ssh round trip" on boot A was FALSE — its
  readiness poll timed out while the console showed the round trip green;
  console verdicts only, per the run_pipes header.)
- (this file updated after every build / boot / commit)

## Attempt-2 post-mortem (reconstructed 14:43-14:55 UTC from boot_dev.log,
gates-P.*, P*.console, /var/log/herd/vm-dev.log; the attempt died at the
14:07 supervisor-watchdog event before writing any of this)

- **Boot 1 (13:46, "premise-5 baseline") — VERDICT VOID, wrong kernel.**
  boot_dev.log line 2495: it installed
  `target/x86_64-unknown-none/release/taoxichuan (775304 B, md5 875d731d)` —
  the REPO `target/` artifact of Oct 27 14:12 (a session-T-era kernel), because
  boot_dev.sh was invoked with a repo-relative path instead of the default
  `/root/ktarget/...`. That kernel predates 61cc5e71's raw `SYS_pipe` (22) arm
  and all of pd's slice 3. Hence every pipe gate died with
  `[sc] unhandled ? (22)` + `sh: can't create pipe: Bad file descriptor`
  (P1-P5 consoles all show it) — the pipes.rs:42 doc comment describes exactly
  this pre-61cc5e71 signature. Nothing was learned about the Fix A tree.
  - gates-P.faults (4 lines: akuma-not-even-once PANIC, one kernel PageFault,
    2× `rip=0 code=0x10` at rsp 0xffff8000006d4898) are **stale whole-log
    greps**: boot_dev.log's copy of the boot has zero Fault/PANIC lines; the
    rip=0 pairs are pd boots 2-3's signatures still in the pre-truncation log.
  - **New ops trap:** after any rebuild, verify the install line's size/md5
    differs from the previous boot (and matches the fresh /root/ktarget
    artifact) before trusting a boot's verdicts. Boot with the DEFAULT kernel
    path only.
- **Boot 2 (13:58, "probe") — correct kernel, guest never served SSH.**
  Installed `/root/ktarget/.../taoxichuan (1008392 B, md5 8e8fde5d)` = the
  artifact built 13:43:01, i.e. tree `e96a57dc` (Fix A + REAL_FORK off).
  boot_dev.log tail: the ssh client stalled **during handshake from its first
  connection** ("no data from server" ×6, then refusals); relay counters:
  `syn_to_guest=2 synack_in=2`, slot 0 open with `in_flight=369 gw_rx=0` for
  40+ min — guest TCP answered SYN but sshd never spoke. No exec ran; no
  `[SSH Auth]` ever. The boot's console 13:58-14:07 was lost to the 14:07
  watchdog-event log truncation (current vm-dev.log starts at up_s≈2520), so
  the guest-side view of the stall is gone. Zombie VM stopped 14:55 UTC
  (`herd stop vm-dev`; both vm services halted, kvm0 free). Plan: retry once
  on a fresh boot before blaming the tree (the diff vs pd's 3-for-3 boots is
  the inert-when-off EXIT reorder + the const flip).
- run_gates_nom4.sh / run_gates_sup.sh (untracked) are copies of run_gates.sh
  the attempt made; run_pipes.sh itself is untouched (git-clean) — P1-P5
  verdicts came from the unmodified script.

## Hooks table (real / no-op / stub — updated when Fix A/B land)

Changed this session (on `ffb09617` + `ecaca7b1`):

- **EXIT arm**: `is_real_task()` → `task_exit_real` FIRST, then `nested_active()`
  → `child_exit` (Fix A reorder, `fa1c0395`) — correct ONLY since Fix A'
  made `is_real_task()` a real discriminator.
- **`is_real_task()`**: slot in 1..NSLOTS && used && running && **`SpawnSlot.real`**
  (new bind bit; only `bind_child_task` sets it — Fix A', `ffb09617`).
- **`sched_root_of` / `sched_task_slot_of`**: key on `exec_twin::shell_tid()`
  (recorded at every SPAWN crossing) instead of hardcoded 0 (Fix C, `ecaca7b1`).
- **`switch_to`**: unchanged mechanically; doc updated (the shell's tid is its
  spawn slot, not 0).

**Still stub/no-op (honest)**: pipe arms still drop wakes / never park (K9 —
and its socket cousin bit us live, see boots E-I); `on_process_exit` no-op
(zombie Process row per child — the `[TRAMP-MISMATCH]` warning is its
signature); child-AS frames leaked per fork; M2 blocking fork still selectable
via `REAL_FORK` (`clone`/`vfork` always blocking).

## Gates

(started; each gate gets console evidence below as it runs)

- **Boot B — premise-5 baseline, Fix A' tree, REAL_FORK off** (15:5x-16:0x UTC,
  artifact c574e2d0 via the DEFAULT /root/ktarget path, BOOT OK ready_in=49s):
  - premise 3: `busybox --list` → `3` of {yes,head,wc} present, 402 applets,
    `[SSH] Exit status: 0` (B1.client).
  - **P1-P5 PASS + no-faults PASS** (`gates-P.txt`, fresh run_pipes.sh run;
    P*.client/P*.console overwritten with the green run — void copies kept as
    *.void*).
  - **F1 exit 0**, client bytes after the client banner exactly
    `x\n2 /tmp/f\n` (od -c F1-pe2.client) — through the M2 scaffold.
  - Zero `Fault|PANIC|sig!` lines above the boot's start (logmark-pe-b4.txt =
    the pre-boot line count; the fault gate's whole-log grep is clean this
    time because the log was truncated at 14:07 — pd's stale rip=0 lines are
    gone; from now on the whole-log fault grep is safe only until the next
    rotation, so fault attribution also uses the logmark).
  - 8+ sequential spawns on one boot with no wedge — Fix A' itself is
    validated by the baseline boot.
- Next: boot C = REAL_FORK on (the flip commit), F1 + P1-P5 through the NEW
  path, then PK1 (`yes | head -n 20000 | wc -l` → `20000\n`) and PK2
  (forkprobe).

## Boots C-G: the NEW path works end to end; the blocker moved to the channel

- **Boot C/D (trace builds, 88826508/9f5bf9c4):** F1 reached the FULL real-child
  lifecycle — `[tmp] enter tid=1` → child ran (open/write/close silent) →
  `[tmp] exit tid=1 code=0 wake=2` (task_exit_real woke the parked shell) —
  **the kernel rip=0 is GONE (H1 confirmed and fixed by Fix A')** — then died
  at a RING-3 `rip=0` fault. Boot D's w4ret frame dump proved the KERNEL frame
  intact at resume → **root cause #2: sched_root_of/sched_task_slot_of
  hardcoded the shell as threading tid 0, but the shell's crate tid is its
  spawn-ring slot (2 on every gate boot) — the wake switched the shell back
  under a stale fork-child record's CR3 (exec-swept, zero stack pages); the
  wrapper's `ret` popped 0. Fixed as Fix C (`ecaca7b1`, live shell tid).**
- **Boot E/F (0df94a5c): console-rotation fog + a guest-side writev retry
  storm** (slot-2 shell writing into a dead ssh channel — EAGAIN forever, no
  backpressure: the K9 pipe cousin on the socket arm; the guest leaked and
  died). The vm-dev.log SIZE-rotation (~250 KB → .old) is what ate consoles
  mid-exec; my per-syscall `sc` print multiplied the storm. sc print dropped
  (`88826508`+trace-3). Logs preserved as vm-dev.log.bootEF-spam*.
- **Boot G (0536b5ca): F1's PARK/WAKE/REAP CYCLE COMPLETED** — enter → exit
  wake=2 → sw to=2 (shell's own band) → `w4ret rax=0x5` (the child's pid,
  perfect frame) → third reap rax=0 — the guest alive at 864 exits/s doing
  IO. **The client connection died mid-exec instead** (stall → 10-attempt
  retry storm → channel burned). Diagnosis: the ~30-connection forward
  ceiling — polls + probes + retry storms exhaust it before the gates do.
- **Boot H plan: one fresh boot, the whole battery back-to-back** (F1, P1-P5,
  PK1; PK2 deferred — forkprobe is a repo crate, not in the image, and
  injection is its own session), console verdicts only, no client retries
  once a stall appears.

## Boots H-I close-out (17:3x-18:1x UTC)

- **Boot H** (fresh, 0df94a5c): F1 executed **fully in-guest** — the identical
  green trace (zombie reap → park → enter → exit wake=2 → `w4ret rax=0x5` →
  third reap rax=0) — but my client's connection had died in the pre-exec
  stall, and P1-PK1's connections were REFUSED: the orphaned-output bridge
  writev-loop leaked the guest to death minutes after F1. **Boot I** (same,
  after a 100 s post-boot wait): identical — the stall is not a fixed window.
- **Where F1 stands**: the M4 mechanism (real fork child + wait4 park + wake +
  switch back + reap) is PROVEN LIVE on three boots (G, H, I) with console
  evidence and zero faults. The client-bytes/Exit-status verdict is blocked by
  the ssh channel: it serves ~2 connections cleanly per boot, then stalls
  (relay `gw_rx=0`, `in_flight` grows, guest net thread starved — vCPU at tick
  rate only), and once an exec's output is orphaned onto a dead channel the
  bridge's writev-EAGAIN loop (no backpressure — K9's socket cousin) leaks the
  guest to death. That is the K9 pipe park/wake class (the plan's unfunded
  Fix B) plus a channel robustness question — NOT the fork path. Next session:
  Fix B (pipe park/wake — the sshd's own event path likely rides it too),
  then the full battery in one boot.

## Pulled / extracted / written-new

Nothing pulled or extracted this session (look-first used throughout; every
fix is target glue in `taoxichuan/src/`):
- **Written new (target glue)**: `SpawnSlot.real` + bind-time set (child.rs);
  `exec_twin::SHELL_THREAD_TID`/`set_shell_tid`/`shell_tid` (exec_twin.rs);
  `stamp_slot()` test canary (akuma-threading, test-only).
- **Committed housekeeping**: pd's Cargo.lock residue (`7d4e4d4d`).

## Remove before final

pd's list still in the tree: `[tmp] enter` (exec_twin.rs), `[tmp] exit`
(child.rs). Session additions, all `[tmp]`-tagged: the `[tmp] w4ret` frame
dump at the wait4 arm (main.rs), the `[tmp] sw to=`/top print in `switch_to`
(exec_twin.rs), the `[tmp] fault ctx` print in `fault_trap` (main.rs).
`[TRAMP-MISMATCH]` is pre-existing (akuma-exec). The dispatch-head `sc` print
was added and removed within the session.

## wc -l

Start of attempt 3: **7530** (`taoxichuan/src/*.rs`, first commit `9ca14fd7`).
End: **7618** (after `983dc1c5`).
