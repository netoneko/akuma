# Taoxichuan stage 8, M4 session pg — RESULT (the exec'd shell's writev loop: root-caused and fixed; F1 green)

## Premise rows (written before the first edit; final after the last boot)

| # | premise | verdict | evidence |
|---|---|---|---|
| 1 | tree is `15cc24e0` and builds | **HOLDS** | `git log --oneline -3` = 15cc24e0/bc5c57e6/96b7948b on `taoxichuan-s4`; `wc -l taoxichuan/src/*.rs` = 7870 at start; `kbuild -p taoxichuan -j 4 --features no-tests` exit 0 (kbuild-pg0.log) |
| 2 | the loop is a writev whose fd 1 is not served by a pipe arm | **WRONG in both halves — the disassembly was right, the "fd 1" guess was not** | objdump of the image busybox: `0x4ce9f` = nr 20 writev, 2 iovecs; loop-back only on `rax>=0 && rax!=rem` ⇒ the kernel answered **Ok(0)** forever (EAGAIN would set F_ERR and leave). The fd was **2, not 1**, and the answer was 0 for a reason no arm could produce: the kernel read the iovec lens through the WRONG address space (`l0=0 l1=0` while the live iovecs were non-empty) |
| 3 | the amd64 kernel serves the same case through a shared crate | **HOLDS** | amd64 AND aarch64 route write/writev through `akuma-syscalls-glue` (fs.rs:1015/1506); ssh exec-child stdout = the channel-bound Stdout arm (fs.rs:1086): `write_bounded` + `check_set_writer` + park — never 0, never EAGAIN. Decision crates: akuma-syscalls-glue + akuma-exec. Archive rows: `AKUMA_AMD64_EPOLLET_REARM_KOT_WEDGE.md` §6.1 (OPEN socket-flavor writev spin, same Ok(0) mechanism reading), `docs/README.md:205` (writev short-write splice, fixed), `AKUMA_SELF_HOSTING_AMD64.md:2270/:511` (the kot spin; musl flushes through writev even for one buffer). No pipe/console-flavor row existed |
| 4 | `REAL_FORK` true and the `[tmp]` prints in the tree | **HOLDS** (start state) | child.rs:434 `REAL_FORK: bool = true`; 9 `[tmp]` prints (child ×4, exec_twin ×2, main ×3); the `sw to=` pair was the only rotation-scale one. **Still true at session end** — see "Remove before final" |

## The two root causes (the session's headline)

