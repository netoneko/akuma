# Stability Urgent Issues

Originally collected from the `01_verify_apk_bootstrap` acceptance run
(2026-05-28). The original report bundled two unrelated things under
"issue #1":

1. A kernel hang that silenced every heartbeat at once. SSH stopped
   responding during that hang, but only because *everything* stopped.
2. Pre-existing SSH usability problems (sluggish responses, missing
   `authorized_keys` log, no persisted host key, etc.) that were
   visible in the same log because the operator was trying to SSH in
   when the hang hit.

They are now split. Issue #1 below is the kernel hang only; SSH
usability and lifecycle items live under issue #2 and in the SSH
Deferred Follow-ups section.

---

## 1. Idle kernel deadlock — `RUNTIME` spinlock self-deadlock from IRQ handler (FIXED 2026-05-28)

### Root cause (confirmed via lldb attach)

Originally reported as "SSH connection triggers kernel deadlock"; SSH
turned out to be incidental. The real bug reproduces idle and is now
identified.

`akuma_exec::runtime::RUNTIME` (and `CONFIG`) were stored as
`spinning_top::Spinlock<Option<T>>` in
`crates/akuma-exec/src/runtime.rs`. The timer IRQ handler dispatches
into `check_preemption_watchdog()`
(`crates/akuma-exec/src/threading/mod.rs:640`), which calls
`runtime().uptime_us()` — i.e. acquires the `RUNTIME` spinlock from
IRQ context.

If any EL1 code held `RUNTIME` (which the watchdog also reads on every
timer tick) when the timer IRQ fired, the IRQ handler re-acquired the
same lock on the same CPU and span forever. Single-CPU kernel, no
other core to release. Both heartbeats stop together because the timer
IRQ itself is wedged.

### How it was caught

`scripts/run_multiple.sh` (8-way parallel boot with the hang
watchdog) flagged instance 7 at uptime 217s in
`logs/daif/hunt-20260528-232632/7.log`. Attached with `lldb -b
gdb-remote 1241`:

- **PC**: `akuma_exec::threading::check_preemption_watchdog+268`
- **Disassembly at PC**: LL/SC spinlock-acquire loop
  (`ldaxrb`/`stxrb`) on the byte at `0x404c0da0`
- **Backtrace**: `irq_handler → rust_irq_handler_with_sp →
  kernel_main::{closure#11} → check_preemption_watchdog` interrupting
  `run_async_main+2228`
- **`nm`** on the kernel ELF located
  `_RNvNtCs..._10akuma_exec7runtime7RUNTIME` exactly at
  `0x404c0da0` — identifying the lock byte

### Fix (landed)

In `crates/akuma-exec/src/runtime.rs`:

- Introduced `OnceCopy<T: Copy>` — a single-shot, lock-free cell
  backed by `AtomicBool` (Release-on-`set`, Acquire-on-`get`) + an
  `UnsafeCell<MaybeUninit<T>>`.
- Replaced `static RUNTIME: Spinlock<Option<ExecRuntime>>` and
  `static CONFIG: Spinlock<Option<ExecConfig>>` with
  `OnceCopy<ExecRuntime>` / `OnceCopy<ExecConfig>`.
- `register()`, `runtime()`, `config()` keep identical public
  signatures. `ExecRuntime` and `ExecConfig` were already
  `#[derive(Clone, Copy)]` and registered exactly once at boot, so the
  swap is semantically equivalent on the happy path — but reads are
  now lock-free and safe to call from IRQ context.

### Tests

- **Host (5 tests) — `runtime::once_copy_tests` in
  `crates/akuma-exec/src/runtime.rs`** — verifies single-shot
  semantics, get-before-set returns `None`, second `set` is ignored,
  many-reads stability, and 8-thread concurrent reader stress
  (10 000 iterations each).
- **Kernel — `test_runtime_is_lock_free_under_masked_irqs` in
  `src/daif_tests.rs`** — drives 10 000 paired `runtime()` /
  `config()` reads inside `with_irqs_disabled` (DAIF.I=1). Pre-fix,
  any contended boot would deadlock here; post-fix this completes
  uniformly and DAIF is restored cleanly. Runs every boot as part of
  the DAIF test suite.
- **Verification corpus — `logs/daif/hunt-20260528-234542/`** — 8
  parallel instances, 32 min wall / ~4h 19m cumulative kernel uptime,
  zero `[HANG?]` events. Prior repros hit at 31s (`logs/daif/1.log`)
  and 217s (instance 7 of `logs/daif/hunt-20260528-232632/`); the
  verification run is ~9× past the worst prior repro on 8× the
  surface area.

### Earlier observations (now subsumed)

- The original acceptance-log symptom (heartbeats stop together at
  uptime ~102s, QEMU still alive at 98% CPU) — same root cause as the
  217s lldb-confirmed hang above.
