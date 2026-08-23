# `box grab` reattach: input silently swallowed by a stale cached channel

**Date:** 2026-08-23. **Status:** FIXED and verified on-device (isolated QEMU
instance, disk clone + private ports — the reporting user's own running VM was
never touched). **Kernel at capture:** devbox-smoltcp, `smp-shared` default
feature set, `SMP=4`.

**One line:** `sys_read`'s stdin loop (`src/syscall/fs.rs`) and
`sys_poll_input_event` (`src/syscall/term.rs`) each fetched the process's
`Arc<ProcessChannel>` **once**, before entering their blocking wait, and kept
reusing that same reference across every park/wake cycle of a single syscall
call. `sys_reattach` repoints `Process::channel` to a **new** `Arc` (the
grabbing session's). A process already parked in a blocking read when it got
grabbed never saw the swap — it kept waking up, checking the *old*, abandoned
channel, finding nothing, and parking again. Forever. The wake itself always
fired correctly; the reader was just looking in the wrong place afterward.

Current-state doc (read this first):
[`../reference/subsystems/syscalls/container.md`](../reference/subsystems/syscalls/container.md)
→ "reattach" section and the Stability note at the top — updated in place to
record the fix.

Observed-from report: the user ran `box grab 0 5` against a hung/orphaned
process and reported "does not actually grab anything, just blocks" and,
critically, "it needs to be interactive" — i.e. typed input never reached the
grabbed process, even though the syscall reported success.

---

## 1. What `box grab` is supposed to do

`box grab <name|id> [pid]` (`userspace/box/src/main.rs::cmd_grab`) calls
`libakuma::reattach(pid)` — the `sys_reattach` syscall
(`src/syscall/container.rs` → `akuma_exec::process::reattach_process_ext`,
`crates/akuma-exec/src/process/exec.rs:220-281`) — then sits in a `waitpid`
loop, mirroring `docker attach` semantics: hold the session open, showing the
target's output and forwarding the caller's input, until the target exits.

`reattach_process_ext` does two things after its box-hierarchy permission
check:

1. Sets `delegate_pid = Some(target_pid)` on the **caller** (so future writes
   to the caller's own stdin, e.g. via `/proc/<caller_pid>/fd/0`, get
   forwarded to the target — `write_to_process_stdin`,
   `crates/akuma-exec/src/process/mod.rs:441-472`).
2. Clones the caller's `Arc<ProcessChannel>` into the **target**'s
   `Process::channel` field (`exec.rs:267`), so the target's own reads/writes
   now flow through the same channel object the grabbing session's sshd
   process is bound to.

Both steps are correct and were not the bug. sshd (`userspace/sshd`) writes
client keystrokes into a session by opening `/proc/<pid>/fd/0` once at session
start and writing through it for the session's lifetime
(`userspace/sshd/src/protocol.rs:260-296`), which resolves through
`crate::vfs::proc.rs:276` → `write_to_process_stdin`. That call correctly
follows `delegate_pid` and lands the bytes in the target's (now-shared)
channel.

## 2. Reproducing it

Reattach's *output* direction is easy to get right by accident: every `write`
syscall re-resolves `current_channel()` fresh (`src/syscall/term.rs`'s
`write_to_process_channel`), so a process that's merely *printing* in a loop
(no blocking reads) streams correctly to a newly-grabbed session with no
special handling. An early repro using a `while true; echo tick; sleep 1; done`
background job looked like reattach worked — it only exercised the write path.

The bug only shows up for a process that was **already parked in a blocking
read at the moment of reattach** — which is every ordinary interactive
foreground process (a shell, `cat`, anything reading stdin), i.e. exactly the
scenario `box grab` exists for.

Minimal repro used to isolate this (private QEMU instance, two SSH sessions):

```
# session A
$ ssh ... cat            # foreground, blocks in read(stdin)

# session B (separate connection)
$ ssh ... box grab 0 <cat's pid>
$ type: hello-from-B      # never echoed back. cat does not exit either —
                          # it is not dead, not EOF'd, just never wakes
                          # usefully again.
```

`ps` from a third session confirmed the target stayed alive (not crashed, not
exited) throughout — ruling out a stdin-EOF/premature-exit explanation and
matching the archived symptom precisely: "target thread stays WAITING despite
an observed wake call."

## 3. Root-causing it: the wake fires, but the reader is stale

`SYSCALL_DEBUG_INFO_ENABLED` (a compile-time const, `src/config.rs`) plus
bumping `klog.rs`'s max log level to `Debug` and adding a few temporary
`log::info!`/`safe_print!` traces at three points — `write_to_process_stdin`'s
accept/wake, `ThreadWaker::wake`'s generation/state check
(`crates/akuma-exec/src/threading/mod.rs:3569`), and the stdin read loop's
register/park/wake points (`fs.rs`) — produced this sequence for the grabbed
`cat` (pid 6, tid 11) after typing `hello-from-B`:

```
[I] write_to_process_stdin pid=6 accepted=13
[I] pid=6 waker_present=true
[I] ThreadWaker::wake tid=11 is_current=true state=5   (5 == WAITING)
    pid=6 woke from park                                ← genuinely woke!
    read(stdin) pid=6 tid=11 registering waker           ← immediately parks again
    pid=6 parking
```

The wake mechanism (`ThreadWaker::wake`,
`crates/akuma-exec/src/threading/mod.rs:3561-3632` — the generation-checked
`WakeHandle`/`WOKEN_STATES`/CAS machinery) is doing exactly what it's supposed
to: `is_current=true`, state was `WAITING`, the CAS to `READY` succeeds, the
thread resumes. The bug is one level up: on resuming, the loop calls
`ch.read_stdin(&mut kernel_buf)` where `ch` is the **same `Arc<ProcessChannel>`
captured once, before the loop began** (`fs.rs`, originally around line 303):

```rust
// BEFORE — captured once, reused across every park/wake cycle of this syscall
let ch = if let Some(c) = akuma_exec::process::current_channel() { c } else {
    /* legacy no-channel fallback, returns immediately */
};
let mut kernel_buf = alloc::vec![0u8; count];
loop {
    let is_pipe = ch.is_stdin_closed() || !ch.is_terminal();
    ...
    let n = ch.read_stdin(&mut kernel_buf);   // ← always the OLD channel
    ...
    akuma_exec::threading::schedule_blocking(u64::MAX);
    ...
}
```

`write_to_process_stdin` writes into whatever `proc.channel` is **at write
time** — which, post-reattach, is the *new* channel (`accepted=13` proves the
bytes landed somewhere real). But the parked reader's local `ch` was fetched
**before** `box grab` ever ran, so it is still pointing at `cat`'s *original*
channel — the one belonging to session A, which nobody has written to since.
The wake correctly proves "there is new data somewhere for this process"; the
loop just checks the wrong `Arc`, finds it empty, and parks again — and will
keep doing so for as long as the process lives, since nothing in the loop ever
re-fetches `current_channel()`.

`sys_poll_input_event` (`src/syscall/term.rs`, the `timeout_us != 0` blocking
branch) has the structurally identical bug: `proc_channel` is fetched once
before its own `loop { ... schedule_blocking(deadline) }`.

This also explains why the *original* investigation
(`archive/KNOWN_ISSUES.md` #4, and the Stability-B note it fed into
`reference/subsystems/syscalls/container.md`) diagnosed this as "the wake
fails to take effect": from the outside — kernel logs showing a write and a
wake call, followed by no observable effect — that is indistinguishable from a
scheduler bug. The trace above is what tells them apart: the wake unquestionably
lands (state transitions, thread resumes), it's the *consumption* immediately
after that reads stale state.

## 4. The fix

Re-resolve `current_channel()` on every loop iteration instead of caching it
once outside the loop, in both call sites. The existence check that decides
between the channel-based path and the legacy no-channel fallback still only
needs to run once (a process that starts with no channel never gains one), but
the per-iteration body must not assume the channel it's holding is still
current.

`src/syscall/fs.rs` (`sys_read`, `Stdin` arm):

```rust
if akuma_exec::process::current_channel().is_none() {
    /* legacy no-channel fallback — unchanged, still runs once */
}

let mut kernel_buf = alloc::vec![0u8; count];
loop {
    // Re-resolve every iteration: `box grab`/`sys_reattach` can repoint this
    // process's channel to a new one while a read is already parked here.
    let ch = match akuma_exec::process::current_channel() {
        Some(c) => c,
        None => return 0,
    };
    let is_pipe = ch.is_stdin_closed() || !ch.is_terminal();
    ...
}
```

`src/syscall/term.rs` (`sys_poll_input_event`, blocking branch): the waker
registration stays once-before-the-loop (a deliberate, unrelated optimization
— see the existing comment there; `schedule_blocking`'s sticky `WOKEN_STATES`
already tolerates a wake landing against a still-registered waker), but the
channel fetch moves inside the loop body:

```rust
bytes_read = loop {
    let proc_channel = match akuma_exec::process::current_channel() {
        Some(c) => c,
        None => break 0,
    };
    let n = proc_channel.read_stdin(&mut kernel_buf);
    ...
};
```

Both fixes are minimal and localized — no change to the wake mechanism, the
reattach permission/delegation logic, or the waker-registration discipline,
all of which were already correct.

## 5. Verification

Built and booted on a **private, isolated QEMU instance** (APFS-cloned disk +
`e2fsck -fy`, `INSTANCE=1`-shifted ports via `scripts/cargo_runner.sh`,
disk/ELF fully separate from the reporting user's live VM on the default
ports — see `docs/README.md`'s pointer to the isolated-verification technique
for why: booting a second instance directly on the same disk/kernel would
corrupt the other session's disk or rebuild over its in-progress `src/`
edits).

Repro steps, pre-fix (confirms the bug): foreground `cat` in session A, `box
grab 0 <pid>` from session B, type input into B → nothing echoed, `cat`
confirmed still alive (not EOF'd/crashed) via a third session's `ps`.

Same steps, post-fix, run twice (once with debug tracing still in the tree,
once on a fully clean rebuild after reverting all temporary instrumentation):
input typed into the grab session (B) is echoed back correctly, repeatedly
(multiple separate lines across separate `write`/`read` round trips), and
session A receives nothing further — the reattach correctly and durably
steals the process's I/O.

`cargo build --release` (default features), `cargo build --release --features
devbox-smoltcp,no-tests`, and `cargo clippy --release --features
devbox-smoltcp,no-tests` are all clean. Host unit tests
(`cargo test --target <host-triple>`) pass unchanged (this fix touches only
`no_std` kernel syscall bodies, not host-testable crate logic).

## Background

- [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) #4 "`reattach` fails to wake target
  process" — the original report of this symptom. The *symptom* description
  was accurate; the attributed cause (a wake/scheduler bug) was not — see §3
  above for why the two are easy to conflate from kernel-log evidence alone.
- [`BOX_CONTAINERS.md`](BOX_CONTAINERS.md) §7.1 "Native Reattachment" — the
  original design intent for `sys_reattach` (kernel-mediated I/O delegation,
  replacing `box`'s old manual byte-proxy), unaffected by this fix.
- [`../reference/subsystems/syscalls/container.md`](../reference/subsystems/syscalls/container.md)
  — current-state doc, updated in place: Stability note, `reattach` section,
  and the Background pointer to this doc's #4 entry all now record FIXED
  2026-08-23.
- [`TERM_POLL_INPUT_PREEMPTION_FIX.md`](TERM_POLL_INPUT_PREEMPTION_FIX.md) —
  an unrelated but structurally similar prior investigation of the *same two
  call sites* (`sys_poll_input_event` / `sys_read`'s Stdin arm): a locking
  hazard in the same (A)(B)(C)(D) register/read/park/clear loop shape. That
  fix (per-attempt lock guards, 2026-08-11) and this one are independent —
  neither caused nor fixed the other — but anyone touching this loop again
  should read both.
- [`crates/akuma-exec/src/process/exec.rs`](../../crates/akuma-exec/src/process/exec.rs)
  → `reattach_process_ext` (`:220`) — unchanged by this fix; confirmed correct
  throughout the investigation.
- [`crates/akuma-exec/src/threading/mod.rs`](../../crates/akuma-exec/src/threading/mod.rs)
  → `ThreadWaker::wake` (`:3561`) — unchanged by this fix; confirmed correct
  (generation check, CAS, SGI trigger all behave as designed) via the debug
  trace in §3.