**1. The writev spin — `switch_to` never maintained `TASK_SLOT`.** The
dispatcher's `task()` is `TASKS[TASK_SLOT]` — every syscall's fd table,
root_pa and regions. Crossings set it (`twin_enter_user`), but a park-resume
of another task followed by a tick switch back left it naming the resumer's
record: the fork child then ran its `__stdio_write` writev with ANOTHER task's
`root_pa`, the gather read zeros out of the wrong address space, and musl —
whose loop-back requires `rax>=0 && rax!=rem` — spun forever on the honest 0
answer. Boot pg-2's instrument nailed it: `nr=20 fd=2 a3=2 l0=0x0 l1=0x0
br=11 dlen=0 ans=0x0`, ~97M laps at 8k exits/s, zero faults, machine alive.
Fix: `switch_to` sets `TASK_SLOT = sched_task_slot_of(to)` beside the CR3 it
loads (idle, tid ≥ NSLOTS, excluded — it leaves CR3 too). Same discipline as
session pe's shell-resume fix (exec_twin.rs:70-74 doc).

**2. The hung sessions after a fork-bearing exec — the M2 exec arm clobbered
the spawn crossing's saved sshd frame.** `child_enter(root, ef, save_ptr(cur))`
stores the caller's frame INTO the spawn's save slot. Every gate command is
`sh -c` wrapped, and ash execs the last command in place, so the inner exec
overwrote sshd's resume frame; the image's `child_exit` then `exit_cross`ed
into the exec's dead save — sshd never resumed: no drain, no
`[SSH] Exit status`, a session that hangs with the bytes already sitting in
the drained buffer (boot pg-4 console: `[TICK post]` then silence). The exec's
save is write-only by construction (execve never returns), so one global
`EXEC_SAVE` scratch now serves both exec call sites.

**3. (supporting fix) the real fork child's stdout went to a buffer nobody
drains.** `bind_child_task` left the child's cross record at its
`CrossSave::new()` defaults — `out_slot: 0` — so `child_write` landed `cat`'s
bytes in slot 0's buffer and answered success (drop+lie). It now copies the
shell's `out_slot` and stamps `depth = shell+1`; `fork_arm_real` admits depth 2
(grandfork's rungs, the `( ls; ls )` class this slice exists to fix).

## Gates (final)

Verdict rule held throughout: the console `[SSH] Exit status` line, never a
runner UNKNOWN taken as green or red.

- **F1: PASS — boot pg-6 (`1f6152ef`) and boot pg-7 (`025a2748`), stable.**
  `sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` → console
  `[SSH] Exit status: 0`, client bytes `x\n2 /tmp/f\n` (od -c, exact), zero
  `Fault|PANIC|sig!` since the boot mark, ticks advancing. Boot pg-6's console
  shows the whole chain live: `bind tid=1 shell=2 out=2 depth=2` →
  `cw cur=1 out=2 len=2` (cat) → `w4ret rax=0x5` (reaped) → wc's 9-byte
  `br=12` write → buffer 2→10→11 → `rco avail=11` drained → exit 0.
- **PK1: NOT closed.** Boot pg-6 died guest-side: `sh: yes: not found` (the
  image had no `yes` applet link; three fork children, park/wake and reaps all
  ran green — exit 127). The bake now adds `/bin/yes -> busybox` (committed);
  boot pg-7's PK1 exec ran with the link present but its console slice cut
  before the verdict window — payload empty, verdict UNKNOWN. ONE clean boot
  answers it.
- **P1-P5: UNKNOWN this boot** (no console exit lines in the runner's slices —
  the ~250 KB log rotated under the battery; pf's boots had them green through
  the M2 path, and no fault fired). ONE clean boot re-runs them.
- **PK2: not attempted — pf's "cheap" premise was wrong**: the probe is gated
  on `AKUMA_DEVROOT_BAKE_FORKPROBE`, never set, and no grandfork binary is
  built in-tree; wiring it needs a musl cross-build of
  `userspace/forktest/c_stress/grandfork.c` plus the env plumbing. Handed over.

## Hooks table (real / no-op / stub — end of session)

- pipe `read_arm`/`write_arm`(+`write_arm_gathered`)/`release`: real park/wake
  (fix B, pf) — untouched, host tests 24/24.
- `switch_to`: now ALSO maintains `TASK_SLOT` (this session's fix 1) — CR3,
  syscall band, trap stack, dispatcher record: one switch, all four in step.
- `bind_child_task`: stamps out_slot + depth (fix 3).
- M2 exec arm + `exec_real_task`: save into `EXEC_SAVE` (fix 2).
- Still stub/no-op (honest, carried): sock.rs read/poll arms busy-wait; socket
  writev retries without backpressure; `on_process_exit` no-op; child-AS
  frames leaked per fork; M2 blocking fork still selectable via `REAL_FORK`.
- `[SCHED] WARNING: yield_now with IRQs masked` fires on every real child's
  exit path (tid=1/4 lr=0x0) — pre-existing print, pre-existing condition,
  not chased this session (zero faults beside it).

## Pulled / extracted / written-new

- **Pulled (dependencies, untouched):** nothing new this session. Re-confirmed
  by reading: `akuma-syscalls-glue` (fs.rs Stdout/Stderr arm — the other
  kernels' never-answer-0 contract), `akuma-exec` (ProcessChannel,
  `fork_process`'s bind hook), `akuma_net` (socket_send — EAGAIN on full, no
  Ok(0) path found), `akuma-syscalls-eventfd` (write = 8/errno). Composition
  rule: every fix is taoxichuan glue over existing crate contracts; the
  amd64-equivalent channel arm (akuma-syscalls-glue) was NOT pulled — it is
  welded to the glue stack (akuma-exec threading), out of this slice's budget,
  and the kernels' difference did not explain the loop anyway (the plan's §2
  finding, written before the first boot).
- **Written new (taoxichuan glue):** `switch_to`'s TASK_SLOT maintenance;
  `EXEC_SAVE` scratch + both exec call sites; `bind_child_task`'s
  out_slot/depth stamp; `fork_arm_real`'s depth-2 gate; the `wtrace`/
  `WBR`/`WLEN` write-path instrument + the wd (boot pg-1/2, to be stripped
  with the rest); devroot-ensure's `/bin/yes` bake.
- **amd64 keeps building:** untouched (`cargo check -p akuma-amd64` side not
  run — no amd64 file or new crate was touched; default-members unchanged).

## Remove before final (the list for the NEXT session; NOT stripped — gates not fully green)

All `[tmp]`: exec_twin `sw to=`/`enter` already gone (removed pre-boot per
PLAN-pg §5). Remaining: main.rs `wtrace`+`WBR`/`WLEN`+the dispatch wrapper
(boot pg-1/2 instrument), the wd line (main.rs ~385 — keep deliberately? it is
the liveness evidence at ~2.6 s), `[tmp] w4ret` frame dump (main.rs ~3029),
`[tmp] fault ctx` (main.rs ~274); child.rs `[tmp] exit`, `[tmp] fork child
pid=`, `[tmp] w4park`, `[tmp] w4resume`, `[tmp] bind`, `[tmp] cw`, `[tmp] rco`;
main.rs `[TICK pre]/[TICK post]` (child.rs:1038/1238 — keep? cheap, once per
exec), the `[wv]`/`[w]`/`[o]`/`[sc]` probe prints, the `[raw]` tsc/dhcp lines,
`[TRAMP-MISMATCH]` (pre-existing, akuma-exec), `[SCHED] WARNING` (pre-existing).
`REAL_FORK` stays `true` by default ✓ (already is).

## wc -l

Start: **7870** (`taoxichuan/src/*.rs`, `15cc24e0`). End: **8029**
(`8ac51685`; the yes bake lives in userspace/ext2probe, outside taoxichuan/)
— +159 lines: the fixes (~60) and the instruments (~100, stripped next
session).

## What is next (the hand-off)

1. **One clean boot, full battery** (F1, PK1, P1-P5; PK2 only after the
   grandfork bake is wired): every verdict this session could not pin —
   PK1's first yes-baked run, the P-gates' lost exit lines — is a
   slice-vs-rotation question, not a known-fail. If the console races again,
   drop the wd line to 4 s before the boot.
2. **PK2's bake**: build `grandfork.c` for the guest (static musl), set
   `AKUMA_DEVROOT_BAKE_FORKPROBE` in the boot env, keep the depth-2 fork gate
   in mind (rungs 2-5 fork from a forked child — the bind stamps depth 3 for
   the third generation, which `fork_arm_real` REFUSES; either flatten the
   gate to depth ≤ 3 or expect rung 2 to pass and 3+ to EPERM — read the
   probe's rung expectations first).
3. **Then the `[tmp]` strip** (the list above), host clippy on touched crates,
   `wc -l`, final commit, push.
4. The tick switch's TASK_SLOT discipline is the pattern to audit anywhere
   else a per-task record hides behind a static index: `grep -n
   "read_volatile(&raw const TASK_SLOT)" taoxichuan/src/main.rs` is the only
   reader, and `switch_to` is now its only switch-path writer.

## Session-notes log (appended; never rewritten)

- 21:4x — premise checks, docs pass, busybox disassembly (the loop needs
  Ok(0)), every WRITEV path read statically — none answers 0; PLAN-pg committed
  (its §2 finding: the kernels' difference does NOT explain the loop).
- 22:0x — boot pg-1 (instrument: dispatch-return trace): the spinner's writev
  is answered 0 BY DISPATCH (fd=2, iovcnt=2, ~97M laps) ⇒ delivery not
  implicated; kind byte 45='-' exposed the post-call register-clobber mistake —
  boot pg-2 re-instrumented with pre-call iov lens + arm branch stencils.
- 22:5x — boot pg-2: `l0=0 l1=0 br=11 dlen=0 ans=0` with live non-empty iovecs
  ⇒ the kernel read the WRONG address space ⇒ TASK_SLOT staleness on the tick
  switch (statically confirmed: switch_to maintained CR3/band/trap-stack but
  not the dispatcher's record). Fix + boot pg-3: the loop is GONE — every gate
  exec now completes (exit lines!) but client bytes empty.
- 23:1x — boot pg-4 (out_slot/depth bind fix): F1 chain visible in-console
  (bind/cw/w4ret/rco all correct) yet the session hung after `[TICK post]` —
  the inner-exec save clobber (spawn's sshd frame overwritten by the exec's
  dead frame). Fix: EXEC_SAVE scratch. Boot pg-5 (prints): the live F1 window
  captured the full green guest-side chain (cat 2 bytes + wc 9 bytes in the
  drained buffer) and the hang.
- 23:5x — boot pg-6 (EXEC_SAVE): **F1 PASS, byte-perfect.** PK1: `yes: not
  found` (image bake gap; the fork/park/wake chain green around it). Boot
  pg-7 (yes bake): F1 PASS again; PK1 ran with the link (slice cut, verdict
  open); P1-P5 slices lost their exit lines; PK2 not attempted (no probe
  binary, no bake env — pf's cheap premise wrong).
- Ops: vm-serve-mc stopped before vm-dev for every boot; both herd confs
  written by boot_dev.sh; install md5 verified every boot; restore at end of
  session (see the tail of this log).
