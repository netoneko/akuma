# Taoxichuan stage 8, M4 session pe — PLAN (slice 3 again: debug the new path, then make it pass)

Branch `taoxichuan-s4`, start head `33378e5d` (verified: `git log` ends there).
Written and committed **before** any build or boot. Phase 2 updates this file in
place with dated notes; changes are additive, never silent rewrites.

## 0. Box-reset finding (one line, per the session note)

The box was hard-reset ~12:30 UTC before this session's first call (`uptime` 59 m
at 13:29); nothing of the earlier attempt survives (no commit, no stale build
locks found, target/ holds only the Oct 20 baseline) — the reset cost nothing
because nothing was committed, which is itself the argument for this file
landing before the first build.

## 1. What is known (facts with evidence, kept apart from guesses)

From `RESULT-pd.md` (boots 2-3, tree `33378e5d`), `RESULT-pc.md` (slices 1-2),
and this session's source reads (file:line checked in the tree):

- **K1** — The park/switch/enter chain WORKS: boot 3 shows shell `wait4` parked
  → real `switch_to` ran → child published, got its own band + CR3 → entered
  ring 3 at the fork return (`[tmp] enter tid=1 rip=0x4c7a31`).
  (RESULT-pd "Gates", "What works".)
- **K2** — Deterministic kernel fault after that entry: `[TICK post] n=5538`
  then `[Fault] PageFault rip=0x0 rsp=0xffff8000006d4898 cr2=0x0 code=0x10`.
  No `[tmp] exit`, no `[forkp] refused`, no ring-3 fault, no `[SSH] Exit
  status`. Same signature on boots 2 and 3. code=0x10 = supervisor
  **instruction fetch** of a non-present page: the kernel *jumped to 0* while
  executing on a syscall band.
- **K3** — `[TICK post]` is printed in exactly one place: `child_exit`
  (`taoxichuan/src/child.rs:1157`). So an `exit_group` **landed in the
  dispatcher** after the child's ring-3 entry — somebody exited.
- **K4** — The dispatcher's EXIT arm routes `if child::nested_active() { child_exit } else if child::is_real_task() { task_exit_real }` (`main.rs:1791-1799`),
  and `nested_active()` is `current_slot() != NO_SLOT` (`child.rs:247-249`) —
  **true for the fork child too** (its slot is 1..NSLOTS). A real child's exit
  therefore takes the **M2 coroutine path**.
- **K5** — The M2 path is meaningless for a published task: `bind_child_task`
  (`child.rs:431-478`) stamps the SpawnSlot, stacks, and task record but never
  writes the child's `CrossSave`; `exit_cross` then restores rsp from that
  never-initialized save and `ret`s — popping a zero return address from a
  fresh per-slot band → K2's `rip=0` on a band rsp. (Popped-garbage fits
  rsp=…4898 = a valid band address after the pop.)
- **K6** — "No exec failure prints" is NOT evidence the child hung: F1's first
  pipeline member is `echo x > /tmp/f`, and `echo` is an **ash builtin** — the
  forked child runs builtin-echo in-process (its open/write/close syscalls are
  served silently by arms that print nothing) and calls `exit_group` without
  ever exec'ing. pd's reading "its execve never reaches its failure prints"
  assumed a non-builtin first command.
- **K7** — `enter_user` (pd hypothesis (a)) checks out on paper: the naked body's
  16 loads (`crates/akuma-ring3-amd64/src/lib.rs:754-780`) match the
  `EntryFrame` repr(C) layout at 0x00..0x78 field-for-field, `rcx`=rip,
  `r11`=rflags, rsp last, frame pointer spent last — the same order as
  `child_enter`'s proven inline copy (`child.rs:1112`), which differs only in
  saving the parent phase first. K1 (the child entered and ran) corroborates.
- **K8** — `wait4_real` (pd hypothesis (b)) has no early-return path that could
  end the shell's crossing: it loops on `schedule_blocking(u64::MAX)` with the
  scan deciding (`child.rs:1487-1539`); the only waker for a parked shell is
  `task_exit_real` (`waiter` wake), which K3/K4 show never ran. A shell that
  *had* exited would take `child_exit` with a **valid** M2 crossing (spawn_arm
  wrote it) — no rip=0. So the shell stayed parked; the exiting task was the child.
- **K9** — The pipe arms drop every `Wakes` the crate returns (`_wakes` at
  `pipes.rs:104,129,144,174`) and never park: an empty pipe read answers 0
  ("drained, or EOF — both read as 0"), a full-pipe write answers EAGAIN.
  PK1 (`yes | head | wc`) cannot work through them: `yes` spins on EAGAIN, and
  when `head` exits, nothing wakes anyone. The crate already carries the whole
  mechanism (`akuma-pipes`: `check_set_reader`/`check_set_writer` one-step
  register-or-proceed, `Wakes.fire`), and its `close_read` doc names
  `busybox yes | busybox head -n 1` as the victim of exactly the dropped wake.
