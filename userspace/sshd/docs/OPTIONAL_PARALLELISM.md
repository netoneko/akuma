# Optional parallelism for sshd: threads vs. processes

A design note, not a landed feature. Prompted by two separate observations
this session: `sshd` used exactly one of four cores in an `SMP=4` boot
(`PSTATS PID .../bin/sshd` never showed multi-core attribution), and the
crash in `PROTOCOL_UNDER_LOAD.md` showed that today's single-process
cooperative model means one connection's bug takes every other connection
down with it. Two different problems; parallelism in the "thread-per-session"
sense only addresses the first.

## Two different things people mean by "parallelism" here

1. **Spreading CPU work across cores** — so `SMP=4` actually uses more than
   one core for crypto/protocol work under concurrent load.
2. **Fault isolation** — one session's crash/bug not taking every other
   session down.

They need different mechanisms, and only one of them is "add threads."

## Thread-per-session: gets you (1), not (2)

`LIMITATIONS.md` §2 already notes `libakuma` exposes no `sys_thread_create` —
`spawn` creates a whole separate process, not a thread sharing the address
space. Real threads (e.g. via the kernel's existing `CLONE_VM`/`CLONE_FILES`
path that musl `pthread_create` already uses for other binaries — see
`docs/reference/abi/musl.md` "Shared fd tables") would need a new libakuma
wrapper issuing that `clone` directly; the kernel-side ABI already exists for
musl binaries, so this is plausibly userspace-only (`libakuma`) work, not a
kernel change — unconfirmed, would need verifying libakuma can issue a raw
`clone(CLONE_VM|...)` without going through musl's own `pthread_create`.

But threads **do not fix fault isolation**: `panic = "abort"` terminates the
whole *process* regardless of which thread panicked (this is also true on
real Linux — `abort()`/`SIGABRT` is process-wide, not thread-scoped). A
thread-per-session `sshd` would still have the exact failure mode in
`PROTOCOL_UNDER_LOAD.md` — one session's malformed packet still kills every
sibling thread's session, just now with the added complexity of shared-memory
data races between sessions that don't exist in today's single-threaded
cooperative model (every `Spinlock`-guarded static — `HOST_KEY`,
`CACHED_CONFIG` — goes from "never actually contended" to "actually
contended," and the input/crypto buffers per session need to actually be
`Send`/isolated correctly rather than just conventionally-single-threaded).

Worth doing for core utilization under CPU-heavy concurrent handshake bursts.
Not worth doing *for* the crash-isolation problem — don't conflate the two
when scoping this.

## Process-per-session: gets you (2), not for free

Spawning a whole process per successful (post-auth) session — extending the
same `spawn()`/`spawn_pty()` pattern `run_shell_session`/`run_exec_session`
already use for the login shell — gives real fault isolation: a panic in
session B's process only aborts session B's process. Session A, running in
its own process, is unaffected. This is the architecturally correct fix for
`PROTOCOL_UNDER_LOAD.md`'s blast-radius finding, not threads.

The blocker: this requires handing the already-`accept()`ed client socket fd
off to the freshly spawned process, and **that primitive does not exist**.
Surveyed in full in `docs/MISSING_SOCKET_MACHINERY.md` — no fd-inheritance
argument on `sys_spawn`, no `SCM_RIGHTS` in `sendmsg`/`recvmsg`, and
`/proc/<pid>/fd/<n>` only ever exposes fd 0/1 (a narrow stdin-injection path
for the existing shell-bridging use, not a general handoff). Any of that
doc's three options is new *kernel* work, not something `sshd` can build
around on its own.

An alternative that avoids needing fd-passing at all: don't hand off an
*accepted* connection — instead have `main()`'s accept loop itself spawn one
process per *listening-socket* accept, i.e. multiple sibling processes race
`accept()` on the same *listening* fd (the classic prefork-server model:
each spawned child inherits nothing special, it just needs to see the
listening socket, which — same gap — still needs some fd to reach that
listening socket in the first place unless each sibling does its own
`socket()`+`bind()`+`listen()` with `SO_REUSEPORT`-equivalent kernel support,
which isn't confirmed to exist here either). Not pursued further in this
note; flagging it as the one option that sidesteps `MISSING_SOCKET_MACHINERY.md`
entirely, in exchange for needing its own kernel-side confirmation
(multi-bind on one port).

## Recommendation, if this gets picked up

- If the goal is core utilization: thread-per-session, once libakuma exposes
  real thread creation. Default it **off** behind a build feature or runtime
  config flag — it's a real behavior change (shared mutable state where there
  was none before), not a pure win, and should be opt-in until proven stable
  under this codebase's existing SMP work (see `docs/reference/subsystems/smp-shared.md`
  for the kind of races real shared-kernel SMP work here has turned up
  before).
- If the goal is fault isolation: process-per-session, but only after
  `docs/MISSING_SOCKET_MACHINERY.md`'s gap is closed on the kernel side. Don't
  attempt a userspace workaround for that gap — the options that don't touch
  the kernel (e.g. re-deriving crypto session state in the child from scratch,
  or some fd-less handoff hack) are more fragile than just building the real
  primitive.
- Simplest actual mitigation available **today**, no new infrastructure: the
  one-line bounds check already applied in `PROTOCOL_UNDER_LOAD.md` closes the
  specific crash found this session. It doesn't generalize to "no single bug
  can ever take down every session," but it removes the one concrete way to
  trigger that outcome that's currently known.

## Background

- The two problems this doc separates: `PROTOCOL_UNDER_LOAD.md` (fault
  isolation / the crash) and this session's `SMP=4` core-utilization
  observation (performance).
- fd-passing survey: `docs/MISSING_SOCKET_MACHINERY.md`.
- Existing threading/concurrency limitations: `LIMITATIONS.md` §1-§2.
- Kernel-side SMP hazard history, for context on why "just add threads"
  carries real risk in this codebase specifically: `docs/reference/subsystems/smp-shared.md`.