- The 22:49 idle reproducer at uptime ~31.7s in `logs/daif/1.log` —
  same root cause.
- The 04-idle-10min run that stalled at uptime ~98s and was earlier
  attributed to macOS host sleep — re-attributed to the same kernel
  bug; host sleep does not match other runs on the same host.
- `Connection reset by peer` errors on SSH attempts shortly before
  the hang fired in the original acceptance run were a symptom of
  the hang having already eaten the network thread, not a separate
  cause.

---

## 2. SSH Jitter and Connection Resilience

### Symptom

Interactive SSH sessions exhibit 800ms–1.8s input stagger, especially when
multiple threads are scheduled. Even after the `SSH_STAGGERING.md` and
`SSH_ECHO_LATENCY_FIX.md` fixes, a multi-await chain in `read_until_channel_data`
remains as a latency multiplier (each non-data SSH packet — window adjust,
keepalive — costs one full scheduler round-trip of ~100ms).

Separately, a single SSH session thread crashing or getting stuck has no recovery
path. The accept loop is the same thread as the session handler; if it panics, SSH
is gone for the lifetime of the VM.

### Known Root Causes

- Multi-await chain in `src/ssh/protocol.rs:91` — non-data packets each cost a
  full round-trip (~100ms × N iterations = seconds of visible lag).
- No watchdog or restart logic around the SSH accept loop.
- Embassy-net has no internal synchronization; concurrent session writes race on
  VirtIO ring state (see `SSH_THREADING_BUG.md`).

### Fix Process

1. **Batch TCP reads before SSH packet dispatch**: read all available TCP bytes
   in one call before entering the packet dispatch loop. This collapses N
   round-trips into 1 for bursts of non-data packets.

2. **Separate the accept loop from session threads**: the accept loop should never
   block on session I/O. Sessions should be separate spawned threads (or tasks)
   that can die without taking down the listener.

3. **Add a supervisory restart for the SSH server thread**: if the server thread
   exits for any reason, thread 0 should detect it (via a channel or `Arc<AtomicBool>`)
   and re-spawn it. Even a 1-second restart delay is better than losing SSH for
   the VM lifetime.

4. **Gate concurrent session writes behind a mutex** (short-term fix from
   `SSH_THREADING_BUG.md` Solution 1) until a proper message-queue architecture
   (Solution 2) can be implemented.

---

## 3. Acceptance Test Tooling: `ssh` Blocked in Crush

### Symptom

The `crush` bash tool blocks `ssh` for security reasons. Steps 5–6 of
`01_verify_apk_bootstrap.md` require SSH to run commands in the VM. The model
spent ~15 turns trying python workarounds, all resulting in connection resets
(due to issue 1 above), and the session was cancelled.

### Fix Process

1. **Allow `ssh` in crush for this project**: add `ssh` to the crush bash
   allowlist in `.claude/settings.json` or `.crush/commands`.

2. **Fix username in playbook**: the VM advertises
   `ssh -o StrictHostKeyChecking=no user@localhost -p 2222` but the playbook
   uses `root@localhost`. Update steps 5–6 to use `user@localhost`.

3. **Add retry logic to the SSH steps in the playbook**: SSH immediately after
   the server starts listening may still fail (first-connection key load, VirtIO
   warm-up). Add a retry loop:
   ```bash
   for i in $(seq 1 10); do
     ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 -p 2222 user@localhost true && break
     sleep 2
   done
   ```

---

## 4. No Kernel-Level Heartbeat Distinguishing Idle from Hung

### Symptom

The acceptance log shows 100+ seconds of identical heartbeat lines
(`[Thread0] loop=N`, `[Mem] Uptime X`) that are indistinguishable between
"kernel running fine, SSH server waiting" and "kernel scheduler looping but
SSH thread is dead". An observer (human or AI) cannot tell from the log alone
whether SSH is actually serviceable.

### Fix Process

1. **Add SSH server status to heartbeat**: log `[SSH] listening` / `[SSH] no
   active listener` in the periodic memory monitor output. One extra word per
   heartbeat line makes the state observable.

2. **Log SSH accept attempts and outcomes**: `[SSH Accept] new connection from
   127.0.0.1` and `[SSH Accept] handshake failed: <reason>` so the log captures
   what the server is doing with incoming connections.

---

## SSH Lifecycle & Visibility (Deferred Follow-ups)

These items came out of the 2026-05-28 investigation into issue #1.
They are real gaps but did NOT need to be fixed to close the immediate
stability work — the DAIF instrumentation and tests landed as the
load-bearing change. They remain useful follow-ups whenever SSH gets
attention next.

### a) Per-session thread recycling needs an explicit accounting check

