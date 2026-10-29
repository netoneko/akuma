# Taoxichuan stage 8, M4 session pg — PLAN (the exec'd shell's writev loop; slice 3 close: F1/P1-P5/PK1 with REAL_FORK on)

Session pf proved: pipe park/wake (fix B) real, timer preemption real and
SMP-ready, and — the big one — the old "~2 connections per boot" channel stall
was never the channel: it is the exec'd task spinning in musl's `__stdio_write`
writev retry loop (pf-8: machine alive, zero faults, `rip=0x4ceaa1 slot=2
tid=2`, io≈15456 exits/s, one thread READY never progressing). What is left is
that loop, then the gates with `REAL_FORK` on, then the `[tmp]` strip.

## 1. What is known (facts with evidence, kept apart from guesses)

- **F1 — the spinner is musl `__stdio_write`, and the kernel must be answering
  Ok(0).** Disassembly of the image busybox (/tmp/refbin/busybox, pf's md5)
  around `0x4ceaa1`: the syscall at `0x4ce9f` has nr=20 (writev), iovcnt=2
  (`r13d=0x14`, `r15d=2` — musl's wbase/wpos + buf pair); the loop-back
  `jmp 0x4cea92` at `0x4ceb11` is taken only when `rax >= 0 && rax != rem`;
  the negative path (`0x4ceacc`) sets F_ERR (`orl $0x20,(%rbx)`) and RETURNS.
  So an endless loop here means the writev is answered **0, success, no
  error**, over and over. EAGAIN or EFAULT would end the loop. (Fact — from
  the instruction bytes, not from musl memory.)
- **F2 — the new WRITEV arm has no Ok(0) path (static read, whole arm
  walked).** main.rs WRITEV (1779): pipe arm → `write_arm_gathered` (returns
  done>0, EAGAIN, EPIPE, or parks a REAL task); socket arm → `sock::
  writev_socket` → `akuma_net::socket::socket_send` (nonblock !can_send →
  EAGAIN; `send_slice` accepts ≥1 byte when can_send); file arm →
  `writev_file_arm_inner` (EIO or the written total; Ok(0) only for an empty
  gather); eventfd → 8/errno; the fd1/2 fallback → `child_write`/`emit` per
  iovec, answering total or EFAULT. `child_write` at OUT_CAP **drops the bytes
  and still answers success** (child.rs:289) — a lie, but not a spin. ⇒ The
  loop's answer does not come from any path I have read. **This is what the
  first boot's instrument is for** — not a guess to code against.
- **F3 — the plumbing (taoxichuan).** userspace/sshd is ring-3 on every
  kernel; an exec session is `spawn(sh, ["-c", cmd])` in pipe mode; the child's
  fd 1 is `OpenFile::ChildOut(slot)` (kernel-side out buffer; sshd drains the
  *shell's* buffer via `bridge_process(pid, stdout_fd)` on its fd≥3 handle —
  FLOW.md). `fork_process` puts the fork child in slot 2 and `fork_copy`s the
  shell's table, so the fork child (cat/wc/echo in F1) inherits
  `ChildOut` on fd 1. `nested_active()` is true for ANY slotted task
  (child.rs:259), so both shell and fork child route fd 1 to `child_write` —
  into **their own slot's** out buffer. The bridge drains only the shell's.
  Nobody drains the fork child's.
- **F4 — the other kernels do not answer 0, ever (the crate-level answer).**
  amd64 and aarch64 route write/writev through `akuma-syscalls-glue`
  (fs.rs sys_write:1015 / sys_writev:1506). An ssh exec child's stdout is a
  channel-bound `Stdout` descriptor; the Stdout/Stderr arm (fs.rs:1086) does
  `ch.write_bounded()` + `check_set_writer(tid)` + `park_indefinitely()` —
  real backpressure: the syscall returns only whole-progress or an errno
  (EINTR on interrupt). musl never sees 0 and never sees EAGAIN. Decision
  crates: **akuma-syscalls-glue** + **akuma-exec** (`ProcessChannel`). amd64's
  own `sys_writev` (usermode.rs:2570) loops `sys_write` per iovec and returns
  the running total; a first `sys_write` of 0 would yield 0 — the kernel
  guards against it by never answering 0 on the channel/console arms.
- **F5 — the archive rows for the symptom words** (`writev`, `__stdio_write`,
  short write, EAGAIN-on-pipe/console, ring-3 write spin):
  - `AKUMA_AMD64_EPOLLET_REARM_KOT_WEDGE.md` §6.1 — OPEN: a metal worker spins
    on `writev` (~650 k/s), socket flavor, candidates "returns 0 in some case
    a caller loops on" or EAGAIN-with-re-arm. Same loop shape, different fd
    kind; still open there; does not decide our case but CONFIRMS the
    Ok(0)-loops-musl mechanism was the reading on amd64 too.
  - `docs/README.md:205` — writev short-write SPLICE (socket corruption,
    FIXED 2026-08-16), archive `WRITEV_SHORT_WRITE_SPLICE.md`: vectored calls
    must stop at the first short write. Guidance for any writev rewrite, not
    this loop.
  - `AKUMA_SELF_HOSTING_AMD64.md:2270` — "spinning on writev at ~650 k calls/s"
    (the same kot wedge from the self-hosting log), and :511-527 — writev had
    to be added because "musl flushes through writev even for a single buffer".
  - No row names a pipe/console writev spin on THIS kernel. No row for
    `__stdio_write` by name.
- **F6 — what pf proved about the surroundings (from RESULT-pf.md):** fix B
  pipe park/wake green (host tests 24/24); preemption live and SMP-ready; the
  single-command exec completes `[SSH] Exit status: 0`; the wd prints every
  ~2.6 s (main.rs:385) — NOT rotation-scale; the rotation driver is
  `exec_twin.rs:118` `[tmp] sw to=`+top — printed from EVERY switch, ~2000/s
  under the tick ping-pong, ~8 KB/s, rotating the 250 KB log every ~30 s.

## 2. The other kernels first — does the difference explain the loop?

**No, and that is a finding, not a gap.** The amd64/aarch64 answer for the very
same busybox on the very same sshd exec session is: never answer 0, park with
backpressure on the channel (akuma-syscalls-glue + akuma-exec). Taoxichuan's
arms as read today: never answer 0 either (pipe arm EAGAIN/park; child_write
drop+success; emit console). Since the disassembly (F1) says the kernel DOES
answer 0 to the spinning task, at least one of the static reads in F2/F3 is
wrong about the running system — most plausibly the fd kind at the spinner is
not what the boot probe's completed exec suggests it should be, or the answer
is produced but not delivered (a tick-switch rax-delivery defect would look
identical from ring 3: writev "returned" 0 forever). **Therefore: no fix is
coded yet; boot 1 is an instrumented boot that names (nr, fd, kind byte,
iovcnt, answer) for every writev/write of the slotted tasks.** The one cheap
static gap left — `OpenFile::ChildOut` missing from the WRITE/WRITEV kind
matches (falls to the fallback) — is noted as H1 below.

## 3. Premise rows (final table lives in RESULT-pg.md)

1. Tree `15cc24e0` builds — checked: `git log --oneline -3` + kbuild exit code.
2. The loop is a writev whose fd 1 is not served by a pipe arm — writev part
   CONFIRMED by disassembly; "fd 1" and "not a pipe arm" are unproven; the
   instrument decides.
3. amd64 serves the same case through a shared crate — HOLDS (akuma-glue
   channel arm, F4).
4. REAL_FORK true and the `[tmp]` prints are in the tree — HOLDS (child.rs:434
   `REAL_FORK: bool = true`; 9 `[tmp]` prints: child.rs ×4, exec_twin.rs ×2,
   main.rs ×3; the `sw to=` pair is the only rotation-scale one).

## 4. Hypotheses (cheapest confirmation first; cost per bit, cheapest first)

All tested by the SAME instrumented boot — cost ≈ 1 boot for the whole list.
- **H1 (fd-kind surprise):** the spinner's fd 1 is not `ChildOut`-answered the
  way the probe's single exec was — e.g. the fork child's inherited fd 1 takes
  a kind the arm mishandles, or the exec path left fd 1 unnamed (kind None →
  fallback → `emit` to console answers total — would NOT spin; so if the trace
  shows emit/child_write success answers AND the task still loops, look at H2).
- **H2 (answer delivery, not answer production):** the arm computes a nonzero
  answer but the resumed ring-3 frame sees rax=0 — a tick-switch frame/rax
  delivery defect (per-switch trap stacks landed in pf; the switch path is new
  code). Distinguisher: the trace's answer vs the loop. If the arm says 200
  and musl loops, it is delivery; fix in the switch path.
- **H3 (socket arm on a full window):** fd 1 is the connection socket and
  `send_slice` can answer Ok(0) despite the can_send pre-check (window closed
  between check and send). Fix shape: Ok(0)→EAGAIN mapping (then musl sets
  F_ERR — un-wedges but LOSES output — so the real fix would still be H1's
  ChildOut routing).
- **H4 (ChildOut with no drainer, silent-drop acceptable but loop elsewhere):**
  the fork child's out buffer has no reader (F3) — its output can never reach
  the bridge. Even if a side path stops the spin, F1 cannot pass until the
  fork child's stdout reaches the buffer the bridge drains. Candidate real
  fix: ChildOut becomes a first-class WRITE/WRITEV kind routed like the pipe
  arm (park-if-REAL / EAGAIN-else), AND the child's out buffer must be the
  shell's (drained) one — the fork child writes to the same session stdout
  its parent had, exactly what fork_copy intended.

## 5. The readable gate runner (decided BEFORE the first gate boot)

pf's UNKNOWNs were the runner losing the race with a ~30 s rotation. Fixes, in
order, before the first gate boot:
1. **Drop the rotation source** — remove `[tmp] sw to=`/`top` (exec_twin.rs:118)
   and `[tmp] enter` (exec_twin.rs:476) now; they are also on pf's "remove
   before final" list, marked "drop first". After this the console write rate
   is human-scale (wd ~2.6 s, sshd/kbuild lines) and the ~250 KB log covers
   minutes. Keep the wd line — it is the boot-liveness evidence.
2. The runner (run_fork.sh/run_pipes.sh) already slices `.old`+live from a
   logmark and greps `Exit status:` — with (1) done, that slice holds the
   verdict. Rule stays: **the verdict is the console `[SSH] Exit status` line,
   never the runner's UNKNOWN taken as green or red; a runner UNKNOWN means
   read the slice by hand.**
3. Boot mark discipline: judge faults only from this boot's start line.

## 6. The fix sequence (order, stop conditions)

- **Step 0 (no boot):** premise rows into RESULT-pg.md; kbuild exit 0; commit
  PLAN+RESULT. Stop: build green, both files committed.
- **Step 1 (boot 1, instrumented):** add the throttled `[tmp] wv` trace at the
  WRITE/WRITEV arms (slot, fd, kind byte, iovcnt, len, answer; print only
  answer<len or every 1024th hit per slot) on top of the sw-to removal.
  `REAL_FORK` stays ON. Run F1 once. Stop conditions: the trace names the
  spinner's (fd, kind, answer) → go to Step 2 with that fact; if F1 PASSES
  with the sw-to prints gone (rotation was also drowning the pump?), go
  straight to Step 3 in a fresh boot.
- **Step 2 (the fix, code shaped by Step 1):**
  - H2 → fix rax/frame delivery in the switch path (compare resumed-frame
    code with the entry path; add a host-testable unit if the crate allows).
  - H3 → map the socket arm's Ok(0) to EAGAIN (one line, mirrors amd64's
    short-write contract).
  - H1/H4 → make `ChildOut` a first-class WRITE/WRITEV kind routed like the
    pipe arm (park-if-REAL via may_park, EAGAIN for the boot task — the fix-B
    contract), and point the fork child's fd 1 at the shell's drained buffer
    so the bridge actually receives `cat`'s bytes. Composition: this is
    taoxichuan glue over akuma-pipes' existing check_set_* contract — no new
    crate, no amd64 extraction (the channel/arm pair lives in amd64+glue, not
    in a pullable crate; per the composition rule the decision crate is
    akuma-syscalls-glue, which pulls in the whole glue stack — out of budget;
    say so in RESULT-pg.md).
  - Stop: clippy+kbuild green → commit → boot 2 → F1 full lifecycle green
    (`[SSH] Exit status: 0`, client bytes `x\n2 /tmp/f\n` via od -c, zero
    faults after the boot mark, ticks advancing). If boot 2 yields no new
    evidence, re-plan per the budget rule (archive pass + hypotheses again).
- **Step 3 (gates, clean boot, ≤8 execs):** F1, P1-P5 (run_pipes.sh), PK1
  (`yes | head -n 20000 | wc -l` → exit 0, `20000\n`), PK2 last if the probe
  bakes cheaply (it is already in-tree per pf's premise 5 — check before
  baking). Stop: all green in one boot, verdicts from console lines.
- **Step 4 (final):** remove every remaining `[tmp]` print (the list in
  RESULT-pg.md), `REAL_FORK` stays `true` by default, host clippy on touched
  crates, kbuild, commit, push `litter`. Then ops restore and RESULT-pg.md
  final rows (hooks table: child-AS frames leak, sock stubs — carried honest;
  add the writev-loop row).

## 7. Budget map (~160 calls, ~6 boots)

Boots: 1 instrumented (Step 1), 1 fix verify (Step 2), 1 gate boot (Step 3),
+2 spare for re-plans, +1 final sanity if the strip touches code paths
(else strip-then-commit with no boot; the strip is prints only).
