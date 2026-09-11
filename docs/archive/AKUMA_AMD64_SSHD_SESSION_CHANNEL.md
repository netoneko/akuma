# amd64: an `ssh` session's stdio becomes a `ProcessChannel`

> 2026-09-11. `proposals/NEXT_AGENT_AMD64_SSHD_CHILD_CHANNEL.md`, all four
> steps. `^C` over `ssh` raises `SIGINT`, a session's fd 0/1/2 are real
> terminals, and the `ioctl` fake tty is retired.
>
> **Two shared crates changed on purpose**, `akuma-exec` and
> `akuma-syscalls-glue`; AArch64 was booted on every one of those changes,
> **307 PASSED / 0 FAILED** throughout.
>
> The piece landed in two passes and the first was wrong. §2's "half-measure"
> and §4 are that record — the wrong answer is kept because it is the shape of
> wrong answer this port keeps producing, and because the second pass only
> happened when someone asked "how did AArch64 get rid of it?" 

## 1. What it was

`sshd` opens `/proc/<pid>/fd/0` and writes the client's keystrokes into it. On
the AArch64 kernel that lands in the mounted `ProcFilesystem`, which calls
`akuma_exec::process::write_to_process_stdin` — the tty's front door, where the
INTR character is consumed and raised as `SIGINT` on the terminal's foreground
process group.

On amd64 it landed nowhere near there. `fd::sys_openat` intercepted exactly that
path, ahead of the VFS, and handed back the **write end of the child's stdin
pipe**. So the bytes reached the shell, and `^C` reached it as a `0x03` byte.

Three facts stood in the way and each was checked before anything was written:

| claimed | actual |
|---|---|
| `SPAWN_FLAG_PTY` is "accepted and currently ignored" (`sys_spawn`'s own doc) | **not received.** The dispatch arm was `301 => sys_spawn(a1..a5)` and the flags word is the sixth argument. `syscall_entry` was never the problem — it pushes `r9` precisely so a sixth argument survives (`futex`'s bitset is the other user). |
| closing the interception "waits for a working AArch64 verification loop" | that clause was spent. `cargo build --release` + `MEMORY=2048M INSTANCE=3 sh scripts/cargo_runner.sh …` reports 307/0 in about two minutes, and was run on every shared-crate change here. |
| the deferred work is "`/proc/<pid>/fd/0` + `delegate_pid`" | `delegate_pid` is `sys_reattach`'s — `box grab` — which moves a *target's* channel to a *grabber*. It has nothing to do with how `sshd` reaches its own child. A leftover phrase, now deleted from both places that carried it. |

## 2. What changed

**`sys_spawn` receives its flags.** One line in the dispatch arm, and the fact
`§1` corrects: the flags word was never dropped, it was never passed.

**A spawned child's stdio is one `ProcessChannel`**, registered the three ways
AArch64 registers it — `Process::channel`, `register_channel(task_slot, ..)` for
the exit status, and `register_child_channel(pid, .., ppid)` behind the
`ChildStdout(pid)` the caller gets back. Its fd table is
`SharedFdTable::with_stdio()`. `SPAWN_FLAG_PTY` decides only `set_terminal`,
which is what glue's `Stdin` arm selects cooked input on, what gates `/dev/tty`,
and what `write_to_process_stdin`'s ISIG branch reads.

**`foreground_pgid = pid`** on the child's fresh `TerminalState`, as the AArch64
spawn does unconditionally. Without it a `^C` would broadcast at `init`'s group:
`TerminalState::default()` seeds `1`. `kill_process_group` excludes the leader,
so this names the shell and reaches the job the shell is running.

**`register_exec_process` takes the channel as a parameter** rather than deriving
it. It derives "is this the console's process" from fd 0 being a
`FileDescriptor::Stdin` — which a session child's fd 0 now *is*, so the table can
no longer answer the question and a derived answer would hand a session the
serial line's channel.

**The `/proc/<pid>/fd/0` interception is gone.** `sshd`'s
`open("/proc/<pid>/fd/0", O_WRONLY)` goes through glue like every other path
under `/proc`, lands in `write_to_process_stdin`, and its ISIG branch consumes
the INTR byte. Nothing in `sshd` changed.