`src/ssh/server.rs:66` spawns a per-session thread via
`spawn_system_thread_fn(run_session)` and immediately recreates the
listener socket. On close, the session thread calls `socket_close()` and
`mark_current_terminated()` (`src/ssh/server.rs:150-154`). Whether the
thread slot is actually returned to the global pool fast enough to
survive a burst of failed handshakes is unverified — add explicit
`SSH_SESSIONS_OPENED` / `SSH_SESSIONS_CLOSED` counters and assert
`opened - closed <= MAX_CONCURRENT` in the SSH heartbeat.

### b) smoltcp socket reaping verification

Closed sockets are queued in `pending_removal` in `smoltcp_net.rs` and
swept inside `poll()` (around lines 544-562). A second SSH connection
right after the first closes appears to work, but there is no test that
proves the socket handle is freshly allocated rather than the same
handle reused before the prior close fully drained. Add an assertion or
counter.

### c) Loud "no authorized_keys" log

`src/ssh/keys.rs:40` silently returns an empty `Vec` when
`/etc/sshd/authorized_keys` is missing. Add a one-shot
`[SSH Auth] WARNING: authorized_keys missing — all pubkey auth will be
denied` at the first call site so the failure is visible in the log
instead of presenting as a generic auth reject.

### d) Persistent host key

`src/ssh/protocol.rs:49` generates a fresh ephemeral host key each boot
and logs `"will load from fs on first connection"` — but no load-or-
generate code path actually exists. Replace with: on first connection,
attempt to read `/etc/sshd/host_key`; if absent, generate and persist.
Today every boot triggers `Host key verification failed` on the client.

### e) Accept-loop / session-thread isolation

The accept loop and the session handler currently share the same
thread. If a session panics or stalls, the listener dies with it for the
lifetime of the VM. Decouple: accept-only on one thread, sessions on
spawned children; have Thread 0 watchdog the listener and respawn if it
ever exits.

### f) SSH status in the heartbeat

Issue #4 in this document — add `[SSH] listening (N active)` (or
`no listener`) once per heartbeat tick so a future observer can tell
"idle but serviceable" from "looks idle but SSH is dead" without
attempting a connection.

### Status of the 2026-05-28 investigation

The DAIF / IRQ-mask work and the runs in the table below were the first
attempt at issue #1 and did *not* find the actual root cause — the
hang did not reproduce reliably enough on a single-instance boot to be
caught while a debugger was attached. The defensive work still landed
and is still useful:

- `IrqGuard` semantics tests pin the foundational invariant in place
  (5 tests under `src/daif_tests.rs`).
- The `YIELD_WITH_IRQS_MASKED` counter is in place and ready to fire
  if an IRQ-masked-yield bug ever does occur.
- `scripts/daif_analyze.sh` makes mechanical regression checks across
  saved runs.

The actual root cause (RUNTIME spinlock acquired from an IRQ handler)
was found later that day by running 8 boots in parallel via
`scripts/run_multiple.sh` with the log-stall watchdog and the QEMU
gdbstub already wired up on every instance — see the top of this
issue for the lldb evidence and the landed fix.

### 2026-05-28 verification runs

Moved from `logs/daif/INDEX.md`. Every boot during the DAIF / IRQ-mask
stability work is captured below. Analyze any run with
`./scripts/daif_analyze.sh logs/daif/<run-dir>`.

| Run | Label | TMR | Thread0 | SCHED | DAIF/5 | Verdict |
|-----|-------|-----|---------|-------|--------|---------|
| 20260528-180007 | 01a-baseline-idle (80s) | 12 | 18 | 0 | 0 (pre-tests) | OK |
| 20260528-180202 | 01b-ssh-trigger (single SSH @30s) | 17 | 26 | 0 | 0 (pre-tests) | OK |
| 20260528-183801 | 01c-ssh-stress (5x rapid SSH) | 26 | 45 | 0 | 0 (pre-tests) | OK |
| 20260528-191601 | 02-daif-tests (verify tests) | 12 | 20 | 0\* | 5/5 | OK |
| 20260528-195143 | 03-boot-1 (45s smoke) | 7 | 10 | 0 | 5/5 | OK |
| 20260528-195229 | 03-boot-2 | 6 | 10 | 0 | 5/5 | OK |
| 20260528-195315 | 03-boot-3 | 7 | 10 | 0 | 5/5 | OK |
| 20260528-195401 | 03-boot-4 | 7 | 10 | 0 | 5/5 | OK |
| 20260528-195447 | 03-boot-5 | 7 | 12 | 0 | 5/5 | OK |
| 20260528-195544 | 04-idle-10min (caffeinate -i only) | 13 | 19 | 0 | 5/5 | host-sleep confound |
| 20260528-200717 | 04b-idle-150s (probe 100s mark) | 21 | 28 | 0 | 5/5 | OK |
| 20260528-201006 | 04c-idle-10min-dis (full caffeinate) | 83 | 125 | 0 | 5/5 | OK |