- **K10** — Threading facts: on x86_64 the crate asks `X86ArchHooks::current_slot()`
  for "who am I" (no separate register), `schedule_blocking` parks that slot,
  `ThreadWaker::wake()` is the sticky-flag + READY handoff whose race is
  host-tested (`park_wake_race_tests`, `crates/akuma-threading/src/lib.rs:7476+`),
  and `wake_handle_for_thread(tid)`/`wake_by_handle` are the generation-validated
  wake pair. The park discipline (publish-WAITING + take-flag as one unit) is
  the load-bearing rule (`docs/reference/subsystems/scheduler.md` § park/wake).
- **K11** — `waiter` in the slot is a bare tid, woken via
  `get_waker_for_thread(waiter)` — that mints the handle **at wake time** with
  the slot's current generation. Safe while the waiter stays parked (a WAITING
  slot is not claimable, so it cannot be recycled under us), which holds for
  every gate this session. Noted as a bounded staleness, not a bug to chase now.
- **K12** — musl ash on this image forks with plain `fork` (57), not `CLONE`:
  boot 3's `[tmp] enter tid=1` proves `fork_arm` took `fork_arm_real`
  (`fork_arm` routes CLONE to the M2 arm; 57/58 to the real arm). The write
  arms' `nested_active() && fd == 1` fallback is CORRECT for real children too —
  an unbound fd 1 *is* the ssh session for every descendant of the shell, and
  bound fds (dup2'd pipe/file) resolve through the bound arms first
  (`main.rs:1650-1660`, `1755-1790`).
- **K13** — Premise-2 build-out is proven (pd boot 1): registration, fork memory
  pass, publish, context capture all real; `fork_process` runs clean end-to-end.

## 2. Hypotheses (cheapest confirmation first; cost per bit, cheapest first)

| # | hypothesis | predicts | cheapest confirm/kill | cost |
|---|---|---|---|---|
| H1 | **The real child's `exit_group` is routed into M2 `child_exit`** by `nested_active()` (K4), and `child_exit`'s `exit_cross` into a never-initialized `CrossSave` is the rip=0 (K5) | Reordering the EXIT arm (real-task test first) makes F1 pass end-to-end: `[tmp] enter` → `[tmp] exit … wake=…` → shell wakes, reaps, next command | Paper re-derivation (done — §1); then Fix A's own gate run: F1 + P1-P5 + warmup execs on one boot | ~1 build + 1 boot (already planned) |
| H1-outcome (16:0x, attempt 3) | **CONFIRMED and exceeded**: the reorder alone was necessary but its test was not a discriminator — `is_real_task()` (used/running/current_slot) is true for M2 spawn children too (spawn_arm stamps the same triple; NEXT_SLOT starts at 0, so spawn #1 hides behind the `cur==0` guard and spawn #2 exposes the flaw). Fix A wedged every boot at the second ssh spawn (boot A console: `[SCHED] WARNING: yield_now with IRQs masked tid=1 lr=0x0`, then idle spin). Fix A' (`ffb09617`) adds the `SpawnSlot.real` bind bit; baseline boot B then ran F1+P1-P5 green (premise 5). The rip=0 fix itself is untested live until boot C | boot C (flag on): F1 exact bytes + `[tmp] enter`/`[tmp] exit` pairs + no rip=0 | — |
| H2 | `enter_user` subtly broken (pd's (a)) — child spins in ring 3, no syscall ever | Child never reaches exit; no `[tmp] exit` even after Fix A | **KILLED on paper** (K7: offsets/machinery verified; K1 shows entry worked). Re-open only if a post-Fix-A boot still shows no `[tmp] exit`; then pd's disassembly step (objdump the naked body vs `child_enter`) — 0 boots | 0 (already spent) |
| H3 | `wait4_real` returns early; shell exits while child lives (pd's (b)) | `[TICK post]` is the SHELL's exit; shell's crossing would be valid (no rip=0) — contradicts K2 | **KILLED on paper** (K8) | 0 (already spent) |
| H4 | Park/wake machinery (schedule_blocking + ThreadWaker + finish/pick) is broken beyond what boot 3 proved | Shell never resumes even with a correct wake; F1 stalls after `[tmp] exit` | Host: `cargo test -p akuma-threading --target $HOST` (`park_wake_race_tests` = the invariant); live: boot 1 after Fix A | ~2 calls |
| H5 | Pipe arms' dropped wakes/parks are the only PK1 blocker (K9) | Adding park/wake + the SIGPIPE stand-in makes PK1 pass (`20000\n`) and P1-P5 stay green | Fix B's own gate run: PK1 + P1-P5 on one boot | ~1 build + 1 boot (already planned) |
| H6 | `yes`/`head`/`wc` missing from the image (pd premise 3, untested) | PK1's first exec fails with ENOENT | Guest `busybox --list \| grep -x "yes\|head\|wc"` in the premise boot's first exec | 1 exec inside a planned boot |

H1 is confirmed or killed by the boot that Fix A needs anyway; H2/H3 are dead
on paper and cost nothing more unless F1 fails again. That is the whole reason
the session starts with Fix A, not with probes.

## 3. The fix sequence (order, stop conditions)

> **Phase-2 note, 14:55 UTC (attempt 3):** step 1's boot already ran once and
> was VOID — the attempt booted a repo-relative `target/...` kernel from
> Oct 27 14:12 (pre-61cc5e71, pre-pd; P1-P5 all failed `[sc] unhandled ? (22)`,
> the exact pre-pipe-arm signature pipes.rs:42 documents). Step 1 restarts
> here with the default /root/ktarget artifact. Attempt-2's second boot
> (13:58, correct `e96a57dc` artifact) never served SSH — handshake stall from
> the first connection, guest console lost to the 14:07 truncation; retrying
> on the step-1 boot counts as the once-per-tree retry. Details:
> RESULT-pe.md "Attempt-2 post-mortem".
>
> **Phase-2 note, 18:1x UTC (attempt 3, close-out):** steps 1-2 discharged as
> far as the mechanism goes — Fix A' + Fix C landed (commits ffb09617,
> ecaca7b1) and the NEW path's full lifecycle (fork → park → child enter/run/
> exit → wake → switch back → reap) is console-proven on three boots with
> zero faults. Step 2's stop condition (F1 exit 0 + exact bytes) is NOT met:
> the ssh channel stalls after ~2 clean connections per boot and an exec whose
> output lands on a dead channel writev-loops the bridge (no backpressure —
> the K9 class) and kills the guest. Next session: Fix B (pipe park/wake,
> which the channel evidence says the sshd's own event path rides), then the
> battery. P1-P5/PK1/PK2 through the new path: not reached. PK2 additionally
> needs forkprobe injected into the image (devroot-ensure).

0. **Commit this plan.** Then premise rows into RESULT-pe.md (before any edit);
   host checks: `cargo test -p akuma-threading --target $HOST`; `kbuild -p
   taoxichuan -j 4 --features no-tests` (premise 1). Stop: tests pass, build exits 0.
1. **Boot 1 — premise + baseline, `REAL_FORK` off**: premise 5 (M2 scaffold
   still green: F1 + P1-P5 on one boot) + premise 3 (`busybox --list` exec) +
   premise 4 (`userspace-layout.md` re-read: forkprobe is libakuma/raw-syscall
   x86_64 static — speaks this kernel's ABI). Stop: F1 exact `x\n2 /tmp/f\n`,
   P1-P5 PASS, zero new `Fault|PANIC|sig!`. Commit Fix A (code may be in the
   tree but flag off) before the boot.
2. **Boot 2 — Fix A, `REAL_FORK` on** (one-line reorder in the EXIT arm:
   `is_real_task()` before `nested_active()`): F1 through the NEW path + P1-P5
   through the new path. Stop: F1 exit 0 + exact bytes; `[tmp] enter`/`[tmp]
   exit` pairs in the console for each pipeline member; no rip=0; P1-P5 PASS.
   If F1 fails with no `[tmp] exit` → H2 reopens (disassembly step, 0 boots,
   then replan per the brief's 2-boot rule). Push `litter` (WIP note ok) —
   this boot teaches something whatever the outcome.
3. **Fix B — pipe park/wake** (`pipes.rs` + `write_arm` SIGPIPE stand-in):
   - read: `check_set_reader(id, tid, ())` → false ⇒ save `user_rsp_stash`,
     `schedule_blocking(u64::MAX)`, restore stash, loop.
   - write: `check_set_writer` same shape; `Wrote(0)`+full ⇒ park; `BrokenPipe`
     ⇒ EPIPE, and **for a real task only** the SIGPIPE-default stand-in:
     terminate via `task_exit_real(13<<8)` (documented stand-in; M2 path keeps
     returning EPIPE so premise 5/P5 are untouched).
   - fire every returned `Wakes` after the op (`get_waker_for_thread(tid).wake()`),
     including `release()`'s close arms (the crate's close_read doc: without it
     a parked writer never learns the pipe broke).
   Stop condition: host clippy clean; then boot 3.
4. **Boot 3 — full gates**: F1, P1-P5, PK1 (`yes | head -n 20000 | wc -l` →
   `20000\n`, exit 0; fall back to the `while true; do echo y; done` form only
   if `yes` is missing per H6), PK2 (`userspace/forkprobe` build + run; its own
   PASS line; zero `Fault|PANIC|sig!` after this boot's start line; ticks
   advance). Stop: all green in one clean boot.
5. **Cleanup + final**: remove `[tmp]` prints (listed in RESULT "Remove before
   final"), host clippy on touched crates, final wc -l, RESULT-pe.md complete,
   push `litter taoxichuan-s4`, ops restore (vm-dev stop/disable both confs,
   stage4 conf from s6, vm-serve-mc start, mirror.sh stop, `herd status`).
   Stop: restored state verified before the last line.

Boot budget: 6 max. Boots 1-3 are planned; 4-6 are contingency. If any step
yields no new evidence after 2 boots, the plan says so here and re-plans from
what is known (brief's rule).

Composition rule accounting (for RESULT): Fix A = written-new twin decision
(one-line routing fix in `taoxichuan/src/main.rs`; amd64 has no twin — its exit
routing lives in glue/crate paths this kernel does not pull). Fix B = **pull**
(`akuma-pipes` already owns the mechanism; the arms consume
`check_set_reader`/`check_set_writer`/`Wakes` — no crate change, no extraction,
no copying from `amd64/`), plus the stand-in written-new (no signals exist).
`cargo check -p akuma-amd64 --target x86_64-unknown-none --features no-tests`
must stay green; `amd64/` untouched this session.

## 4. What from the amd64 kernel's history applies

- **`AKUMA_AMD64_RING3_SEAM_SLICE2.md` §3.1** — "wired but unreachable is the
  shape that rots, and the failure it rots into is a child resuming on a
  register file that is subtly not its parent's — which reads as a userspace
  bug a long way from here." Exactly H2's shape; the doc's answer (a boot test
  for the unreachable half) is what slice 3's boots are. Also the source of
  `write_user_context`/`UserCtx` the twin re-implements.
- **`CURRENT_TRAP_FRAME_STALE_ON_EXIT.md` + `AKUMA_STALE_TID_ZOMBIE_SLOT.md` +
  `STALE_THREAD_SLOT_KILL.md`** — the exit-path class: a task's death must
  leave no context that a later dispatch can resume or a later wake can hit.
  Our K4/K5 is that class on the new path (a dead-shape M2 context left
  resumable); the bare-tid `waiter` (K11) is the slot-reuse half, bounded this
  session.
- **`CONTEXT_SWITCH_BUGS.md` / `CONTEXT_SWITCH_FIX.md` /
  `AKUMA_THREADING_X86_SWITCH.md`** — the switch contract (callee-saved + stack
  + AS root move together; a switch to a never-seeded context reads as a jump
  to garbage). K1 proves the twin's `switch_to` + crate asm land; H4's host
  test re-checks the park/wake invariant.
- **`AMD64_SYSCALL_ABI_REGISTER_CLOBBER.md`** — rcx/r11 are syscall's scratch;
  any enter/return crossing must treat them as targets, not state. Verified in
  K7 (`enter_user` loads rcx/r11 last, from the frame).
- **`AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md` /
  `AKUMA_AMD64_SWITCH_FREED_CR3_UAF.md`** — switch-shaped page faults: read the
  fault's rsp against the band map first (K2's rsp names a per-slot band, which
  is what pinned the fault to the exit path, not the entry).
- **`SYSCALL_BLOCKING.md`, `WAIT_QUEUES.md`,
  `runbooks/debug-futex-lost-wakeup.md`** — the blocking-syscall pattern (wait
  queue + waker + park in that order) and the lost-wakeup signature; Fix B is
  the pipe arm of exactly that pattern, and the crate's `check_set_*` one-step
  form is the anti-TOCTOU lesson those docs cost.
- **`AKUMA_PIPES_EXTRACTION.md` / `PIPE_TABLE_UNIFICATION.md`** (titles; the
  crate doc-strings carry the specifics) — the "wakes are returned, never
  performed" rule exists precisely so a second kernel can own the firing;
  taoxichuan's arms are the second owner amd64's `set_wake_sink` anticipated.
- **`docs/reference/subsystems/scheduler.md` § park/wake handshake** — the
  publish/take invariant + `park_wake_race_tests` as the host regression gate
  (H4). `docs/runbooks/trace-failing-syscalls-amd64.md` — strace-style prints
  are the fallback evidence if boot 2 fails (Fix C in §3).
- Symptom-first rows already run by pd for the rip=0 shape ("no row matches
  jump-to-0 from a syscall band after a real switch") — this plan does not
  re-litigate them; the matching row lands in RESULT-pe.md.

## Session-notes log (appended, never rewritten)

- 2026-10-28 (plan time): H2/H3 from RESULT-pd killed on paper (K6-K8); root
  cause candidate is H1 (exit-arm routing + uninitialized CrossSave). No code
  read contradicted K1-K13.
