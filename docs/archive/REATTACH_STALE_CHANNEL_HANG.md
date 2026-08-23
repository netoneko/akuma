# `box grab` reattach: stale cached channel, then detach and exit-detection

**Date:** 2026-08-23. **Status:** FIXED and verified on-device (isolated QEMU
instances, disk clones + private ports — no live VM the reporting user was
using was ever touched). **Kernel at capture:** devbox-smoltcp, `smp-shared`
default feature set, `SMP=4`.

Same day, same feature, in the order things were found: §1-§5 is the original
bug (input silently swallowed by a stale cached channel); §6 is what surfaced
immediately afterward once `box grab` actually worked well enough to use for
real — no way to detach a previous session (`-d`, like `screen -d`) and no way
for `box grab` to notice its target had exited; §7 is what came out of a
"just curious" question about repaint signals — `SIGWINCH` propagation on
attach, turning §6's raw force-stop detach into a real disposition-aware kill
with a terminal reset first (§7.1-§7.2), discovering `tmux` itself cannot run
on Akuma at all for an unrelated, pre-existing reason (§7.3), and verifying
the whole mechanism instead against a real raw-mode full-screen app,
`busybox`'s `vi` (§7.4).

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

## 6. Follow-on (2026-08-23): detach-and-take-over, and `box grab` never noticing the target exit

Two more gaps surfaced immediately once the channel-staleness fix above made
`box grab` usable for real interactive sessions.

### 6.1 No protection against stealing a live session (`-d`, like `screen -d`)

Nothing before this stopped a second `box grab` on the same pid from silently
stealing the channel out from under a first, still-active grab — no warning,
no error, and (worse, see §6.2) the first grabber had no way to notice and
would just sit there forever, uselessly, with no channel. `screen -r` refuses
this by default and only proceeds with an explicit `-d`; `box grab` had no
such concept at all.

Added `Process::grabbed_by: Option<Pid>` (`crates/akuma-exec/src/process/mod.rs`),
set by a successful reattach, trusted only while that pid is still alive (a
grabber that already exited leaves a harmless stale value — self-correcting
on the next check, no exit-path cleanup needed). `reattach_process_ext` gained
a `force: bool` parameter:

- Without it, reattaching to an already-(live-)held target now fails with a
  distinct error (`"Already attached"`), mapped to a new `EBUSY` errno
  (`crates/akuma-primitives/src/errno.rs` — the one errno table; `EBUSY = 16`
  had never been needed anywhere in the tree before, unlike most of that
  table's entries, which consolidated pre-existing scattered definitions).
- With it (`force = true`), the previous holder is sent `SIGTERM`
  (`akuma_exec::process::kill_process_with_signal`) before the reattach
  proceeds, so its own wait loop actually exits instead of spinning forever
  against an abandoned channel — the detach has to be *observable* by the
  detachee, which is exactly §6.2's fix on the read side.

`sys_reattach`'s signature changed from `(pid)` to `(pid, force)` — every
caller updated: `libakuma::reattach(pid, force)`, and in `userspace/box`
(`main.rs`'s `cmd_open`/`cmd_use`, `run.rs`'s `box run`) and
`userspace/paws` (`execute_external_reattach`), all pass `force = false` —
each of those reattaches a process it *just* spawned itself, which cannot
already have a holder. `box grab` exposes the flag as `-d`/`--detach` and
prints a `screen`-style refusal (`PID N is already attached. Use -d to detach
it and take over.`) when it isn't passed and the target is held.

### 6.2 `box grab` never exiting when its target does

Separately reported: even with the channel fix, `box grab` would sit forever
after the grabbed process exited on its own (no second grab involved at all).
Root cause: `cmd_grab`'s loop polls `waitpid(pid_to_grab)`
(`userspace/box/src/main.rs`), but `reattach` does not reparent anything — the
grabbed pid is essentially never `box grab`'s own child. `wait4`/`waitpid`
semantics (here, same as Linux) only report a *child's* exit; on a non-child
pid the kernel syscall returns an error indistinguishable, at the
`libakuma::waitpid()` wrapper level, from "still running" (`waitpid_status`
maps both `result == 0` and `result < 0` to `None`). So the loop could never
tell "target is gone" from "target is fine," regardless of anything else —
this was true before the channel-staleness fix too, just impossible to notice
until reattach actually worked well enough to sit and watch a real session.

Fixed by falling back to a plain liveness probe when `waitpid` reports
nothing: `libakuma::kill_signal(pid_to_grab, 0)` (a `kill(2)`-style existence
check that delivers nothing) returning non-zero means the target is gone, at
which point `box grab` prints `process exited` and exits 0. `waitpid` is
tried first and still wins when the target genuinely is `box grab`'s own
child (not the common case, but not excluded either) — the probe is purely a
fallback for the case `waitpid` can never resolve on its own.

### 6.3 Verification

Same isolated-QEMU technique as §5 (fresh disk clone + `e2fsck -fy` +
`INSTANCE=1`-shifted ports; the userspace binaries were re-staged into
`bootstrap/bin/` by `userspace/build.sh --box-only`/`--paws-only` and the disk
repopulated with `--bin-only` before boot — no live VM was touched). Three
scenarios, each confirmed on-device:

1. Session A: foreground `cat`. Session B: `box grab 0 <pid>` (no `-d`,
   nothing held yet) — succeeds, input/output round-trips.
2. Session C: `box grab 0 <pid>` **without** `-d` while B still holds it —
   refused, prints the `already attached` message, exit code 1; B is
   untouched and keeps working.
3. Session C: `box grab -d 0 <pid>` — B is observably killed (its ssh client
   exits with a nonzero status and its connection closes) and C takes over
   the same live `cat`, confirmed by typing into C and seeing it echoed.
4. A process that exits on its own (`sh -c 'sleep 3; echo done'`) while
   grabbed: its final output (`done`) streams through as expected, and `box
   grab` prints `process exited` and exits 0 immediately after, rather than
   hanging.

`cargo build`/`cargo clippy` (default and `devbox-smoltcp,no-tests` feature
sets) and `cargo test --target <host-triple>` (which now also exercises the
new `EBUSY` table entry via `errno::tests::every_value_is_the_linux_number`)
are all clean.

## 7. Follow-on (2026-08-23, same day): repaint on attach, graceful death on detach

Two more pieces, prompted by a "just curious" question about whether there's
a standard signal for "please repaint" — there is (`SIGWINCH`) — which led
straight back to two gaps in §6's `-d`/detach work.

### 7.1 `SIGWINCH` and terminal-size propagation on every reattach

`box grab`ing a full-screen app (anything ncurses-based) left it looking
frozen or misdrawn until something else prompted a redraw, because nothing
told it its terminal had effectively changed. `screen`/`tmux` handle this on
every attach by pushing the new terminal's size onto the session and sending
`SIGWINCH` — "your window changed, requery (`TIOCGWINSZ`) and redraw." Added
the same thing to `sys_reattach` (`src/syscall/container.rs`), unconditionally
on every successful reattach (not just `-d`): copy the caller's
`term_width`/`term_height` onto the target's `TerminalState`, then
`SIGWINCH` (28) the target through the normal disposition-aware `sys_kill`
path. Confirmed safe to send unconditionally, including at a target with no
handler installed (a plain `cat`): `SIGWINCH`'s POSIX default action is
Ignore, and 28 is absent from `crate::syscall::signal::signal_is_fatal_default`
— so an unhandled `SIGWINCH` is silently dropped, never fatal.

### 7.2 Detaching a previous holder was a raw force-stop, not a real kill

§6.1's original `-d` implementation detached the previous holder from
*inside* `reattach_process_ext` (`crates/akuma-exec`, the crate boundary)
using `kill_process_with_signal` — a crate-internal hard-stop that manipulates
process state directly (marks it a zombie, sets the exit code, notifies the
parent) without going through the normal disposition-aware signal path or a
real `exit_group` teardown. Reported as insufficiently "graceful," and
separately: nothing reset the displaced session's terminal, so whatever the
grabbed app had left it in (raw mode, alternate screen, hidden cursor) would
still be sitting there once the connection dropped — that state lives in the
*client's* terminal emulator, which nothing server-side can reach once the
connection is gone, so it has to be fixed by sending a reset **before** the
connection closes, not after.

Fixed by moving the actual detach out of the crate and up to the syscall
boundary, which is where the real, disposition-aware signal path already
lives (`src/syscall/proc.rs::sys_kill` — the same one a plain `kill(2)`
uses). `reattach_process_ext`'s job shrank to *deciding* whether someone
needs detaching: it now returns `Ok(Some(previous_holder))` instead of acting.
`sys_reattach` does the detach itself, in order: write a soft terminal-reset
escape sequence (exit alt-screen `?1049l`, show cursor `?25h`, clear
attributes `0m`, DECSTR soft reset `!p`, newline) into the previous holder's
own channel — while the connection is still up — then `sys_kill(previous,
SIGTERM)`, which runs the previous holder through its own normal `exit_group`
cleanup rather than a raw state flip.

Verified on the same isolated-QEMU setup as §6.3: grabbing a `cat`, having a
second session `-d`-grab it, and confirming the first session's final bytes
before disconnect are exactly the reset escape sequence, followed by the
connection closing with a nonzero exit — not a bare "connection reset,"
a real, escape-sequence-terminated goodbye.

### 7.3 tmux itself is blocked — a real, pre-existing, unrelated gap

**`tmux` cannot run on Akuma at all, independent of anything in this doc.**
Tried it directly: `box pull alpine` + `box run ... apk add tmux` worked fine
(network and apk-inside-a-container are both functional), and `tmux 3.7c`
installed and executed — up to the point where it tried to create its
client/server rendezvous socket:

```
error connecting to /tmp/tmux-0/default (Address family not supported by protocol)
```

`tmux`'s entire architecture is a client and a server talking over a **named,
filesystem-bound `AF_UNIX` socket** (`bind()` a path, `listen()`, a separate
process `connect()`s to it later). `sys_socket()` (`src/syscall/net.rs:115`)
hard-rejects any domain other than `AF_INET`; `bind`/`listen`/`connect` are
all gated on the `smoltcp` (IP) stack. The *only* `AF_UNIX` support that
exists is `socketpair()` — an anonymous, already-connected pair for one
process's own IPC (what `rustc` uses to talk to its linker child) — which
cannot serve a rendezvous between two independently-launched processes. This
is a pre-existing, already-documented limitation
([`../reference/subsystems/syscalls/net.md`](../reference/subsystems/syscalls/net.md)
— "AF_UNIX socketpair exclusion"), not something introduced or exposed by
`box grab`. Fixing it would mean building a real bindable/connectable
`AF_UNIX` socket subsystem — a separate, much larger project, out of scope
here.

(One rough edge hit along the way, unrelated to any of this: a `box run`
whose spawn fails — e.g. `tmux`'s own strict check that its runtime dir
`/tmp/tmux-$UID` be mode `0700`, which the container's default `/tmp` isn't —
leaves the box registered with no way to `box close` it under its name until
whatever call path unregisters an emptied box runs. Not investigated further;
noted here in case it resurfaces.)

### 7.4 Verified instead against a real raw-mode full-screen app (`vi`)

Since `tmux` itself is off the table, repeated the same on-device check
(isolated QEMU instance, per the established technique) against `busybox`'s
`vi` — a genuine full-screen, raw-terminal-mode editor, not a synthetic stand-in
like `cat`. Three things confirmed, each against the live process, not
inferred:

1. **Repaint on attach, unprompted.** Grabbing a running `vi` and then simply
   typing into the new session showed a full clear-and-redraw (`\e[H\e[J` +
   every tilde line + status line redrawn) fire on its own — `SIGWINCH`
   reaching a real app and producing exactly the repaint §7.1 was built for,
   not merely "delivered without crashing."
2. **Raw-mode keystrokes forward correctly.** Entered insert mode (`i`),
   typed text, `Esc` — `vi`'s buffer and status line (`[Modified]`) updated
   correctly after being relayed through the reattached channel, confirming
   input forwarding survives reattach for a process in raw/noncanonical mode
   (echo off, no line buffering), not just a line-buffered tool like `cat`.
3. **`-d` detach against a real app, twice over.** Grabbed `vi` from session
   B, then `-d`-grabbed it from session C. B died observably (nonzero exit,
   connection closed) with its exact final bytes being §7.2's terminal-reset
   sequence (`\e[?1049l\e[?25h\e[0m\e[!p`). C then continued editing the
   *same live* `vi` process correctly (typed `from-C`, saw it inserted,
   modified flag set). `vi` never crashed or visibly corrupted its screen
   state across any of this.

This doesn't exercise `tmux`'s own client/server reattach protocol (§7.3
explains why that's currently impossible to test at all), but it does confirm
the mechanism this whole doc is about — reattach, `SIGWINCH`-triggered
repaint, and detach-with-terminal-reset — against real, unmodified
third-party software in raw terminal mode, not only the synthetic `cat`/`sh`
targets used in §5-§7.2.

## Background

- [`../reference/subsystems/syscalls/net.md`](../reference/subsystems/syscalls/net.md)
  — "AF_UNIX socketpair exclusion": why `tmux` can't run at all (§7.3).
  `sys_socket` only accepts `AF_INET`; the only `AF_UNIX` path is an
  anonymous `socketpair()`, with no `bind`/`listen`/`connect` for a named
  socket two independent processes could rendezvous on.
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
  → `reattach_process_ext` (`:229`) — unchanged by §1-§5; gained the
  `force`/`grabbed_by` detach *decision* in §6.1, then (§7.2) had the actual
  detach action moved back out to `sys_reattach`, since only the syscall
  boundary can reach the disposition-aware signal path.
- [`src/syscall/container.rs`](../../src/syscall/container.rs) → `sys_reattach`
  — the SIGWINCH/winsize-propagation (§7.1) and terminal-reset-then-`sys_kill`
  detach (§7.2) both live here now, reusing `src/syscall/proc.rs::sys_kill`
  directly (`pub(super)`, reachable from any `crate::syscall` submodule) —
  the same disposition-aware path `kill(2)` uses, not reimplemented.
- [`crates/akuma-exec/src/threading/mod.rs`](../../crates/akuma-exec/src/threading/mod.rs)
  → `ThreadWaker::wake` (`:3561`) — unchanged by this fix; confirmed correct
  (generation check, CAS, SGI trigger all behave as designed) via the debug
  trace in §3.
- [`crates/akuma-primitives/src/errno.rs`](../../crates/akuma-primitives/src/errno.rs)
  — the one errno table (`docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md`
  §5.7); gained `EBUSY = 16` for §6.1's "already attached" case, pinned in
  `every_value_is_the_linux_number`.
- [`userspace/box/README.md`](../../userspace/box/README.md) → `box grab`
  options — the `-d`/`--detach` flag and the exit-on-target-exit behavior,
  user-facing.