\*sched=0 after excluding the deliberate test-induced warning in
`test_yield_now_detects_masked_yield`.

Operational notes:

- The 04-idle-10min run silently stalled at kernel uptime ~98s, ~500s
  before the script's SIGTERM. It was originally attributed to a macOS
  host-sleep confound, but **that explanation no longer holds**: the
  Claude Code harness kept the other runs (including 04b/04c) flowing
  on the same host without trouble, and the new 1.log reproducer
  (below) shows the same abrupt-stop shape after 31s — host sleep is
  ruled out. 04-idle-10min should be re-classified as a suspected
  kernel hang.
- The DAIF instrumentation in
  `crates/akuma-exec/src/threading/mod.rs`
  (`YIELD_WITH_IRQS_MASKED`) never triggered outside the deliberate
  test in any of the runs above — so whatever the bug is, it is not
  reached by the current `yield_now()` masked-IRQ probe.

### 2026-05-28 22:49 — new idle reproducer (`logs/daif/1.log`)

A subsequent run silently stalled at kernel uptime **~31.7s** while
fully idle (no SSH attempts, no client traffic). QEMU PID 59574 was
still alive at 98% CPU when discovered, confirming the VM is running
but the kernel scheduler / heartbeat threads are wedged. This matches
the *shape* of the original issue #1 symptom (all heartbeats silenced
together while QEMU keeps spinning), minus the SSH trigger:

- Last lines: `[TMR] t=2500 T=0 f=0`, `[Thread0] loop=500000`,
  `[Mem] Uptime 31742293`.
- No `[SCHED] WARNING: yield_now with IRQs masked` outside the
  expected deliberate test warning at boot.
- No `[Heartbeat]` after the single `Loop 708293` line at the very
  end of the visible run.
- No SSH connect attempts in the log — host sleep, SSH, and the
  authorized-keys path are all ruled out as triggers for this one.

This run was NOT started with `-s`/`-gdb`, so the still-alive QEMU
could not be attached to. A re-hunt with `scripts/run_multiple.sh`
(8-way parallel, `GDB=1` on every instance) caught the same hang on
instance 7 at uptime 217s and produced the lldb evidence used to
identify the `RUNTIME` spinlock root cause — see the new top of issue
#1.

## GDB attach playbook

Repro on a fresh boot with `GDB=1` (gdbstub on `localhost:1234+i`);
when the hang fires, attach lldb (or `aarch64-elf-gdb`) and inspect
PC / LR / DAIF / SP / disassembly. Apple's system `lldb` speaks the
gdb-remote protocol and works directly against the QEMU stub — no
need to install a separate toolchain.

1. Run the parallel hang hunt — it boots N kernels with `GDB=1`
   already wired, prints `[HANG?]` (with the exact attach command)
   the moment a log stops growing:
   ```bash
   scripts/run_multiple.sh 8     # 8-way; Ctrl-C to stop
   ```
   For single-instance hunts, `GDB=1 cargo run --release` exposes
   the stub on :1234.
2. When `[HANG?]` fires (or heartbeats stop), attach lldb in batch
   mode and dump the wedged state:
   ```bash
   ELF=target/aarch64-unknown-none/release/akuma
   PORT=$((1234 + INSTANCE))   # instance index from the [HANG?] line
   lldb -b \
     -o "target create --no-dependents $ELF" \
     -o "gdb-remote $PORT" \
     -o "thread list" \
     -o "register read pc lr sp cpsr" \
     -o "thread backtrace all" \
     -o "disassemble --pc --count 8" \
     -o "detach"
   ```
3. Decode the PC. If the disassembly shows `ldaxrb`/`stxrb` it's a
   spinlock acquire; map the address (e.g. `[x21, #0xda0]` →
   `x21 + 0xda0`) back to a symbol with
   `nm $ELF | grep -i <name>` or by sorting nm output by address.
4. Save the lldb transcript next to the matching kernel log under
   `logs/daif/`.

## Priority Order

| # | Issue | Severity | Effort |
|---|-------|----------|--------|
| 1 | ~~Idle kernel deadlock — RUNTIME spinlock self-deadlock from IRQ handler~~ — **FIXED 2026-05-28** | resolved | resolved |
| 1a | SSH connection → NETWORK spinlock deadlock (kept as separate watch item) | high | medium |
| 1b | NETWORK lock timeout / deadlock detector | high | low |
| 1c | `authorized_keys` missing → silent reject | high | low |
| 1d | Host key not persisted | medium | medium |
| 3 | Wrong username + `ssh` blocked in crush | **blocker** (acceptance) | low |
| 2a | Accept loop / session restart isolation | high | medium |
| 2b | Multi-await batch reads (jitter) | medium | medium |
| 2c | Concurrent session write lock | medium | low |
| 4 | SSH status in heartbeat | low | low |