**`sys_write` learned whose channel it is looking at.** On this target
`FileDescriptor::Stdout` has always meant *the serial line*, so the console
preamble would have claimed every session child's fd 1.
`crate::console::is_console_channel` is an identity test against the one channel
`console::init` built; only a session's goes to glue.

### The half-measure this replaced, and why it is recorded

The first pass kept the pipes and gave the child a channel *beside* them, purely
so `is_terminal()` would be true and the ISIG branch would run. The bytes then
had to reach a pipe, so `write_to_process_stdin` gained an arm that resolved the
target's own fd 0 and wrote the `PipeRead` behind it through a new `pipe_write`
`ExecRuntime` hook.

It worked — `^C` was a signal, verified — and it was the wrong answer, for the
reason §4 gives: a channel added beside a pipe is a third implementation, not
the deletion of a second. Both the arm and the hook are **reverted**; the tree
has neither. What is worth keeping is the hazard it surfaced, because it would
bite any future version of that idea: an *empty* `data` (the lone `^C`
keystroke, filtered to nothing) must never be offered to a pipe, or a `^C` typed
at a shell whose stdin pipe has lost its reader comes back `EPIPE`, is reported
to `sshd` as a failed write of a byte the line discipline already consumed, and
is resent — a `SIGINT` every bridge tick instead of one per keystroke.

## 3. What went away

| | before | after |
|---|---|---|
| `sys_openat`'s `/proc/<pid>/fd/0` interception | 12 lines + 20 of doc | deleted — the module header's "what is left is one path" is now "nothing is left" |
| `fd::bind_stdio` | 37 lines + 24 of doc | deleted |
| `fd::alloc_pipe_fd(id, is_write, adopt_initial)` | 4 combinations, 2 live | `fd::install_child_stdout(pid)`, one line |
| `pipe::free` | the `waitpid`-knows-the-child-is-gone escape hatch | deleted; nothing bypasses the end counts any more |
| `usermode::cleanup_spawn_slot` | 3 failure paths unwound pipes in a stated order | deleted; the failure path drops an `Arc` |
| `usermode::child_of_stdout_pipe` + `fd::child_pipe_set_winsize` | a `for_each_process` scan to map a pipe id back to a child | glue's `ChildStdout(pid)` arm |
| `usermode::stdin_pipe_for_pid` | `pub`, 2 callers | deleted; `sys_close_child_stdin` is glue's arm |
| `Spawn` | `pid`, `exec_slot`, `stdin_pipe` (+ 3 more, earlier) | `pid`, `exec_slot` |
| the `ioctl` fake tty (`fd < FIRST_FILE_FD`) | claimed a pipe at fd 0 as a terminal | deleted — fd 0 **is** a terminal |

`dead_code = "deny"` found the tail of that list on its own: deleting
`bind_stdio` stranded `cleanup_spawn_slot`, which stranded `pipe::free`. That is
the deny lint doing the job the prompt predicted — "something should still be
deleted, and if nothing is, a channel was added beside the pipe instead of
replacing it."

## 4. The second implementation, and the framing that was wrong about it

The prompt's step 4 was "fd 0 becomes the channel, and the `ioctl` preamble's
first term can go". **It is done** — but the first write-up of *why it was hard*
was wrong, and that wrong answer is worth keeping because it is the shape of
wrong answer this port keeps producing.

It said: glue's `Stdin` read arm writes the line discipline's echo to the
channel's *stdout* FIFO, and on amd64 nothing drains that FIFO, so fd 0 cannot
move without fd 1/2, and that is a separate piece. Every clause is true and the
conclusion it invites — that something is *missing* here and has to be built —
is not. The right question was the one asked out loud: **how did AArch64 get rid
of it?**

It never had it. Checked, not remembered
(`crates/akuma-exec/src/process/spawn.rs` and glue's `sys_spawn`): an AArch64
spawned child has **one** `ProcessChannel` doing all three jobs —

* `process.channel = Some(ch)` — its I/O;
* `register_channel(tid, ch)` — its exit status, which `sys_exit` stamps;
* `register_child_channel(pid, ch, ppid)`, and the parent's returned descriptor
  is `FileDescriptor::ChildStdout(pid)`, resolving to that same channel.

Its fd table is `SharedFdTable::with_stdio()`, so fd 0/1/2 are
`Stdin`/`Stdout`/`Stderr`, all served from it. **There are no pipes.** The echo
and the program's own output land in one FIFO and `sshd` reads that FIFO. There
was no drain to find because there was no second sink.

What stood in amd64's way was not a missing mechanism. It was a **second,
parallel implementation of session stdio**, built on `crate::pipe` because
`sys_spawn` here predates all of it. The work was a deletion.

### What it cost to delete

| | before | after |
|---|---|---|
| `sys_spawn`'s stdio | 2 `pipe::alloc`s, `bind_stdio`, `cleanup_spawn_slot` on three failure paths | one `ProcessChannel`, `SharedFdTable::with_stdio()` |
| the caller's handle | `PipeRead` + an adopt-don't-clone rule (`AMD64_SPAWN_PIPE_LEAK.md`) | `ChildStdout(pid)` |
| `Spawn` row | `pid`, `exec_slot`, `stdin_pipe` | `pid`, `exec_slot` |
| the reap (`sys_waitpid` **and** `sweep_reaped_spawn_rows`) | `pipe::close_write` on a reference no descriptor named | nothing — the last `Arc` out drops it |
| `TIOCSWINSZ` at the child | `child_pipe_set_winsize` + `child_of_stdout_pipe` (a `for_each_process` scan) | glue's `ChildStdout(pid)` arm |
| `close_child_stdin` (326) | `pipe::close_write` off the row | glue's arm → `close_process_stdin` |
| `sys_ioctl`'s preamble | `fd < FIRST_FILE_FD \|\| console_end(fd).is_some() \|\| …` | `console_end(fd).is_some() \|\| …` — **the fake tty is retired** |
| `fd::bind_stdio`, `fd::alloc_pipe_fd`, `pipe::free` | 3 functions | deleted (`dead_code = "deny"` found the last of them) |

`sys_write` gained one thing rather than losing it, and it is the only place the
two kernels still differ: on this target `FileDescriptor::Stdout` has always meant
*the serial line*, so a session child's fd 1 would have been claimed by the
console preamble. `crate::console::is_console_channel` tells the console's
channel from a session's by identity — both report `is_terminal()`, and nothing
else distinguishes them — and only a session's goes to glue.

### The regression it caused, and the reader that was actually wrong

Giving a session child a `Process::channel` broke `^C` in a way no gate caught:
`sh -c 'sleep 60; echo X'` killed the `sleep` but the shell went on to print
`X`, where before it aborted the list.

`is_current_interrupted()` read `Process::channel` **first** and only fell back
to the per-thread registry — while `interrupt_thread`, which `deliver_signal`
calls for every target tid, writes the registry and *nothing else*. The two
agreed for as long as they were the same object: they are for an AArch64 spawn,
and they were on amd64 only because a session child had **no** `Process::channel`
at all, so the fallback always ran. A `fork` child inside a session now has the
session's channel in `Process::channel` and its own exit channel under its tid,
so the flag `deliver_signal` had just set was invisible.

The fix is in the reader, not the writer: the registry is the right source on
every shape — including `sys_reattach` (`box grab`), which points a target's
`Process::channel` at the *grabber's* while the flag still lands on the target's
own tid. `Process::channel` stays as the fallback for a caller with no tid
registration. This is a shared-crate change and AArch64 was booted on it.

`children.rs`'s long comment about why the *writer* must not touch
`Process::channel` stands unchanged. It was right; it simply never said the
reader had the same problem from the other end.

## 5. Cautions that survived the change

- `Process::channel` is `clone`d by `inherit_from`, so a session's channel is
  shared by the shell and everything it forks. That is right for a session — it
  is what makes the whole session one terminal — and it is exactly why a
  per-process interrupt flag must not live there
  (`AKUMA_AMD64_SIGNAL_DELIVERY.md` §5d, and §4 above for the reader half).
- **Two channels per process is still this target's shape for `fork`/`clone`
  children**, just no longer for spawns. A `sys_spawn` child's I/O channel *is*
  its exit channel *is* its parent's `ChildStdout` (§4); a `fork` child inherits
  the first and gets a fresh third from `spawn_child_thread_and_publish`. Know
  which one you are holding — `current_channel()` prefers `Process::channel` and
  falls back to the per-tid one, and they are different objects for a forked
  child.
- `ProcessChannel::new` defaults `is_terminal` to **true**, so "the process has a
  channel that says it is a terminal" proves nothing on its own. A non-pty
  `sys_spawn` explicitly `set_terminal(false)`, which is what keeps a piped
  child's stream from being cooked.
- **`is_interrupted()` consumes** (`swap(false)`), and glue's dispatch prologue
  calls it on every syscall. It no longer stamps the caller `Zombie(130)` (§5f of
  the signal doc), but it still returns `EINTR`.
- `sys_write`'s console preamble is now the one place that asks "whose channel is
  this?". If a third kind of channel ever appears here, that question needs a
  third answer — `is_console_channel` is an identity test, not a category.
- **A session's output is `ONLCR`-translated twice, and that is inherited rather
  than introduced.** Glue's `Stdout` arm runs `TerminalState::translate_output`,
  whose default `oflag` is `OPOST|ONLCR` on both kernels, and `sshd`'s
  `cook_output(pty = true)` adds its own `\n` → `\r\n` under a comment saying
  "the shell's stdout is a pipe, not a terminal, so no line discipline cooks it".
  That comment stopped being true on AArch64 whenever a spawn got a channel, and
  is now equally untrue here. A terminal renders `\r\r\n` as one newline, which
  is why nobody has noticed on either kernel. Left alone deliberately: removing
  either half is a behaviour change on AArch64, and this piece is about making
  amd64 do what AArch64 does, including this.

## 6. Verification

| gate | before | after |
|---|---|---|
| QEMU/TCG `SMP=4` | 665 / 0 | **666 / 0** |
| Firecracker/KVM `SMP=4` | 643 / 0 | **644 / 0** (before the interrupt-bit fix) |
| QEMU/TCG `SMP=1` | — | **656 / 0** |
| `amd64_ring3_check.py --smp 1 -n 20` | OK | **OK** — heap drift **+5 kB**, down from +37 |
| `amd64_mem_trials.py --local-only` | 10 / 10 | **10 / 10** |
| AArch64 boot suite (`MEMORY=2048M INSTANCE=3`) | 307 / 0 | **307 / 0** |
| host tests (`cargo test`) | 1375 / 0 | **1375 / 0** |
| clippy — amd64 `--release`, `--release --features no-tests`, and AArch64 | clean | **clean** |
| **bare metal** (HP box, `root=/dev/sda1`) | 665 / 0 | **666 / 0** — 663/3 before the interrupt-bit fix |

Bare metal is the gate that matters here: it is the **only** one that saw the
regression at all — everything above it was green through both the broken and
the fixed tree. 666 = the parent's 665 plus this piece's one new check, which is
the arithmetic that says nothing else moved.

The `+1` on the two boot suites is the new `spawn:` check that the child has an
I/O channel behind its stdio descriptors. That one is not decoration: glue's
`Stdin` arm falls back to `Process::read_stdin` when `current_channel()` is
`None`, and that buffer is never filled here — a reader would **park**, not
report EOF (`AKUMA_AMD64_CONSOLE_PROCESSCHANNEL.md` §3, where a comment claiming
the opposite cost a boot).

### Bare metal — and the regression only it could see

The box runs it: an `ssh` session works and `dmesg` reads back over one. The
suite, however, went **665 / 0 -> 663 / 3**, and the three failures are one
symptom:

```
fdprobe: every syscall claim held   got 0xffffffffffffffff want 0xfff
fdprobe: teardown leaks nothing     got 0x8c67e want 0x8c708
spawn:   teardown leaks nothing     got 0x8c708 want 0x8c67e
```

`0xffff…ffff` is `u64::MAX`, the sentinel meaning `EXIT_STATUS` was **never
written** — `fdprobe` hit its `spins < 10_000` bound rather than failing a
claim (which is also why no per-claim breakdown printed: that branch excludes
`u64::MAX`). The two leak numbers are complementary — the same 138 pages leave
during `fdprobe` and come back during `spawn` — which is deferred reclaim, not
a leak. Deterministic across two boots. `75041b73` on the same disk minutes
later: both `fdprobe` checks `[OK]`, windowed FAIL count **0**. So it was mine.

**The clock was the first hypothesis and it is ruled out.** The tick is
calibrated against the PIT and the boot prints the measurement:

```
lapic: calibrated vs PIT: 62361 counts per 10000us (99 MHz)
lapic: ticks per 50ms (expect 5) 5
```

Exactly 5, not merely inside `clock_rate_check`'s ±2x band — worth saying,
because that band would pass a clock running twice as fast, so the check's
`[OK]` is not evidence and the raw note is.

**The cause was §4's own fix, and the cost of it.** `is_current_interrupted`
runs in the syscall prologue of both kernels. Moving it off `Process::channel`
and onto the per-thread registry made it correct and made every syscall pay
`get_channel`, which is

```rust
with_irqs_disabled(|| PROCESS_CHANNELS.lock().get(&tid).cloned())
```

— an IRQ mask, a spinlock, a `BTreeMap` lookup and an `Arc` clone-and-drop,
where `has_pending_kill` right beside it is one array load. Isolated on the
metal rather than argued: `e414f41d` plus a one-line revert of **only** that
ordering, staged and booted, and all three failures disappeared.

**The third shape is the correct one.** The flag is now a per-thread bit
(`akuma_threading::THREAD_INTERRUPTED`) beside `PENDING_KILL`, scrubbed with the
rest of a slot's signal state so a recycled tid cannot inherit it.
`interrupt_thread` sets the bit *and* the channel flag — different readers, both
needed — and the three sites that used to set the channel alone
(`kill_process`, `kill_process_with_signal`, `check_itimers`) go through
`interrupt_thread` now, so no writer can raise half an interrupt.
`check_itimers` also loses a second write to `Process::channel` that existed
only because the reader looked in the wrong place, and which wrote a flag an
entire `fork` tree shares.

The prologue is now **cheaper than before any of this**: it resolves no identity
at all. The AArch64 boot suite said so before a human did — `akuma_get_version`
pins a `FastPath::Leaf` syscall at exactly 2 identity resolutions, both of them
`is_current_interrupted -> current_process_shared`, with a comment saying that
if the count ever moves "this trips and somebody decides on purpose". It tripped
at **0**, on the first boot after the fix. `LEAF_EXPECTED_RESOLVES` is 0 now.

### Two things this bare-metal round cost, both rig rather than kernel

- **A boot with no `root=` cannot run a command.** The RAM image's `/bin/sh` is
  a hard link to a busybox `mkdisk.sh` fetches with `curl`, behind
  `if [ -f "$BB" ]` — absent on that box, so every applet is silently missing
  and `sshd` answers `failed to spawn '/bin/sh' for exec`. The machine is up and
  authenticating; it simply has nothing to run, and no way to be told to reboot.
  Every recorded bare-metal baseline uses `root=/dev/sda1` for this reason.
- **`dmesg` wraps.** 50 `xhci` lines after boot are enough to push the suite's
  own tally out of the ring, so read it early or read `FAILED:` lines, which
  survive longer than the summary that follows them.

An earlier ring-3 run reported FAILED and it is worth recording that it did not
mean anything: its *pre-workload* sample came back unparsed — both `free`'s
columns and `/proc/meminfo`'s `Slab:` row read `None` — so the heap column had no
baseline to difference against, which the script counts as a failure by design
("this run proves nothing about the kernel heap"). Every probe in that same run
passed. It is the first ssh command after boot that is flaky, not the change.

### The observables, run by hand

No script covers any of this, so it was driven through a real `pty.fork()` client
against `INIT=/bin/sshd` (`ssh -tt`, so the client sends the `pty-req` that sets
`SPAWN_FLAG_PTY`), and **A/B'd against `75041b73` on the same rig** — the version
of each test that matters is the one where before and after differ.

| | `75041b73` | after |
|---|---|---|
| `[ -t 0 ] [ -t 1 ] [ -t 2 ]` in an `ssh` session | tty / tty / tty — the **fake tty**, `fd < FIRST_FILE_FD` | **tty / tty / tty** — real channel-backed descriptors, the preamble deleted (and the same on bare metal) |
| `^C` on `sh -c 'echo MARK; sleep 60; echo NOTREACHED'`, QEMU | list aborted | **same** (it printed `NOTREACHED` until the interrupt-flag fix — §4) |
| the shell survives `^C` | yes | **yes**, same pid |
| `busybox stty size` | `stty: standard input` | `stty: standard input` — **pre-existing**, now with `ENOTTY` attached where it used to report nothing |
| `busybox tty` | `not a tty` | `not a tty` — pre-existing; `ttyname` needs a `/proc/self/fd/0` readlink and procfs answers `NotFound` for a non-`File` fd |

### `^C` over `ssh` does not work on bare metal

**Measured, not inferred**, with a timing probe rather than by reading output:
send `echo GO; sleep 30; echo NOTREACHED`, wait for `GO`, send `0x03` three
seconds later, and time how long until the prompt returns.

| | seconds |
|---|---|
| QEMU/TCG `SMP=1` | **5.6** — the job was killed |
| QEMU/TCG `SMP=4` | **4.8** — killed |
| bare metal `SMP=4` | **30.2** — the sleep ran to completion |
| bare metal **single core** (`nosmp`) | **30.4** — ditto |

This is a gap in a **new** capability, not a regression: there was no `^C` over
`ssh` on this target at all before this piece, so there is no "before" to have
broken. It is also not any of the obvious things, each ruled out rather than
assumed:

- **Not the clock.** PIT-calibrated, `ticks per 50ms (expect 5) 5` (above).
- **Not cross-core signal delivery.** It fails identically at `nosmp`.
- **Not the pty path.** The console says `[SSH] Spawning shell: /bin/sh`, which
  is `handle_shell` -> `spawn_pty`, so `SPAWN_FLAG_PTY` is set.
- **Not a missing stdin fd.** No `bridge_process: couldn't open stdin` line, and
  typed commands run.
- **Not the channel.** `[ -t 0 ]` answers *tty* on the metal, and with the fake
  tty deleted the only thing that can say so is glue's ioctl seeing a `Stdin`
  descriptor whose `current_channel().is_terminal()` holds.

Two dead ends worth recording so they are not re-walked:

- **`/proc/<pid>/stat` cannot show you a process group here.** Fields 5 and 6
  are rendered as *the pid itself*, deliberately (`akuma-procfs`: "neither kernel
  has process groups in the sense the `getpgrp`/`getsid` syscalls give"). A
  reading of `pgrp` from there is not evidence about `Process::pgid`.
- **`busybox kill -TERM` on a backgrounded `sleep` fails on QEMU too**, so it
  does not discriminate anything. Run the control before believing a metal-only
  symptom; this one cost a hypothesis.

The next instrumentation is a `safe_print!` inside `write_to_process_stdin`'s
ISIG branch — does it fire on the metal, and what `foreground_pgid` does it
broadcast to? One boot answers it, and nothing short of that is worth guessing.

Two earlier claims of mine did **not** survive their A/B and are struck rather
than quietly dropped:

- "the INTR byte is consumed, not delivered" — still true, and still worth the
  probe that showed it: `busybox sh -c 'trap "" INT; busybox od -An -c'` fed
  `0x03`, `Z`, newline printed `Z \n 004 …` with **no `003`**. But it was true on
  `75041b73` too, from the same ISIG branch, so it is evidence the mechanism
  works and not evidence this piece changed it.
- "the kernel line discipline owns the echo now" — **unproven**. `stty -echo`
  suppresses typed echo on both trees, because `busybox ash`'s own line editor
  reads the termios `console_ioctl` stores and does its own echo. The A/B cannot
  tell the two apart, and no claim about echo should be made without one that
  can.

The serial-console control (`AKUMA_AMD64_CONSOLE_PROCESSCHANNEL.md` §6's delayed
feed, `INIT=/bin/busybox INITARGS=cat`) still prints each typed line **twice** —
the line discipline's echo and `cat`'s output — which is what it printed before.

## 7. The version regression this turned up

Unrelated to the above, found while it was in flight. `uname -r` reported
**`0.1.0`** on both kernels — `akuma-syscalls-glue`'s own package version.
`UTSNAME`'s release field was `env!("CARGO_PKG_VERSION")`, which expands to *the
crate the macro is written in*, so the moment `src/syscall/` became a crate
(2026-09-01) it silently stopped naming the kernel. Exactly the same class of
break as `AKUMA_GIT_SHA`, which that same move caught only because it failed to
compile.

There were **three** numbers, and the two that mattered were different on
purpose:

| where | value | for |
|---|---|---|
| root + `amd64/` `Cargo.toml` | `0.0.7` | the kernel package version — what `uname -r` was meant to report |
| `version::VERSION_TRIPLE` | `[0, 0, 8]` | a hand-maintained value for `akuma_get_version` |
| `banner::RELEASE` | `"0.1.0-amd64"` | nothing; simply wrong |

**The first fix was wrong and is worth recording.** It made `RELEASE` the source
and derived the triple from it — which removed the crate-version leak but
*coupled* two numbers whose own doc comment said they were independent on
purpose ("coupling them would make a routine `Cargo.toml` bump a silent ABI
change; `uname -r` is where the package version is reported"). The ABI value won
the merge, so `uname -r` moved to `0.0.8` while both manifests still said
`0.0.7`, and the question "which number does `uname -r` mean?" was still
unanswerable.

**The triple is deleted.** Nothing in userspace ever read it — `grep` across
`userspace/` finds no consumer of `akuma_get_version` at all; it is floor
control for a syscall-boundary benchmark
(`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`), not an ABI. Its only real effect was to
be a second version number that agreed with nothing. `pack`, `unpack` and the
compile-time round-trip assert went with it (dead once the triple was gone, and
`dead_code = "deny"` is a workspace lint), and `AKUMA_VERSION` is now the commit
alone — which identifies a build exactly, where a hand-maintained triple
identified nothing. The boot test's `triple_ok` rung is gone and its
`non_negative` rung relaxed from `> 0` to `>= 0`: the packed form's leading
`patch` byte guaranteed a positive value, and a build outside a git checkout
legitimately has no commit and answers `0`.

**And the release comes from `Cargo.toml`.** Not a literal — a literal that
happens to match the manifest is indistinguishable from one that has drifted,
which is the failure being closed. `akuma-syscalls-glue`'s `build.rs` reads the
workspace root's `[package] version` and emits `AKUMA_KERNEL_VERSION`, the same
mechanism already there for `AKUMA_GIT_SHA` and for the same reason: a crate
cannot see the version of the binary linking it, because `rustc-env` does not
propagate. It looks for the `[package]` header first rather than the file's
first `version =`, because the root manifest has `[workspace]` above it and
`[workspace.package]`/`[dependencies.*]` carry `version` keys of their own.

`banner::RELEASE` reads the same const and appends `-amd64`; its doc comment had
claimed it was "shared with `usermode::UTSNAME` so the banner and `uname(2)`
cannot disagree", and there is no `usermode::UTSNAME` — `uname` folded into glue
at C1 step 3 — so they did disagree, in a comment asserting they could not.

Verified in QEMU on **both** architectures, which is the only check that
distinguishes "reports the right number" from "reports a number that looks
right":

```
aarch64 (HVF)  Akuma akuma 0.0.7 2bf2c311-release-smp-shared aarch64 Linux
x86_64  (TCG)  Akuma akuma 0.0.7 2bf2c311-release-smp-shared x86_64 GNU/Linux
amd64 banner   Akuma/amd64 (x86_64 bring-up)  0.0.7-amd64
```

## Background

`AKUMA_AMD64_CONSOLE_PROCESSCHANNEL.md` (the worked example — a *producer* was
the missing half there and it was again here),
`AKUMA_AMD64_SIGNAL_DELIVERY.md` §5d and §5f,
`AKUMA_AMD64_SPAWN_ROW_STDIO.md` (why `Spawn::stdin_pipe` outlived its
siblings), `AMD64_SPAWN_PIPE_LEAK.md` (the adopt-vs-clone rule
`alloc_child_stdout_fd` still carries), `TTY_SHENANIGANS.md` round 3 (the two
interactive-shell architectures), `CTRL_C_SIGINT_DELIVERY.md` (the ISIG branch
this piece finally reaches), `SRC_SYSCALL_EXTRACTION.md` (§7's regression).
